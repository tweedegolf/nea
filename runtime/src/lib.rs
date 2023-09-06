// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use index::Http2FutureIndex;
use serde::Deserialize;
use std::{
    future::Future,
    ops::{DerefMut, Range},
    pin::Pin,
    sync::{
        atomic::{AtomicU32, AtomicU8, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
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

// type Http1Connection = hyper::client::conn::http1::Connection<reactor::TcpStream, String>;
type Http2Connection =
    hyper::client::conn::http2::Connection<reactor::TcpStream, String, HyperExecutor>;

type PinBoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[repr(transparent)]
#[derive(Debug)]
struct Refcount(AtomicU32);

impl Refcount {
    // the bucket is empty, and available to claim by a worker
    const EMPTY_U32: u32 = u32::MAX;
    const EMPTY: Self = Self(AtomicU32::new(Self::EMPTY_U32));

    // the bucket is claimed, but no futures are inserted yet
    const RESERVED_U32: u32 = Self::EMPTY_U32 - 1;

    fn try_reserve(&self) -> Result<(), ()> {
        match self.0.compare_exchange(
            Self::EMPTY_U32,
            Self::RESERVED_U32,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(()),
        }
    }

    fn claim(&self) {
        match self
            .0
            .compare_exchange(Self::RESERVED_U32, 0, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => {}
            Err(_) => {
                panic!("claiming a bucket that was not reserved!");
            }
        }
    }

    fn incref(&self) -> u32 {
        let new = self.0.fetch_add(1, Ordering::Relaxed) + 1;
        new
    }

    fn decref(&self) -> u32 {
        // value was one, so after subtracting 1, it will be zero, and we can free this slot
        let old1 = self.0.load(Ordering::Relaxed);
        let new = old1.saturating_sub(1);

        if new == 0 {
            let old2 = self.0.swap(Self::EMPTY_U32, Ordering::Relaxed);

            debug_assert_eq!(old1, old2);
        }

        new
    }
}

struct Inner<F> {
    io_resources: IoResources,
    refcounts: Box<[Refcount]>,
    futures: Box<[Mutex<Option<Task<F>>>]>,
    connections: Box<[Mutex<Slot<Http2Connection>>]>,
    http2_futures: Box<[Mutex<Slot<PinBoxFuture>>]>,
    // queue: SimpleQueue<QueueIndex>,
    queue: ComplexQueue,
}

#[derive(Debug)]
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

#[derive(Clone, Copy)]
struct HyperExecutor {
    bucket_index: BucketIndex,
}

impl<F> hyper::rt::Executor<F> for HyperExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        // NOTE: the unit here is a lie! we just don't know the correct type for the future here
        let executor = Executor::<()>::get().unwrap();

        // make a future that just always returns unit
        let future = async move {
            let _ = future.await;
        };

        let pinned = Box::pin(future);

        let indices = executor.inner.io_resources.http2_futures(self.bucket_index);
        let http2_future_start_index = indices.start;

        let lock = executor.inner.http2_futures[indices]
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| Some((i, slot.try_lock().ok()?)))
            .find_map(|(i, mut slot)| match slot.deref_mut() {
                Slot::Empty => Some((i, slot)),
                Slot::Reserved => None,
                Slot::Occupied(_) => None,
            });

        let Some((i, mut slot)) = lock else {
            todo!("insufficient space for another http2 future");
        };

        *slot = Slot::Occupied(pinned);

        let http2_future_index = Http2FutureIndex {
            identifier: self.bucket_index.identifier,
            index: (http2_future_start_index + i) as u32,
        };

        let queue_index =
            QueueIndex::from_http2_future_index(executor.inner.io_resources, http2_future_index);

        executor.inner.refcounts[self.bucket_index.index as usize].incref();

        if executor.inner.queue.enqueue(queue_index).is_err() {
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
    ) -> std::io::Result<hyper::client::conn::http2::SendRequest<String>> {
        println!("handshake for bucket {}", bucket_index.index);
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
                // Slot::Reserved | Slot::Occupied(_) => continue,
                Slot::Reserved => {
                    continue;
                }
                Slot::Occupied(_) => {
                    continue;
                }
            }

            opt_connection_offset = Some(i);
            break;
        }

        let Some(i) = opt_connection_offset else {
            todo!("no free connection slot for bucket {}", bucket_index.index);
        };

        let connection_index = ConnectionIndex {
            identifier: bucket_index.identifier,
            index: (connection_index + i) as u32,
        };

        let reactor = reactor::Reactor::get().unwrap();

        let queue_index =
            QueueIndex::from_connection_index(self.inner.io_resources, connection_index);
        let stream = reactor.register(queue_index, stream).unwrap();

        // let (sender, conn) = hyper::client::conn::http1::handshake(stream).await.unwrap();
        let hyper_executor = HyperExecutor { bucket_index };
        let (sender, conn) = hyper::client::conn::http2::handshake(hyper_executor, stream)
            .await
            .unwrap();

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
                let refcounts = std::iter::repeat_with(|| Refcount::EMPTY)
                    .take(bucket_count)
                    .collect();

                let queue_capacity = bucket_count * io_resources.per_bucket();
                let futures = std::iter::repeat_with(|| Mutex::new(None))
                    .take(queue_capacity)
                    .collect();

                let connection_count = bucket_count * io_resources.http_connections;
                let connections = std::iter::repeat_with(|| Mutex::new(Slot::Empty))
                    .take(connection_count)
                    .collect();

                let http2_future_count = bucket_count * io_resources.http2_futures;
                let http2_futures = std::iter::repeat_with(|| Mutex::new(Slot::Empty))
                    .take(http2_future_count)
                    .collect();

                let queue = ComplexQueue::with_capacity(queue_capacity);

                let inner = Inner {
                    io_resources,
                    refcounts,
                    futures,
                    connections,
                    http2_futures,
                    queue,
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

    const fn queue_slots(self, bucket_index: BucketIndex) -> Range<usize> {
        let start = bucket_index.index as usize * self.per_bucket();

        start..(start + self.per_bucket())
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

            loop {
                let io_index = IoIndex::from_index(self.io_resources, queue_index);

                let done_with_item = match io_index {
                    IoIndex::InputStream(bucket_index) => {
                        let poll = self.poll_input_stream(queue_index, bucket_index);

                        match poll {
                            Poll::Ready(()) => {
                                let new_rc =
                                    self.decref(queue_index.to_bucket_index(self.io_resources));

                                if new_rc != 0 {
                                    self.queue.done_with_item(queue_index)
                                } else {
                                    DoneWithItem::Done
                                }
                            }
                            Poll::Pending => {
                                // keep the future in the slab
                                self.queue.done_with_item(queue_index)
                            }
                        }
                    }
                    IoIndex::CustomStream(_) => todo!(),
                    IoIndex::HttpConnection(connection_index) => {
                        let poll = self.poll_http_connection(queue_index, connection_index);

                        match poll {
                            Poll::Ready(_) => self.queue.done_with_item(queue_index),
                            Poll::Pending => self.queue.done_with_item(queue_index),
                        }
                    }
                    IoIndex::Http2Future(future_index) => {
                        let poll = self.poll_http2_future(queue_index, future_index);

                        match poll {
                            Poll::Ready(()) => {
                                let new_rc =
                                    self.decref(queue_index.to_bucket_index(self.io_resources));

                                if new_rc != 0 {
                                    self.queue.done_with_item(queue_index)
                                } else {
                                    DoneWithItem::Done
                                }
                            }
                            Poll::Pending => {
                                // keep the future in the slab
                                self.queue.done_with_item(queue_index)
                            }
                        }
                    }
                };

                // the queue index may have been enqueued again while we processed it.
                match done_with_item {
                    DoneWithItem::Done => break,
                    DoneWithItem::GoAgain => continue,
                }
            }
        }
    }

    fn poll_input_stream(&self, queue_index: QueueIndex, bucket_index: BucketIndex) -> Poll<()> {
        let mut task_mut = self.futures[bucket_index.index as usize].lock().unwrap();

        let task = match task_mut.as_mut() {
            None => panic!("race condition"),
            Some(task) if task.identifier != queue_index.identifier => panic!(),
            Some(inner) => inner,
        };

        let pinned = unsafe { Pin::new_unchecked(&mut task.fut) };

        let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        match pinned.poll(&mut cx) {
            std::task::Poll::Ready(_) => {
                drop(task_mut);

                Poll::Ready(())
            }
            std::task::Poll::Pending => Poll::Pending,
        }
    }

    fn poll_http_connection(
        &self,
        queue_index: QueueIndex,
        connection_index: ConnectionIndex,
    ) -> Poll<()> {
        let mut task_mut = self.connections[connection_index.index as usize]
            .lock()
            .unwrap();

        let connection = match task_mut.deref_mut() {
            Slot::Empty | Slot::Reserved => panic!("race condition"),
            Slot::Occupied(inner) => inner,
        };

        let pinned = unsafe { Pin::new_unchecked(connection) };

        let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        match pinned.poll(&mut cx) {
            std::task::Poll::Ready(result) => {
                *task_mut = Slot::Empty;
                drop(task_mut);

                if let Err(e) = result {
                    log::warn!("error in http connection {e:?}");
                }

                Poll::Ready(())
            }
            std::task::Poll::Pending => Poll::Pending,
        }
    }

    fn poll_http2_future(
        &self,
        queue_index: QueueIndex,
        future_index: Http2FutureIndex,
    ) -> Poll<()> {
        let mut task_mut = self.http2_futures[future_index.index as usize]
            .lock()
            .unwrap();

        let connection = match task_mut.deref_mut() {
            Slot::Empty | Slot::Reserved => {
                log::warn!("http2 future in the queue, but there is no future");
                return Poll::Ready(());
            }
            Slot::Occupied(inner) => inner,
        };

        let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        match connection.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(_) => {
                *task_mut = Slot::Empty;
                drop(task_mut);

                Poll::Ready(())
            }
            std::task::Poll::Pending => Poll::Pending,
        }
    }

    fn try_claim(&self) -> Option<BucketIndex> {
        let guard = &self.refcounts;
        let index = guard.iter().position(|id| id.try_reserve().is_ok())?;

        guard[index].claim();

        let index = index as u32;
        let identifier = 0;
        Some(BucketIndex { identifier, index })
    }

    fn set(&self, index: BucketIndex, value: Task<F>) {
        let refcount = &self.refcounts[index.index as usize];
        assert_eq!(refcount.incref(), 1);

        let old = self.futures[index.index as usize]
            .try_lock()
            .unwrap()
            .replace(value);
        debug_assert!(old.is_none());
    }

    fn decref(&self, index: BucketIndex) -> u32 {
        let guard = &self.refcounts;
        let refcount = &guard[index.index as usize].0;

        // value was one, so after subtracting 1, it will be zero, and we can free this slot
        let old = refcount.fetch_sub(1, Ordering::Relaxed);
        if old == 1 {
            // handler is done, free up its spot in the slab
            log::info!("task {} is done", index.index);

            self.remove(index);
        }

        old.saturating_sub(1)
    }

    fn remove(&self, index: BucketIndex) {
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

        for slot in self.http2_futures[self.io_resources.http2_futures(index)].iter() {
            let mut slot = slot.try_lock().unwrap();

            // will drop/clean up the TCP connection
            *slot = Slot::Empty;
        }

        for slot in self.queue.queue[self.io_resources.queue_slots(index)].iter() {
            slot.store(0, Ordering::Relaxed)
        }

        self.refcounts[index.index as usize].decref();

        log::trace!("cleared index {}", index.index)
    }
}

const ENQUEUED_BIT: u8 = 1;
const IN_PROGRESS_BIT: u8 = 1 << 1;

struct ComplexQueue {
    queue: Box<[AtomicU8]>,
    index: AtomicUsize,
    // number of currently enqueued items
    active: AtomicU32,
}

#[derive(Debug)]
enum DoneWithItem {
    Done,
    GoAgain,
}

impl ComplexQueue {
    fn with_capacity(capacity: usize) -> Self {
        let queue = std::iter::repeat_with(|| AtomicU8::new(0))
            .take(capacity)
            .collect();

        Self {
            queue,
            index: AtomicUsize::new(0),
            active: AtomicU32::new(0),
        }
    }

    fn increment_active(&self) {
        let _ = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        atomic_wait::wake_one(&self.active);
    }

    fn decrement_active(&self) {
        // let _ = self.active.fetch_sub(1, Ordering::Relaxed) - 1;
        //
        let _ = self
            .active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_sub(1))
            });
    }

    fn wait_active(&self) {
        while self.active.load(Ordering::Relaxed) == 0 {
            atomic_wait::wait(&self.active, 0);
        }
    }

    fn enqueue(&self, index: QueueIndex) -> Result<(), QueueIndex> {
        let current = &self.queue[index.index as usize];

        let mut new_active = false;

        let added = current.fetch_update(Ordering::Acquire, Ordering::Relaxed, |value| {
            // if the slot is already in progress, don't increment the active count
            new_active = value & IN_PROGRESS_BIT == 0;

            Some(value | ENQUEUED_BIT)
        });

        if new_active {
            self.increment_active();
        }

        match added {
            Ok(_) => Ok(()),
            Err(_) => Err(index),
        }
    }

    #[must_use]
    fn done_with_item(&self, index: QueueIndex) -> DoneWithItem {
        let current = &self.queue[index.index as usize];

        let mut done_with_item = DoneWithItem::Done;

        let _ = current.fetch_update(Ordering::Acquire, Ordering::Relaxed, |value| {
            // this slot should be in progress
            debug_assert!(value & IN_PROGRESS_BIT != 0);

            if value & ENQUEUED_BIT != 0 {
                // this slot got enqueued while we were processing it
                // slot must be processed again
                done_with_item = DoneWithItem::GoAgain;
                Some(IN_PROGRESS_BIT)
            } else {
                Some(0)
            }
        });

        done_with_item
    }

    fn try_dequeue(&self) -> Option<QueueIndex> {
        // first, try to find something
        let index = self.index.load(Ordering::Relaxed);

        let (later, first) = self.queue.split_at(index);
        let it = first.iter().chain(later).enumerate();

        for (i, value) in it {
            let r = value.compare_exchange(
                ENQUEUED_BIT,
                IN_PROGRESS_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            );

            match r {
                Err(_) => continue,
                Ok(_) => {
                    let new_index = (index + i) % self.queue.len();

                    self.index.store(new_index, Ordering::Relaxed);

                    self.decrement_active();

                    return Some(QueueIndex {
                        index: new_index as u32,
                        identifier: 0,
                    });
                }
            }
        }

        None
    }

    fn blocking_dequeue(&self) -> QueueIndex {
        loop {
            self.wait_active();

            if let Some(index) = self.try_dequeue() {
                return index;
            }
        }
    }
}
