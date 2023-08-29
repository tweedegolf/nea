pub mod reactor;

// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
//https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13
use std::{
    collections::VecDeque,
    future::Future,
    ops::DerefMut,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex, OnceLock,
    },
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

const QUEUE_CAPACITY: usize = 64;

const RAW_WAKER_V_TABLE: RawWakerVTable = {
    fn clone_raw(ptr: *const ()) -> RawWaker {
        RawWaker::new(ptr, &RAW_WAKER_V_TABLE)
    }

    fn drop_raw(_ptr: *const ()) {
        /* no-op */
    }

    fn wake_by_ref(ptr: *const ()) {
        wake(ptr)
    }

    fn wake(ptr: *const ()) {
        let index = Index::from_ptr(ptr);
        log::trace!("wake {}", index.index);

        // NOTE: the unit here is a lie! we just don't know the correct type for the future here
        let executor = Executor::<()>::get().unwrap();

        if executor.inner.queue.enqueue(Job::Index(index)).is_err() {
            log::warn!("task cannot be woken because the queue is full! ");
        }
    }

    RawWakerVTable::new(clone_raw, wake, wake_by_ref, drop_raw)
};

pub struct Executor<F: 'static> {
    inner: &'static Inner<F>,
}

struct Connection {
    connection: Mutex<hyper::client::conn::http1::Connection<reactor::TcpStream, String>>,
    identifier: u32,
}

impl std::task::Wake for Connection {
    fn wake(self: Arc<Self>) {
        log::trace!("wake {}", self.identifier);

        let job = Job::Connection(self);

        // NOTE: the unit here is a lie! we just don't know the correct type for the future here
        let executor = Executor::<()>::get().unwrap();

        if executor.inner.queue.enqueue(job).is_err() {
            log::warn!("task cannot be woken because the queue is full! ");
        }
    }
}

enum Job {
    Index(Index),
    Connection(Arc<Connection>),
}

impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Index(l0), Self::Index(r0)) => l0 == r0,
            (Self::Connection(l0), Self::Connection(r0)) => l0.identifier == r0.identifier,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoResources {
    pub tcp_streams: usize,
    pub http_connections: usize,
}

enum IoIndex {
    // a bucket index
    InputStream(usize),
    CustomStream(usize),
    // an index into the global vector of http connection futures
    HttpConnection(usize),
}

impl IoIndex {
    fn from_index(resources: IoResources, index: usize) -> Self {
        let total_per_bucket = resources.tcp_streams + resources.http_connections;

        let bucket_index = index / total_per_bucket;

        match index % total_per_bucket {
            0 => IoIndex::InputStream(bucket_index),
            n if (1..resources.tcp_streams).contains(&n) => todo!(),
            n => IoIndex::HttpConnection(
                bucket_index * resources.http_connections + (n - resources.tcp_streams),
            ),
        }
    }
}

type Http1Connection = hyper::client::conn::http1::Connection<reactor::TcpStream, String>;

struct Inner<F> {
    io_resources: IoResources,
    filled: Mutex<Box<[bool]>>,
    futures: Box<[Mutex<Option<Task<F>>>]>,
    connections: Box<[Mutex<Option<Http1Connection>>]>,
    queue: SimpleQueue<Job>,
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

    pub async fn handshake(
        &self,
        index: Index,
        host: &str,
        port: u16,
    ) -> std::io::Result<hyper::client::conn::http1::SendRequest<String>> {
        let stream = std::net::TcpStream::connect(format!("{}:{}", host, port)).unwrap();
        stream.set_nonblocking(true)?;

        let reactor = reactor::Reactor::get().unwrap();
        let tcp_index = Index {
            identifier: index.identifier,
            index: 2,
        };

        let stream = reactor.register(tcp_index, stream).unwrap();

        let (sender, conn) = hyper::client::conn::http1::handshake(stream).await.unwrap();

        let connection = Connection {
            connection: Mutex::new(conn),
            identifier: index.identifier,
        };

        let job = Job::Connection(Arc::new(connection));

        if self.inner.queue.enqueue(job).is_err() {
            log::warn!("connection cannot be started because the queue is full! ");
        }

        Ok(sender)
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

    pub fn get_or_init(bucket_count: usize, io_resources: IoResources) -> Self {
        match INNER.get() {
            None => {
                let queue_capacity =
                    bucket_count * (io_resources.tcp_streams + io_resources.http_connections);

                let filled = Mutex::new(std::iter::repeat(false).take(bucket_count).collect());

                let futures = std::iter::repeat_with(|| Mutex::new(None))
                    .take(bucket_count)
                    .collect();

                let connection_count = bucket_count * io_resources.http_connections;
                let connections = std::iter::repeat_with(|| Mutex::new(None))
                    .take(connection_count)
                    .collect();

                let queue = SimpleQueue::with_capacity(queue_capacity);

                let inner = Inner {
                    io_resources,
                    filled,
                    futures,
                    connections,
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
                let inner: &Inner<F> = unsafe { &*(ptr.cast()) };
                assert_eq!(io_resources, inner.io_resources);
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

        match self.inner.queue.enqueue(Job::Index(index)) {
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
            let job = self.queue.blocking_dequeue();

            match job {
                Job::Index(task_index) => {
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
                Job::Connection(task) => {
                    let waker = std::task::Waker::from(task.clone());
                    let mut cx = Context::from_waker(&waker);

                    let mut locked = task.connection.lock().unwrap();
                    let pinned = unsafe { Pin::new_unchecked(locked.deref_mut()) };

                    match pinned.poll(&mut cx) {
                        std::task::Poll::Ready(_) => {
                            // drop(task_mut);
                        }
                        std::task::Poll::Pending => {
                            // keep the future in the slab
                        }
                    }
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
