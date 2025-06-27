#![doc = include_str!("../readme.md")]

use std::{
    fmt, mem,
    ptr::{self, null_mut},
};

#[cfg(not(loom))]
use std::sync::{
    Arc,
    atomic::{AtomicPtr, Ordering},
};

#[cfg(loom)]
use loom::sync::{
    Arc,
    atomic::{AtomicPtr, Ordering},
};

/// An atomic pointer to an [`Arc`].
///
/// This pointer provides a safe atomic pointer to an [`Arc`]. Each load will
/// clone the [`Arc`] ensuring that concurrent reads/writes will only drop the
/// value when all references are decremented. The inner [`Arc`] can be swapped
/// atomically with another value.
///
/// This value is not itself cloneable and can itself be wrapped in an [`Arc`].
pub struct AtomicArc<T> {
    ptr: AtomicPtr<T>,
}

impl<T> AtomicArc<T> {
    /// Creates a new atomic pointer to an [`Arc`].
    pub fn new(arc: Arc<T>) -> Self {
        let raw = Arc::into_raw(arc) as *mut _;
        let ptr = AtomicPtr::new(ptr::null_mut());
        ptr.store(raw, Ordering::SeqCst);
        Self { ptr }
    }

    /// Load the current value cloning the inner [`Arc`].
    ///
    /// This will increment a reference count of the current [`Arc`] and return
    /// this to the caller.
    pub fn load(&self) -> Arc<T> {
        loop {
            let raw = self.ptr.swap(null_mut(), Ordering::Acquire);
            if raw.is_null() {
                // Other thread has exclusive access to the atomic ptr
                std::thread::yield_now();
                continue;
            }

            break unsafe {
                let arc = mem::ManuallyDrop::new(Arc::from_raw(raw));
                let r = mem::ManuallyDrop::into_inner(arc.clone());
                self.ptr.store(raw, Ordering::Release);
                r
            };
        }
    }

    /// Replace the current value, dropping the previously stored [`Arc`].
    pub fn store(&self, arc: Arc<T>) {
        let _ = self.swap(arc);
    }

    /// Swap the current value, returning the previously stored [`Arc`].
    ///
    /// The returned [`Arc`] may have additional references still held by other
    /// load calls previously requested.
    pub fn swap(&self, arc: Arc<T>) -> Arc<T> {
        let ptr = Arc::into_raw(arc.clone()) as *mut T;
        loop {
            let old = self.ptr.load(Ordering::Acquire);
            if old.is_null() {
                // Other thread has exclusive access to the atomic ptr
                std::thread::yield_now();
                continue;
            }

            if self
                .ptr
                .compare_exchange(old, ptr, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                // Other thread has exclusive access to the atomic ptr
                std::thread::yield_now();
                continue;
            }

            break unsafe { Arc::from_raw(old) };
        }
    }
}

impl<T> Drop for AtomicArc<T> {
    fn drop(&mut self) {
        let ptr = self.ptr.load(Ordering::Acquire);
        unsafe {
            ptr.as_mut().map(|ptr| Arc::from_raw(ptr));
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for AtomicArc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AtomicArc").field(&self.load()).finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::thread;

    use super::*;

    struct PrintOnDrop(i32);

    impl Drop for PrintOnDrop {
        fn drop(&mut self) {
            println!("drop: {}", self.0);
        }
    }

    #[test]
    fn basics() {
        let arc = AtomicArc::new(Arc::new(PrintOnDrop(1)));
        let _orig = arc.load();
        arc.swap(Arc::new(PrintOnDrop(2)));
    }

    #[test]
    fn concurrent() {
        let arc = Arc::new(AtomicArc::new(Arc::new(PrintOnDrop(1))));

        let t1 = thread::spawn({
            let handle = arc.clone();
            move || {
                for _ in 0..10 {
                    println!("{}", handle.load().0);
                }
            }
        });
        let t2 = thread::spawn({
            let handle = arc.clone();
            move || {
                for _ in 0..10 {
                    println!("{}", handle.load().0);
                }
            }
        });

        arc.swap(Arc::new(PrintOnDrop(2)));
        arc.swap(Arc::new(PrintOnDrop(3)));

        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn test_seqfault() {
        let arc = AtomicArc::new(std::sync::Arc::new(0));
        thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..1000000 {
                    let load = arc.load();
                    assert_eq!(*load, 0);
                }
            });
            scope.spawn(|| {
                for i in 0..1000000 {
                    arc.store(Arc::new(0));
                }
            });
        });
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::thread;

    use super::*;

    #[test]
    fn single_thread() {
        loom::model(|| {
            let arc = Arc::new(AtomicArc::new(Arc::new(0)));
            let handle = arc.clone();
            thread::spawn(move || {
                let v = *handle.load();
                assert!(v == 0 || v == 1);
            });
            arc.swap(Arc::new(1));
        });
    }

    #[test]
    fn two_threads() {
        loom::model(|| {
            let arc = Arc::new(AtomicArc::new(Arc::new(0)));
            let handle1 = arc.clone();
            let handle2 = arc.clone();
            thread::spawn(move || {
                let v = *handle1.load();
                assert!(v == 0 || v == 1);
            });
            thread::spawn(move || {
                let v = *handle2.load();
                assert!(v == 0 || v == 1);
            });
            arc.swap(Arc::new(1));
        });
    }

    #[test]
    fn init_and_swap() {
        loom::model(|| {
            let arc = Arc::new(AtomicArc::new(Arc::new(0)));
            arc.swap(Arc::new(1));
            let handle = arc.clone();
            thread::spawn(move || {
                let v = *handle.load();
                assert!(v == 1 || v == 2);
            });
            arc.swap(Arc::new(2));
        });
    }

    // #[test]
    // fn other_segfault() {
    //     loom::model(|| {
    //         let arc = Arc::new(AtomicArc::new(Arc::new(0)));
    //         let handle = arc.clone();
    //         thread::spawn(move || {
    //             for i in 0..3 {
    //                 let load = handle.load();
    //                 assert_eq!(*load, 0);
    //             }
    //         });
    //         thread::spawn(move || {
    //             for i in 0..3 {
    //                 arc.store(Arc::new(0));
    //             }
    //         });
    //     });
    // }
}
