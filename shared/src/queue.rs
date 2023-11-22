use std::{
    cell::{Cell, UnsafeCell},
    mem::MaybeUninit,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

#[derive(Debug)]
struct Slot<T> {
    stamp: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

#[derive(Debug)]
pub struct LockFreeQueue<T, const N: usize> {
    buffer: [Slot<T>; N],
    head: AtomicUsize,
    tail: AtomicUsize,
    waiter: AtomicU32,
}

impl<T, const N: usize> Default for LockFreeQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

// it is safe to move a LockFreeQueue<T, N> across threads if T's can be moved across threads
unsafe impl<T: Send, const N: usize> Send for LockFreeQueue<T, N> {}

// it is safe to share a LockFreeQueue<T, N> between threads if T's can be moved between threads
unsafe impl<T: Send, const N: usize> Sync for LockFreeQueue<T, N> {}

impl<T, const N: usize> LockFreeQueue<T, N> {
    const ONE_LAP: usize = (N + 1).next_power_of_two();
    const EMPTY_SLOT: Slot<T> = Slot {
        stamp: AtomicUsize::new(0),
        value: UnsafeCell::new(MaybeUninit::uninit()),
    };

    pub fn new() -> Self {
        let buffer = [Self::EMPTY_SLOT; N];

        let mut i = 0;
        while i < N {
            buffer[i].stamp.store(i, Ordering::Relaxed);
            i += 1;
        }

        LockFreeQueue {
            buffer,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            waiter: AtomicU32::new(0),
        }
    }

    // we assume a single writer
    pub fn enqueue(&self, item: T) -> Result<(), T> {
        let backoff = Backoff::new();
        let mut tail = self.tail.load(Ordering::Relaxed);

        loop {
            // Deconstruct the tail
            let index = tail & (Self::ONE_LAP - 1);
            let lap = tail & !(Self::ONE_LAP - 1);

            let new_tail = if index + 1 < N {
                // Same lap, incremented index.
                // Set to `{ lap: lap, index: index + 1 }`.
                tail + 1
            } else {
                // One lap forward, index wraps around to zero.
                // Set to `{ lap: lap.wrapping_add(1), index: 0 }`.
                lap.wrapping_add(Self::ONE_LAP)
            };

            // Inspect the corresponding slot.
            debug_assert!(index < self.buffer.len());
            let slot = unsafe { self.buffer.get_unchecked(index) };
            let stamp = slot.stamp.load(Ordering::Acquire);

            // If the tail and the stamp match, we may attempt to push.
            if tail == stamp {
                // Try moving the tail.
                match self.tail.compare_exchange_weak(
                    tail,
                    new_tail,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Write the value into the slot and update the stamp.
                        unsafe {
                            slot.value.get().write(MaybeUninit::new(item));
                        }
                        slot.stamp.store(tail + 1, Ordering::Release);

                        atomic_wait::wake_one(&self.waiter);

                        return Ok(());
                    }
                    Err(t) => {
                        tail = t;
                        backoff.spin();
                    }
                }
            } else if stamp.wrapping_add(Self::ONE_LAP) == tail + 1 {
                std::sync::atomic::fence(Ordering::SeqCst);
                return Err(item);
            } else {
                // Snooze because we need to wait for the stamp to get updated.
                backoff.snooze();
                tail = self.tail.load(Ordering::Relaxed);
            }
        }
    }

    pub fn blocking_dequeue(&self) -> T {
        loop {
            match self.dequeue() {
                Some(x) => return x,
                None => {
                    log::trace!("thread {:?} going to sleep", std::thread::current().id());
                    atomic_wait::wait(&self.waiter, 0);
                    log::trace!("thread {:?} is back up", std::thread::current().id());
                }
            }
        }
    }

    fn dequeue(&self) -> Option<T> {
        let backoff = Backoff::new();
        let mut head = self.head.load(Ordering::Relaxed);

        loop {
            // Deconstruct the head.
            let index = head & (Self::ONE_LAP - 1);
            let lap = head & !(Self::ONE_LAP - 1);

            // Inspect the corresponding slot.
            debug_assert!(index < self.buffer.len());
            let slot = unsafe { self.buffer.get_unchecked(index) };
            let stamp = slot.stamp.load(Ordering::Acquire);

            // If the the stamp is ahead of the head by 1, we may attempt to pop.
            if head + 1 == stamp {
                let new = if index + 1 < N {
                    // Same lap, incremented index.
                    // Set to `{ lap: lap, index: index + 1 }`.
                    head + 1
                } else {
                    // One lap forward, index wraps around to zero.
                    // Set to `{ lap: lap.wrapping_add(1), index: 0 }`.
                    lap.wrapping_add(Self::ONE_LAP)
                };

                // Try moving the head.
                match self.head.compare_exchange_weak(
                    head,
                    new,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Read the value from the slot and update the stamp.
                        let msg = unsafe { slot.value.get().read().assume_init() };
                        slot.stamp
                            .store(head.wrapping_add(Self::ONE_LAP), Ordering::Release);
                        return Some(msg);
                    }
                    Err(h) => {
                        head = h;
                        backoff.spin();
                    }
                }
            } else if stamp == head {
                std::sync::atomic::fence(Ordering::SeqCst);
                let tail = self.tail.load(Ordering::Relaxed);

                // If the tail equals the head, that means the channel is empty.
                if tail == head {
                    return None;
                }

                backoff.spin();
                head = self.head.load(Ordering::Relaxed);
            } else {
                // Snooze because we need to wait for the stamp to get updated.
                backoff.snooze();
                head = self.head.load(Ordering::Relaxed);
            }
        }
    }
}

const SPIN_LIMIT: u32 = 6;
const YIELD_LIMIT: u32 = 10;

pub struct Backoff {
    step: Cell<u32>,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    /// Creates a new `Backoff`.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_utils::Backoff;
    ///
    /// let backoff = Backoff::new();
    /// ```
    #[inline]
    pub fn new() -> Self {
        Backoff { step: Cell::new(0) }
    }
    #[inline]
    pub fn spin(&self) {
        for _ in 0..1 << self.step.get().min(SPIN_LIMIT) {
            core::hint::spin_loop()
        }

        if self.step.get() <= SPIN_LIMIT {
            self.step.set(self.step.get() + 1);
        }
    }

    #[inline]
    pub fn snooze(&self) {
        if self.step.get() <= SPIN_LIMIT {
            for _ in 0..1 << self.step.get() {
                core::hint::spin_loop();
            }
        } else {
            println!("thread {:?} going to sleep", std::thread::current().id());
            ::std::thread::yield_now();
        }

        if self.step.get() <= YIELD_LIMIT {
            self.step.set(self.step.get() + 1);
        }
    }
}
