// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use index::Http2FutureIndex;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    future::Future,
    ops::{DerefMut, Range},
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex, OnceLock,
    },
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

mod config;
mod index;
pub mod reactor;
mod server;

pub use index::{BucketIndex, ConnectionIndex, IoIndex, QueueIndex};
pub use server::Nea;

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
        let index = QueueIndex::from_ptr(ptr);
        log::trace!("wake {}", index.index);

        // NOTE: the unit here is a lie! we just don't know the correct type for the future here
        let executor = Executor::<()>::get().unwrap();

        if executor.inner.queue.enqueue(index).is_err() {
            log::warn!("task cannot be woken because the queue is full! ");
        }
    }

    RawWakerVTable::new(clone_raw, wake, wake_by_ref, drop_raw)
};

pub struct Executor<F: 'static> {
    inner: &'static Inner<F>,
}

type Http1Connection = hyper::client::conn::http1::Connection<reactor::TcpStream, String>;
type Http2Connection =
    hyper::client::conn::http2::Connection<reactor::TcpStream, String, Executor<()>>;

type PinBoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct Inner<F> {
    io_resources: IoResources,
    filled: Mutex<Box<[bool]>>,
    futures: Box<[Mutex<Option<Task<F>>>]>,
    connections: Box<[Mutex<Slot<Http1Connection>>]>,
    http2_futures: Box<[Mutex<Slot<PinBoxFuture>>]>,
    queue: SimpleQueue<QueueIndex>,
    identifier: AtomicU32,
}

enum Slot<T> {
    Empty,
    Reserved,
    Occupied(T),
}

unsafe impl<F: Send> Send for Inner<F> {}
unsafe impl<F: Send> Sync for Inner<F> {}

#[repr(C)]
struct Task<F> {
    fut: F,
    identifier: u32,
}

static INNER: OnceLock<&()> = OnceLock::new();

impl<F, T> hyper::rt::Executor<F> for Executor<T>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        // TODO get from a thread-local
        let bucket_index = BucketIndex {
            index: 0,
            identifier: 0,
        };

        // make a future that just always returns unit
        let future = async move {
            let _ = future.await;
        };

        let pinned = Box::pin(future);

        let indices = self.inner.io_resources.http2_futures(bucket_index);
        let http2_future_start_index = indices.start;

        let lock = self.inner.http2_futures[indices]
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| Some((i, slot.try_lock().ok()?)))
            .find_map(|(i, mut slot)| match slot.deref_mut() {
                Slot::Empty => Some((i, slot)),
                Slot::Reserved | Slot::Occupied(_) => None,
            });

        let Some((i, mut slot)) = lock else {
            todo!("insufficient space for another http2 future");
        };

        *slot = Slot::Occupied(pinned);

        let http2_future_index = Http2FutureIndex {
            identifier: bucket_index.identifier,
            index: (http2_future_start_index + i) as u32,
        };

        let queue_index =
            QueueIndex::from_http2_future_index(self.inner.io_resources, http2_future_index);

        if self.inner.queue.enqueue(queue_index).is_err() {
            log::warn!("connection cannot be started because the queue is full! ");
        }
    }
}

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
        bucket_index: BucketIndex,
        host: &str,
        port: u16,
    ) -> std::io::Result<hyper::client::conn::http1::SendRequest<String>> {
        let stream = std::net::TcpStream::connect(format!("{}:{}", host, port)).unwrap();
        stream.set_nonblocking(true)?;

        let indices = self.inner.io_resources.http_connections(bucket_index);
        let connection_index = indices.start;

        let mut opt_connection_offset = None;

        for (i, slot) in self.inner.connections[indices].iter().enumerate() {
            let Ok(mut slot) = slot.try_lock() else {
                continue;
            };

            match slot.deref_mut() {
                Slot::Empty => *slot = Slot::Reserved,
                Slot::Reserved | Slot::Occupied(_) => continue,
            }

            opt_connection_offset = Some(i);
            break;
        }

        let Some(i) = opt_connection_offset else {
            todo!("no free connection slot");
        };

        let connection_index = ConnectionIndex {
            identifier: bucket_index.identifier,
            index: (connection_index + i) as u32,
        };

        let reactor = reactor::Reactor::get().unwrap();

        let queue_index =
            QueueIndex::from_connection_index(self.inner.io_resources, connection_index);
        let stream = reactor.register(queue_index, stream).unwrap();

        let (sender, conn) = hyper::client::conn::http1::handshake(stream).await.unwrap();

        *self.inner.connections[connection_index.index as usize]
            .try_lock()
            .unwrap() = Slot::Occupied(conn);

        if self.inner.queue.enqueue(queue_index).is_err() {
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
                let queue_capacity = bucket_count * io_resources.per_bucket();
                let futures = std::iter::repeat_with(|| Mutex::new(None))
                    .take(queue_capacity)
                    .collect();
                let filled = Mutex::new(std::iter::repeat(false).take(bucket_count).collect());

                let connection_count = bucket_count * io_resources.http_connections;
                let connections = std::iter::repeat_with(|| Mutex::new(Slot::Empty))
                    .take(connection_count)
                    .collect();

                let http2_future_count = bucket_count * io_resources.http2_futures;
                let http2_futures = std::iter::repeat_with(|| Mutex::new(Slot::Empty))
                    .take(http2_future_count)
                    .collect();

                let queue = SimpleQueue::with_capacity(queue_capacity);

                let inner = Inner {
                    io_resources,
                    filled,
                    futures,
                    connections,
                    http2_futures,
                    queue,
                    identifier: AtomicU32::new(0),
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

    pub fn try_claim(&self) -> Option<BucketIndex> {
        self.inner.try_claim()
    }

    pub fn execute(&self, index: BucketIndex, fut: F) {
        let task = Task {
            fut,
            identifier: index.identifier,
        };

        self.inner.set(index, task);

        let queue_index = QueueIndex::from_bucket_index(self.inner.io_resources, index);
        match self.inner.queue.enqueue(queue_index) {
            Ok(()) => (),
            Err(_) => unreachable!("we claimed a spot!"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct IoResources {
    pub tcp_streams: usize,
    pub http_connections: usize,
    pub http2_futures: usize,
}

impl IoResources {
    #[inline]
    const fn per_bucket(self) -> usize {
        self.tcp_streams + self.http_connections + self.http2_futures
    }

    const fn http_connections(self, bucket_index: BucketIndex) -> Range<usize> {
        let start = bucket_index.index as usize * self.http_connections;

        start..(start + self.http_connections)
    }

    const fn http2_futures(self, bucket_index: BucketIndex) -> Range<usize> {
        let start = bucket_index.index as usize * self.http2_futures;

        start..(start + self.http2_futures)
    }
}

impl Default for IoResources {
    fn default() -> Self {
        IoResources {
            tcp_streams: 1,
            http_connections: 0,
            http2_futures: 0,
        }
    }
}

impl<F> Inner<F>
where
    F: Future<Output = ()> + Send,
{
    fn run(&self) {
        loop {
            let queue_index = self.queue.blocking_dequeue();

            match IoIndex::from_index(self.io_resources, queue_index) {
                IoIndex::InputStream(bucket_index) => {
                    let mut task_mut = self.futures[bucket_index.index as usize].lock().unwrap();

                    let task = match task_mut.as_mut() {
                        None => continue,
                        Some(task) if task.identifier != queue_index.identifier => continue,
                        Some(inner) => inner,
                    };

                    let pinned = unsafe { Pin::new_unchecked(&mut task.fut) };

                    let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
                    let waker = unsafe { Waker::from_raw(raw_waker) };
                    let mut cx = Context::from_waker(&waker);

                    match pinned.poll(&mut cx) {
                        std::task::Poll::Ready(_) => {
                            // allright, future is done, free up its spot in the slab
                            log::info!("task {} is done", bucket_index.index);
                            drop(task_mut);
                            self.remove(queue_index.to_bucket_index(self.io_resources));
                        }
                        std::task::Poll::Pending => {
                            // keep the future in the slab
                        }
                    }
                }
                IoIndex::CustomStream(_) => todo!(),
                IoIndex::HttpConnection(connection_index) => {
                    let mut task_mut = self.connections[connection_index.index as usize]
                        .lock()
                        .unwrap();

                    let connection = match task_mut.deref_mut() {
                        Slot::Empty | Slot::Reserved => continue,
                        Slot::Occupied(inner) => inner,
                    };

                    let pinned = unsafe { Pin::new_unchecked(connection) };

                    let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
                    let waker = unsafe { Waker::from_raw(raw_waker) };
                    let mut cx = Context::from_waker(&waker);

                    match pinned.poll(&mut cx) {
                        std::task::Poll::Ready(_) => {
                            // drop(task_mut);
                        }
                        std::task::Poll::Pending => {
                            // keep the future in the slab
                        }
                    }
                }
                IoIndex::Http2Future(future_index) => {
                    let mut task_mut = self.http2_futures[future_index.index as usize]
                        .lock()
                        .unwrap();

                    let connection = match task_mut.deref_mut() {
                        Slot::Empty | Slot::Reserved => continue,
                        Slot::Occupied(inner) => inner,
                    };

                    let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
                    let waker = unsafe { Waker::from_raw(raw_waker) };
                    let mut cx = Context::from_waker(&waker);

                    match connection.as_mut().poll(&mut cx) {
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

    fn try_claim(&self) -> Option<BucketIndex> {
        let mut guard = self.filled.lock().unwrap();
        let index = guard.iter().position(|filled| !filled)?;

        guard[index] = true;

        let identifier = self.identifier.fetch_add(1, Ordering::Relaxed);
        let index = index as u32;

        Some(BucketIndex { identifier, index })
    }

    fn set(&self, index: BucketIndex, value: Task<F>) {
        let old = self.futures[index.index as usize]
            .try_lock()
            .unwrap()
            .replace(value);
        debug_assert!(old.is_none());
    }

    fn remove(&self, index: BucketIndex) {
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

        for slot in self.connections[self.io_resources.http_connections(index)].iter() {
            let mut slot = slot.try_lock().unwrap();

            // will drop/clean up the TCP connection
            *slot = Slot::Empty;
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
