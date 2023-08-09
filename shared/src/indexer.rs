use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU64, Ordering},
};

pub struct Indexer {
    handles: UnsafeCell<&'static [AtomicU64]>,
}

unsafe impl Sync for Indexer {}

impl Indexer {
    pub const fn new() -> Self {
        Self {
            handles: UnsafeCell::new(&[]),
        }
    }

    fn handles(&self) -> &'static [AtomicU64] {
        unsafe { std::ptr::read(self.handles.get()) }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn insert(&self, fd: std::os::fd::RawFd) -> Option<usize> {
        for (i, stored) in self.handles().iter().enumerate() {
            match stored.compare_exchange(0, fd as u64, Ordering::Relaxed, Ordering::Relaxed) {
                Err(_) => {
                    // no matter, just try the next index
                    continue;
                }
                Ok(_) => {
                    return Some(i);
                }
            }
        }

        None
    }

    pub fn remove(&self, index: usize) {
        self.handles()[index].store(0, Ordering::Relaxed);
    }

    pub fn initialize(&self, size: usize) {
        let handles: Vec<_> = std::iter::repeat_with(|| AtomicU64::new(0))
            .take(size)
            .collect();

        let leaked_handles = handles.leak();
        unsafe { *self.handles.get() = leaked_handles };
    }
}
