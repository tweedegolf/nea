pub mod reactor;

// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
//https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

const QUEUE_CAPACITY: usize = 64;

const RAW_WAKER_V_TABLE: RawWakerVTable =
    RawWakerVTable::new(clone_raw, wake, wake_by_ref, drop_raw);

fn clone_raw(ptr: *const ()) -> RawWaker {
    RawWaker::new(ptr, &RAW_WAKER_V_TABLE)
}

fn drop_raw(_ptr: *const ()) {
    /* no-op */
}

fn wake_by_ref(_ptr: *const ()) {
    unreachable!()
}

fn wake(ptr: *const ()) {
    let index = Index::from_ptr(ptr);
    log::trace!("wake {}", index.index);

    // NOTE: the unit here is a lie! we just don't know the correct type for the future here
    let executor = Executor::<()>::get().unwrap();

    if executor.inner.queue.enqueue(index).is_err() {
        log::warn!("task cannot be woken because the queue is full! ");
    }
}

pub struct Executor<F: 'static> {
    inner: &'static Inner<F>,
}

struct Inner<F> {
    futures: Box<[Mutex<Option<Task<F>>>]>,
    filled: Mutex<Box<[bool]>>,
    queue: SimpleQueue<Index>,
    identifier: AtomicU32,
}

unsafe impl<F: Send> std::marker::Send for Inner<F> {}
unsafe impl<F: Send> std::marker::Sync for Inner<F> {}

#[repr(C)]
struct Task<F> {
    fut: F,
    identifier: u32,
}

static INNER: OnceLock<&()> = OnceLock::new();

impl<F> Executor<F> {
    pub fn get() -> Option<Self> {
        let inner: &&() = INNER.get()?;
        let ptr: *const () = *inner;
        let inner = unsafe { &*(ptr.cast()) };
        // already initialized
        Some(Executor { inner })
    }
}

impl<F> Executor<F>
where
    F: Future<Output = ()> + Send + 'static,
{
    pub fn type_hint<G>(&self, _: G)
    where
        G: Fn(reactor::TcpStream) -> F,
    {
    }

    pub fn get_or_init() -> Self {
        match INNER.get() {
            None => {
                let futures = std::iter::repeat_with(|| Mutex::new(None))
                    .take(QUEUE_CAPACITY)
                    .collect();
                let filled = Mutex::new(std::iter::repeat(false).take(QUEUE_CAPACITY).collect());

                let queue = SimpleQueue::with_capacity(QUEUE_CAPACITY);

                let inner = Inner {
                    futures,
                    filled,
                    queue,
                    identifier: AtomicU32::new(1),
                };

                let boxed = Box::new(inner);
                let leaked = Box::leak(boxed) as &'static Inner<_>;
                let ptr_unit =
                    unsafe { std::mem::transmute::<&'static Inner<_>, &'static ()>(leaked) };

                let _ = INNER.get_or_init(move || ptr_unit);

                Executor { inner: leaked }
            }

            Some(inner) => {
                let ptr: *const () = *inner;
                let inner = unsafe { &*(ptr.cast()) };
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

    pub fn try_claim(&self) -> Option<Index> {
        self.inner.try_claim()
    }

    pub fn execute(&self, index: Index, fut: F) {
        let task = Task {
            fut,
            identifier: index.identifier,
        };

        self.inner.set(index, task);

        match self.inner.queue.enqueue(index) {
            Ok(()) => (),
            Err(_) => unreachable!("we claimed a spot!"),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Index {
    pub identifier: u32,
    pub index: u32,
}

impl Index {
    const fn to_usize(self) -> usize {
        let a = self.identifier.to_ne_bytes();
        let b = self.index.to_ne_bytes();

        let bytes = [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]];

        usize::from_ne_bytes(bytes)
    }

    pub const fn to_ptr(self) -> *const () {
        self.to_usize() as *const ()
    }

    fn from_usize(word: usize) -> Self {
        let bytes = word.to_ne_bytes();

        let [a0, a1, a2, a3, b0, b1, b2, b3] = bytes;

        let identifier = u32::from_ne_bytes([a0, a1, a2, a3]);
        let index = u32::from_ne_bytes([b0, b1, b2, b3]);

        Self { identifier, index }
    }

    fn from_ptr(ptr: *const ()) -> Self {
        Self::from_usize(ptr as usize)
    }
}

impl<F> Inner<F>
where
    F: Future<Output = ()> + Send,
{
    fn run(&self) {
        loop {
            let task_index = self.queue.blocking_dequeue();

            let mut task_mut = self.futures[task_index.index as usize].lock().unwrap();

            let task = match task_mut.as_mut() {
                None => continue,
                Some(task) if task.identifier != task_index.identifier => continue,
                Some(inner) => inner,
            };

            let pinned = unsafe { Pin::new_unchecked(&mut task.fut) };

            let raw_waker = RawWaker::new(task_index.to_ptr(), &RAW_WAKER_V_TABLE);
            let waker = unsafe { Waker::from_raw(raw_waker) };
            let mut cx = Context::from_waker(&waker);

            match pinned.poll(&mut cx) {
                std::task::Poll::Ready(_) => {
                    // allright, future is done, free up its spot in the slab
                    log::info!("task {} is done", task_index.index);
                    drop(task_mut);
                    self.remove(task_index);
                }
                std::task::Poll::Pending => {
                    // keep the future in the slab
                }
            }
        }
    }

    fn try_claim(&self) -> Option<Index> {
        let mut guard = self.filled.lock().unwrap();
        let index = guard.iter().position(|filled| !filled)?;

        guard[index] = true;

        let identifier = self.identifier.fetch_add(1, Ordering::Relaxed);
        let index = index as u32;

        Some(Index { identifier, index })
    }

    fn set(&self, index: Index, value: Task<F>) {
        let old = self.futures[index.index as usize]
            .try_lock()
            .unwrap()
            .replace(value);
        debug_assert!(old.is_none());
    }

    fn remove(&self, index: Index) {
        let mut guard = self.filled.lock().unwrap();

        loop {
            match self.futures[index.index as usize].lock().unwrap().take() {
                Some(old) => {
                    drop(old);
                    break;
                }
                None => {
                    log::info!("could not claim {}", index.index as usize);
                }
            }
        }

        let mut set = false;
        std::mem::swap(&mut guard[index.index as usize], &mut set);

        assert!(set, "to remove a future it must be present");
    }
}

struct SimpleQueue<T> {
    queue: Mutex<VecDeque<T>>,
    condvar: std::sync::Condvar,
}

impl<T: PartialEq> SimpleQueue<T> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            condvar: std::sync::Condvar::new(),
        }
    }

    fn enqueue(&self, value: T) -> Result<(), T> {
        let mut queue = self.queue.lock().unwrap();

        if !queue.contains(&value) {
            queue.push_back(value);
            self.condvar.notify_one();
        }

        Ok(())
    }

    fn blocking_dequeue(&self) -> T {
        let mut queue = self.queue.lock().unwrap();

        loop {
            match queue.pop_front() {
                Some(value) => return value,
                None => queue = self.condvar.wait(queue).unwrap(),
            }
        }
    }
}
