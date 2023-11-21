use std::ops::Range;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;

use crate::executor::QueueIndex;

const ENQUEUED_BIT: u8 = 0b0001;
const IN_PROGRESS_BIT: u8 = 0b0010;

#[derive(Debug)]
pub struct QueueSlotState {
    pub is_enqueued: bool,
    pub is_in_progress: bool,
}

pub struct QueueSlot {
    flags: AtomicU8,
    pub id: AtomicU32,
}

impl QueueSlot {
    const fn empty() -> Self {
        Self {
            flags: AtomicU8::new(0),
            id: AtomicU32::new(0),
        }
    }

    fn is_empty(&self) -> bool {
        self.flags.load(Ordering::Relaxed) == 0
    }

    fn is_enqueued(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & ENQUEUED_BIT != 0
    }

    fn is_in_progress(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & IN_PROGRESS_BIT != 0
    }

    pub fn mark_empty(&self) {
        self.flags.store(0, Ordering::Relaxed);
    }

    fn try_process(&self) -> Result<u32, ()> {
        match self.flags.compare_exchange(
            ENQUEUED_BIT,
            IN_PROGRESS_BIT,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(self.id.load(Ordering::Relaxed)),
            Err(_) => Err(()),
        }
    }

    fn mark_enqueued(&self) -> QueueSlotState {
        let old = self.flags.fetch_or(ENQUEUED_BIT, Ordering::Relaxed);

        QueueSlotState {
            is_enqueued: old & ENQUEUED_BIT != 0,
            is_in_progress: old & IN_PROGRESS_BIT != 0,
        }
    }

    fn mark_in_progress(&self) {
        let old = self.flags.fetch_or(IN_PROGRESS_BIT, Ordering::Relaxed);
    }

    pub fn clear_enqueued(&self) -> QueueSlotState {
        let old = self.flags.fetch_and(!ENQUEUED_BIT, Ordering::Relaxed);

        QueueSlotState {
            is_enqueued: old & ENQUEUED_BIT != 0,
            is_in_progress: old & IN_PROGRESS_BIT != 0,
        }
    }

    pub fn clear_in_progress(&self) -> QueueSlotState {
        let old = self.flags.fetch_and(!IN_PROGRESS_BIT, Ordering::Relaxed);

        QueueSlotState {
            is_enqueued: old & ENQUEUED_BIT != 0,
            is_in_progress: old & IN_PROGRESS_BIT != 0,
        }
    }
}

pub struct ComplexQueue {
    queue: Box<[QueueSlot]>,
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

    fn increment_active(&self) -> u32 {
        let mut guard = self.active_mutex.lock().unwrap();
        match guard.checked_add(1) {
            None => {
                let thread = thread::current().id();
                eprintln!("{thread:?}: RC does not overflow");
                panic!("increment_active: overflow");
            }
            Some(v) => {
                *guard = v;
                v
            }
        }
    }

    fn wake_one(&self) {
        self.active_condvar.notify_one()
    }

    fn decrement_active(&self) -> u32 {
        let mut guard = self.active_mutex.lock().unwrap();
        match guard.checked_sub(1) {
            None => {
                let thread = thread::current().id();
                eprintln!("{thread:?}: RC does not overflow");
                panic!("decrement_active: overflow");
            }
            Some(v) => {
                *guard = v;
                v
            }
        }
    }

    fn wait_active(&self) -> u32 {
        let thread = thread::current().id();
        let mut guard = self.active_mutex.lock().unwrap();

        if *guard == 0 {
            log::warn!("{thread:?}: 😴");

            while *guard == 0 {
                guard = self.active_condvar.wait(guard).unwrap();
            }
        }

        *guard
    }
}

#[derive(Debug)]
pub enum NextStep {
    /// The element is no longer in the queue
    Done,
    /// The element was enqueued again while it was processed
    GoAgain,
}

impl ComplexQueue {
    pub fn with_capacity(capacity: usize) -> Self {
        let queue = std::iter::repeat_with(|| QueueSlot::empty())
            .take(capacity)
            .collect();

        Self {
            queue,
            jobs_in_queue: Refcount2::new(),
        }
    }

    fn increment_jobs_available(&self) -> u32 {
        self.jobs_in_queue.increment_active()
    }

    fn decrement_jobs_available(&self) -> u32 {
        self.jobs_in_queue.decrement_active()
    }

    fn load_jobs_available(&self) -> u32 {
        *self.jobs_in_queue.active_mutex.lock().unwrap()
    }

    fn wait_jobs_available(&self) -> u32 {
        self.jobs_in_queue.wait_active()
    }

    pub fn is_range_empty(&self, range: Range<QueueIndex>) -> bool {
        for slot in &self.queue[range.start.index as usize..range.end.index as usize] {
            if !slot.is_empty() {
                return false;
            }
        }

        true
    }

    /// Wake an existing task by putting it back into the queue
    pub fn wake(&self, index: QueueIndex) {
        let thread = thread::current().id();

        let Some(current) = self.queue.get(index.index as usize) else {
            panic!("{thread:?}: queue index {index:?} out of bounds");
        };

        let id = current.id.load(Ordering::Relaxed);
        if id != index.identifier {
            log::info!("{thread:?}: skipped waking {}; out of date", index.index);
            log::info!("{thread:?}: {} (queue) vs {} (waker)", id, index.identifier);

            return;
        }

        // eagerly increment (but don't wake anyone yet). This is to prevent race conditions
        // between marking the slot as enqueued and updating the count of enqueued slots.
        let jobs_available = self.increment_jobs_available();

        let old_slot_state = current.mark_enqueued();

        if !old_slot_state.is_enqueued && !old_slot_state.is_in_progress {
            log::warn!(
                "{thread:?}: ☀️  job {} is new in the queue ({jobs_available} jobs available)",
                index.index
            );

            // this is a new job, notify a worker
            self.jobs_in_queue.wake_one();
        } else {
            // the job was already enqueue'd, revert
            self.decrement_jobs_available();

            log::warn!(
                "{thread:?}: job {} was already in the queue ({old_slot_state:?}, {} jobs  available)",
                index.index,
                jobs_available - 1,
            );
        }
    }

    /// The first enqueue of a task. Must NOT be used to wake an existing task!
    pub fn initial_enqueue(&self, index: QueueIndex) {
        let thread = thread::current().id();

        let Some(current) = self.queue.get(index.index as usize) else {
            panic!("{thread:?}: queue index {index:?} out of bounds");
        };

        // eagerly increment (but don't wake anyone yet). This is to prevent race conditions
        // between marking the slot as enqueued and updating the count of enqueued slots.
        let jobs_available = self.increment_jobs_available();

        let old_slot_state = current.mark_enqueued();

        if !old_slot_state.is_enqueued && !old_slot_state.is_in_progress {
            log::warn!(
                "{thread:?}: ☀️  job {} (id {}) is new in the queue ({jobs_available} jobs available)!",
                index.index,
                index.identifier,
            );

            current.id.store(index.identifier, Ordering::Relaxed);

            // this is a new job, notify a worker
            self.jobs_in_queue.wake_one();
        } else {
            // the job was already enqueue'd, revert
            self.decrement_jobs_available();

            log::warn!(
                "{thread:?}: job {} was already in the queue ({old_slot_state:?}, {} jobs  available)!",
                index.index,
                jobs_available - 1,
            );
        }
    }

    #[must_use]
    pub fn done_with_item(&self, queue_slot: &QueueSlot) -> NextStep {
        // in theory you can be unlucky and have the clear progress step fail.
        for _ in std::iter::repeat(()).take(2) {
            let enqueued_while_in_progress = queue_slot.flags.compare_exchange(
                ENQUEUED_BIT | IN_PROGRESS_BIT,
                IN_PROGRESS_BIT,
                Ordering::Acquire,
                Ordering::Acquire,
            );

            let Err(state) = enqueued_while_in_progress else {
                return NextStep::GoAgain;
            };

            assert_eq!(state, IN_PROGRESS_BIT);

            let clear_in_progress = queue_slot.flags.compare_exchange(
                IN_PROGRESS_BIT,
                0,
                Ordering::Acquire,
                Ordering::Acquire,
            );

            match clear_in_progress {
                Err(_) => continue,
                Ok(_) => return NextStep::Done,
            }
        }

        unreachable!("could not clear in progress state")
    }

    fn try_dequeue(&self) -> Option<(QueueIndex, &QueueSlot)> {
        let thread = thread::current().id();

        // first, try to find something
        let split_index = 0;

        let (later, first) = self.queue.split_at(split_index);
        let it = first.iter().chain(later).enumerate();

        for (i, queue_slot) in it {
            // find a task that is enqueued but not yet in progress
            let Ok(identifier) = queue_slot.try_process() else {
                // either
                //
                // - the slot is empty
                // - the slot is not enqueued
                // - the slot is already in progress
                continue;
            };

            // this has a (potential) race condition with the insertion. A value is marked as
            // enqueued before the reference count is incremented
            let now_available = self.decrement_jobs_available();

            log::warn!("{thread:?}: picked {i} (now {now_available} available)");

            let index = QueueIndex {
                index: i as u32,
                identifier,
            };

            return Some((index, queue_slot));
        }

        None
    }

    pub fn blocking_dequeue(&self) -> (QueueIndex, &QueueSlot) {
        let thread = thread::current().id();

        loop {
            let n = self.wait_jobs_available();
            log::warn!("{thread:?}: ⚙️  done waiting, {n} jobs available");

            log::warn!(
                "{thread:?}: blocking dequeue, {} available",
                self.load_jobs_available()
            );

            if let Some((index, queue_slot)) = self.try_dequeue() {
                return (index, queue_slot);
            } else {
                log::warn!("{thread:?}: could not claim the promised job");
            }
        }
    }
}
