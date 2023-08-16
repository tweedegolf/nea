pub mod reactor;

// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
//https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13
use std::{
    cell::UnsafeCell,
    future::Future,
    mem::{ManuallyDrop, MaybeUninit},
    ops::DerefMut,
    pin::Pin,
    sync::{Mutex, OnceLock},
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

const QUEUE_CAPACITY: usize = 4;

const RAW_WAKER_V_TABLE: RawWakerVTable =
    RawWakerVTable::new(clone_raw, wake, wake_by_ref, drop_raw);

fn clone_raw(ptr: *const ()) -> RawWaker {
    RawWaker::new(ptr, &RAW_WAKER_V_TABLE)
}

fn drop_raw(_ptr: *const ()) {}

fn wake_by_ref(_ptr: *const ()) {
    unreachable!()
}

fn wake(ptr: *const ()) {
    let index = ptr as usize;
    log::info!("wake {index}");

    let executor = Executor::get();

    if let Err(_) = executor.inner.queue.enqueue(index) {
        log::warn!("task cannot be woken because the queue is full! ");
    }
}

pub struct Executor {
    inner: &'static Inner,
}

struct Inner {
    futures: Box<[UnsafeCell<MaybeUninit<Task>>]>,
    filled: Mutex<Box<[bool]>>,
    queue: shared::queue::LockFreeQueue<usize, QUEUE_CAPACITY>,
}

unsafe impl std::marker::Send for Inner {}
unsafe impl std::marker::Sync for Inner {}

struct Task {
    fut: Box<dyn Future<Output = ()> + Send>,
}

impl Executor {
    pub fn get() -> Self {
        static INNER: OnceLock<Inner> = OnceLock::new();

        match INNER.get() {
            None => {
                let futures = std::iter::repeat_with(|| UnsafeCell::new(MaybeUninit::uninit()))
                    .take(QUEUE_CAPACITY)
                    .collect();
                let filled = Mutex::new(std::iter::repeat(false).take(QUEUE_CAPACITY).collect());

                let queue = shared::queue::LockFreeQueue::new();

                let inner = Inner {
                    futures,
                    filled,
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

    pub fn reserve(&self) -> Result<usize, ()> {
        self.inner.reserve().ok_or(())
    }

    pub fn execute<F>(&self, index: usize, fut: F) -> Result<(), ()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task = Task { fut: Box::new(fut) };

        self.inner.set(index, task);

        self.inner.queue.enqueue(index).unwrap();

        Ok(())
    }
}

impl Inner {
    fn run(&self) {
        loop {
            let task_index = self.queue.blocking_dequeue();

            // *mut MaybeUninit<Box<dyn Future<Output = ()> + Send>>
            let x = self.futures[task_index].get();
            let mut task = unsafe { std::ptr::read(x) };

            let pinned = unsafe { Pin::new_unchecked(task.assume_init_mut().fut.as_mut()) };

            let raw_waker = RawWaker::new(task_index as *const (), &RAW_WAKER_V_TABLE);
            let waker = unsafe { Waker::from_raw(raw_waker) };
            let mut cx = Context::from_waker(&waker);

            match pinned.poll(&mut cx) {
                std::task::Poll::Ready(_) => {
                    // allright, future is done, free up its spot in the slab
                    log::info!("task {task_index} is done");
                    self.remove(task_index);
                }
                std::task::Poll::Pending => {
                    // keep the future in the slab
                }
            }
        }
    }

    fn reserve(&self) -> Option<usize> {
        let mut guard = self.filled.lock().unwrap();
        let index = guard.iter().position(|filled| !filled)?;

        guard[index] = true;

        Some(index)
    }

    fn set(&self, index: usize, value: Task) {
        let mut value = ManuallyDrop::new(value);

        let source = (&mut value).deref_mut();
        let target = self.futures[index].get();

        unsafe { std::ptr::swap(source, target.cast::<Task>()) };
    }

    fn remove(&self, index: usize) {
        let mut guard = self.filled.lock().unwrap();

        let mut value = MaybeUninit::uninit();
        let source = self.futures[index].get().cast::<Task>();
        let target = value.as_mut_ptr();

        unsafe { std::ptr::swap(source, target) };

        drop(unsafe { value.assume_init() });

        let mut set = false;
        std::mem::swap(&mut guard[index], &mut set);

        assert!(set, "to remove a future it must be present");
    }
}
