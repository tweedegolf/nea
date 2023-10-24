use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread;

use crate::executor::QueueIndex;

pub const ENQUEUED_BIT: u8 = 0b0001;
pub const IN_PROGRESS_BIT: u8 = 0b0010;

pub struct ComplexQueue {
    queue: Box<[Mutex<u8>]>,
    // number of currently enqueued items
    jobs_in_queue: Refcount2,
}

struct Refcount2 {
    active_mutex: Arc<Mutex<u32>>,
    active_condvar: Condvar,
}

impl Refcount2 {
    fn new() -> Self {
        Self {
            active_mutex: Default::default(),
            active_condvar: Condvar::new(),
        }
    }

    fn increment_active(&self) {
        let mut guard = self.active_mutex.lock().unwrap();
        *guard = guard.checked_add(1).expect("no overflow");
    }

    fn wake_one(&self) {
        self.active_condvar.notify_one()
    }

    fn decrement_active(&self) {
        let mut guard = self.active_mutex.lock().unwrap();
        *guard = guard.checked_sub(1).expect("no underflow");
    }

    fn wait_active(&self) -> u32 {
        let mut guard = self.active_mutex.lock().unwrap();
        while *guard == 0 {
            guard = self.active_condvar.wait(guard).unwrap();
        }

        *guard
    }
}

struct Refcount1 {
    rc: AtomicU32,
}

impl Refcount1 {
    fn new() -> Self {
        Self {
            rc: AtomicU32::new(0),
        }
    }

    fn increment_active(&self) {
        let old = self.rc.fetch_add(1, Ordering::SeqCst);

        if let None = old.checked_add(1) {
            let thread = thread::current().id();
            eprintln!("{thread:?}: RC does not overflow");
            panic!();
        }
    }

    fn wake_one(&self) {
        atomic_wait::wake_one(&self.rc);
    }

    fn decrement_active(&self) {
        let old = self.rc.fetch_sub(1, Ordering::SeqCst);

        if let None = old.checked_sub(1) {
            let thread = thread::current().id();
            eprintln!("{thread:?}: RC does not underflow");
            panic!();
        }
    }

    fn wait_active(&self) {
        while self.rc.load(Ordering::Relaxed) == 0 {
            atomic_wait::wait(&self.rc, 0);
        }
    }
}

#[derive(Debug)]
pub enum DoneWithItem {
    Done,
    GoAgain,
}

impl ComplexQueue {
    pub fn with_capacity(capacity: usize) -> Self {
        let queue = std::iter::repeat_with(|| Mutex::new(0))
            .take(capacity)
            .collect();

        Self {
            queue,
            jobs_in_queue: Refcount2::new(),
        }
    }

    fn increment_jobs_available(&self) {
        self.jobs_in_queue.increment_active()
    }

    fn decrement_jobs_available(&self) {
        self.jobs_in_queue.decrement_active()
    }

    fn wait_jobs_available(&self) -> u32 {
        self.jobs_in_queue.wait_active()
    }

    pub fn enqueue(&self, index: QueueIndex) -> Result<(), QueueIndex> {
        let current = &self.queue[index.index as usize];

        // eagerly increment (but don't wake anyone yet)
        self.increment_jobs_available();

        // mark this slot as enqueue'd
        let mut guard = current.lock().unwrap();

        // let old_value = current.fetch_or(ENQUEUED_BIT, Ordering::Relaxed);
        let old_value = *guard;
        *guard |= ENQUEUED_BIT;

        if old_value & ENQUEUED_BIT == 0 && old_value & IN_PROGRESS_BIT == 0 {
            // this is a new job, notify a worker
            self.jobs_in_queue.wake_one();

            let thread = thread::current().id();
            log::warn!(
                "{thread:?}: ++++++++++ job {} is new in the queue",
                index.index
            );
        } else {
            // the job was already enqueue'd, revert
            self.decrement_jobs_available();

            let thread = thread::current().id();
            log::warn!(
                "{thread:?}: ++++++++++ job {} was already in the queue ({}, {})",
                index.index,
                format_args!("ENQUEUED_BIT: {}", old_value & ENQUEUED_BIT != 0),
                format_args!("IN_PROGRESS_BIT: {}", old_value & IN_PROGRESS_BIT != 0),
            );
        }

        Ok(())
    }

    #[must_use]
    pub fn done_with_item(&self, guard: &mut MutexGuard<u8>) -> DoneWithItem {
        // let old_value = current.fetch_and(!ENQUEUED_BIT, Ordering::Relaxed);
        let old_value = **guard;
        **guard &= !ENQUEUED_BIT;

        assert!(old_value & IN_PROGRESS_BIT != 0);

        if old_value & ENQUEUED_BIT != 0 {
            DoneWithItem::GoAgain
        } else {
            // let old_value = current.fetch_and(!IN_PROGRESS_BIT, Ordering::Relaxed);
            // assert!(old_value & IN_PROGRESS_BIT != 0);

            DoneWithItem::Done
        }
    }

    pub fn done_with_item_forever(&self, guard: &mut MutexGuard<u8>) {
        **guard = 0;
    }

    fn try_dequeue(&self) -> Option<(QueueIndex, MutexGuard<u8>)> {
        // first, try to find something
        let split_index = 0;

        let (later, first) = self.queue.split_at(split_index);
        let it = first.iter().chain(later).enumerate();

        for (i, value) in it {
            // find a task that is enqueued but not yet in progress
            let mut guard = match value.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Poisoned(e)) => panic!("{e:?}"),
            };

            let old = *guard;

            if *guard != ENQUEUED_BIT {
                continue;
            }

            *guard = IN_PROGRESS_BIT;

            let thread = thread::current().id();
            log::warn!("{thread:?}: picked {i} {old:b}");

            self.decrement_jobs_available();

            let index = QueueIndex {
                index: i as u32,
                identifier: 0,
            };

            return Some((index, guard));
        }

        None
    }

    pub fn blocking_dequeue(&self) -> (QueueIndex, MutexGuard<u8>) {
        let thread = thread::current().id();

        loop {
            let n = self.wait_jobs_available();
            log::warn!("{thread:?}: done waiting, {n} jobs available");

            if let Some((index, guard)) = self.try_dequeue() {
                return (index, guard);
            } else {
                log::warn!("{thread:?}: could not claim the promised job");
            }
        }
    }

    pub fn clear_slots_for(&self, range: Range<usize>) {
        use std::ops::DerefMut;

        for slot in self.queue[range].iter() {
            // let old = slot.swap(0, Ordering::Relaxed);
            let old = std::mem::take(slot.lock().unwrap().deref_mut());
        }
    }
}
