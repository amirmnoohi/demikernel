use futures::task::AtomicWaker;
use std::{
    alloc::{
        AllocRef,
        Global,
        Layout,
    },
    mem,
    ops::Deref,
    ptr::{
        self,
        NonNull,
    },
    sync::{
        atomic::{
            self,
            AtomicU64,
            Ordering,
        },
        Arc,
    },
    task::{
        RawWaker,
        RawWakerVTable,
    },
};

pub const WAKER_PAGE_SIZE: usize = 64;

#[repr(align(64))]
pub struct WakerPage {
    refcount: AtomicU64,
    notified: AtomicU64,
    completed: AtomicU64,
    dropped: AtomicU64,
    waker: Arc<AtomicWaker>,
    _unused: [u8; 24],
}

impl WakerPage {
    pub fn new(waker: Arc<AtomicWaker>) -> WakerPageRef {
        let layout = Layout::new::<WakerPage>();
        assert_eq!(layout.align(), 64);
        let mut ptr: NonNull<WakerPage> = Global.alloc(layout).expect("Allocation failed").cast();
        unsafe {
            let page = ptr.as_mut();
            page.refcount.store(1, Ordering::SeqCst);
            page.notified.store(0, Ordering::SeqCst);
            page.completed.store(0, Ordering::SeqCst);
            page.dropped.store(0, Ordering::SeqCst);
            ptr::write(&mut page.waker as *mut _, waker);
        }
        WakerPageRef(ptr)
    }

    pub fn notify(&self, ix: usize) {
        debug_assert!(ix < 64);
        self.notified.fetch_or(1 << ix, Ordering::SeqCst);
        self.waker.wake();
    }

    pub fn take_notified(&self) -> u64 {
        // Unset all ready bits, since spurious notifications for completed futures would lead
        // us to poll them after completion.
        let mut notified = self.notified.swap(0, Ordering::SeqCst);
        notified &= !self.completed.load(Ordering::SeqCst);
        notified &= !self.dropped.load(Ordering::SeqCst);
        notified
    }

    pub fn has_completed(&self, ix: usize) -> bool {
        debug_assert!(ix < 64);
        self.completed.load(Ordering::SeqCst) & (1 << ix) != 0
    }

    pub fn mark_completed(&self, ix: usize) {
        debug_assert!(ix < 64);
        self.completed.fetch_or(1 << ix, Ordering::SeqCst);
    }

    pub fn mark_dropped(&self, ix: usize) {
        debug_assert!(ix < 64);
        self.dropped.fetch_or(1 << ix, Ordering::SeqCst);
        self.waker.wake();
    }

    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::SeqCst)
    }

    pub fn was_dropped(&self, ix: usize) -> bool {
        debug_assert!(ix < 64);
        self.dropped.load(Ordering::SeqCst) & (1 << ix) != 0
    }

    pub fn initialize(&self, ix: usize) {
        debug_assert!(ix < 64);
        self.notified.fetch_or(1 << ix, Ordering::SeqCst);
        self.completed.fetch_and(!(1 << ix), Ordering::SeqCst);
        self.dropped.fetch_and(!(1 << ix), Ordering::SeqCst);
    }

    pub fn clear(&self, ix: usize) {
        debug_assert!(ix < 64);
        let mask = !(1 << ix);
        self.notified.fetch_and(mask, Ordering::SeqCst);
        self.completed.fetch_and(mask, Ordering::SeqCst);
        self.dropped.fetch_and(mask, Ordering::SeqCst);
    }
}

pub struct WakerPageRef(NonNull<WakerPage>);

impl WakerPageRef {
    pub fn raw_waker(&self, ix: usize) -> RawWaker {
        self.waker(ix).into_raw_waker()
    }

    fn waker(&self, ix: usize) -> WakerRef {
        debug_assert!(ix < 64);

        // Bump the refcount for our new reference.
        let self_ = self.clone();
        mem::forget(self_);

        unsafe {
            let base_ptr: *mut u8 = self.0.as_ptr().cast();
            let ptr = NonNull::new_unchecked(base_ptr.offset(ix as isize));
            WakerRef(ptr)
        }
    }

}

impl Clone for WakerPageRef {
    fn clone(&self) -> Self {
        let new_refcount = unsafe {
            // See comment in `std::sync::Arc` for why `Relaxed` is safe.
            self.0.as_ref().refcount.fetch_add(1, Ordering::Relaxed)
        };
        debug_assert!(new_refcount < std::isize::MAX as u64);
        Self(self.0)
    }
}

impl Drop for WakerPageRef {
    fn drop(&mut self) {
        unsafe {
            if self.0.as_ref().refcount.fetch_sub(1, Ordering::Release) != 1 {
                return;
            }
            atomic::fence(Ordering::Acquire);
            ptr::drop_in_place(self.0.as_mut());
            Global.dealloc(self.0.cast(), Layout::for_value(self.0.as_ref()));
        }
    }
}

impl Deref for WakerPageRef {
    type Target = WakerPage;

    fn deref(&self) -> &WakerPage {
        unsafe { self.0.as_ref() }
    }
}

#[repr(transparent)]
struct WakerRef(NonNull<u8>);

impl WakerRef {
    fn base_ptr(&self) -> (NonNull<WakerPage>, usize) {
        let ptr = self.0.as_ptr();

        let forward_offset = ptr.align_offset(64);
        let mut base_ptr = ptr;
        let mut offset = 0;
        if forward_offset != 0 {
            offset = 64 - forward_offset;
            base_ptr = ptr.wrapping_sub(offset);
        }
        unsafe { (NonNull::new_unchecked(base_ptr).cast(), offset) }
    }

    fn wake_by_ref(&self) {
        let (base_ptr, offset) = self.base_ptr();
        let base = unsafe { &*base_ptr.as_ptr() };
        base.notify(offset);
    }

    fn wake(self) {
        self.wake_by_ref()
    }

    fn into_raw_waker(self) -> RawWaker {
        let ptr = self.0.cast().as_ptr() as *const ();
        let waker = RawWaker::new(ptr, &VTABLE);
        mem::forget(self);
        waker
    }
}

unsafe fn waker_ref_clone(ptr: *const ()) -> RawWaker {
    let p = WakerRef(NonNull::new_unchecked(ptr as *const u8 as *mut u8));
    let q = p.clone();
    mem::forget(p);
    q.into_raw_waker()
}

unsafe fn waker_ref_wake(ptr: *const ()) {
    let p = WakerRef(NonNull::new_unchecked(ptr as *const u8 as *mut u8));
    p.wake();
}

unsafe fn waker_ref_wake_by_ref(ptr: *const ()) {
    let p = WakerRef(NonNull::new_unchecked(ptr as *const u8 as *mut u8));
    p.wake_by_ref();
    mem::forget(p);
}

unsafe fn waker_ref_drop(ptr: *const ()) {
    let p = WakerRef(NonNull::new_unchecked(ptr as *const u8 as *mut u8));
    drop(p);
}

const VTABLE: RawWakerVTable = RawWakerVTable::new(
    waker_ref_clone,
    waker_ref_wake,
    waker_ref_wake_by_ref,
    waker_ref_drop,
);

impl Clone for WakerRef {
    fn clone(&self) -> Self {
        let (base_ptr, _) = self.base_ptr();
        let p = WakerPageRef(base_ptr);
        mem::forget(p.clone());
        mem::forget(p);
        WakerRef(self.0)
    }
}

impl Drop for WakerRef {
    fn drop(&mut self) {
        let (base_ptr, _) = self.base_ptr();
        // Decrement the refcount.
        drop(WakerPageRef(base_ptr));
    }
}

#[cfg(test)]
mod tests {
    use super::WakerPage;
    use futures::task::AtomicWaker;
    use std::{
        mem,
        sync::Arc,
    };

    #[test]
    fn test_size() {
        assert_eq!(mem::size_of::<Arc<AtomicWaker>>(), 8);
        assert_eq!(mem::size_of::<WakerPage>(), 64);
    }

    #[test]
    fn test_basic() {
        let waker = Arc::new(AtomicWaker::new());
        let p = WakerPage::new(waker);

        let q = p.waker(0);
        let r = p.waker(63);
        let s = p.waker(16);

        q.wake();
        r.wake();

        assert_eq!(p.take_notified(), 1 << 0 | 1 << 63);

        s.wake();

        assert_eq!(p.take_notified(), 1 << 16);
    }
}