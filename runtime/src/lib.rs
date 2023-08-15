pub mod reactor;

// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
//https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

use slab::Slab;

const QUEUE_CAPACITY: usize = 4;

const RAW_WAKER_V_TABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake, wake, drop_raw);

fn clone_raw(ptr: *const ()) -> RawWaker {
    RawWaker::new(ptr, &RAW_WAKER_V_TABLE)
}

fn drop_raw(_ptr: *const ()) {}

fn wake(ptr: *const ()) {
    let index = ptr as usize;

    let executor = Executor::get();

    if let Err(_) = executor.inner.queue.enqueue(index) {
        log::warn!("task cannot be woken because the queue is full! ");
    }
}

pub struct Executor {
    inner: &'static Inner,
}

struct Inner {
    slab: Mutex<Slab<Task>>,
    queue: shared::queue::LockFreeQueue<usize, QUEUE_CAPACITY>,
}

struct Task {
    fut: Box<dyn Future<Output = ()> + Send>,
}

impl Executor {
    pub fn get() -> Self {
        static INNER: OnceLock<Inner> = OnceLock::new();

        match INNER.get() {
            None => {
                let slab = Slab::with_capacity(QUEUE_CAPACITY);
                let shared = Mutex::new(slab);

                let queue = shared::queue::LockFreeQueue::new();

                let inner = Inner {
                    slab: shared,
                    queue,
                };

                let inner = INNER.get_or_init(move || inner);

                Executor { inner }
            }

            Some(inner) => {
                // already initialized
                Executor { inner }
            }
        }
    }

    pub fn spawn_worker(&self) -> std::io::Result<std::thread::JoinHandle<()>> {
        let inner = self.inner;
        std::thread::Builder::new()
            .name(String::from("nea-worker"))
            .spawn(|| inner.run())
    }

    pub fn execute<F>(&self, fut: F) -> Result<(), ()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut slab = self.inner.slab.lock().unwrap();
        let task = Task { fut: Box::new(fut) };

        if ACTIVE.load(Ordering::Acquire) > QUEUE_CAPACITY {
            log::info!("queue is full! request is dropped");

            Err(())
        } else {
            ACTIVE.fetch_add(1, Ordering::Release);

            let index = slab.insert(task);
            if let Err(_) = self.inner.queue.enqueue(index) {
                log::warn!("queue is full! request is dropped");
                Err(())
            } else {
                Ok(())
            }
        }
    }
}

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

impl Inner {
    fn run(&self) {
        loop {
            let task_index = self.queue.blocking_dequeue();

            let mut slab = self.slab.lock().unwrap();
            let task = &mut slab[task_index];
            let pinned = unsafe { Pin::new_unchecked(task.fut.as_mut()) };

            let raw_waker = RawWaker::new(task_index as *const (), &RAW_WAKER_V_TABLE);
            let waker = unsafe { Waker::from_raw(raw_waker) };
            let mut cx = Context::from_waker(&waker);

            match pinned.poll(&mut cx) {
                std::task::Poll::Ready(_) => {
                    // allright, future is done, free up its spot in the slab
                    slab.remove(task_index);
                    ACTIVE.fetch_sub(1, Ordering::Release);
                }
                std::task::Poll::Pending => {
                    // keep the future in the slab
                }
            }
        }
    }
}
