// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use std::{
    future::Future,
    ops::DerefMut,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError,
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

pub use crate::index::{BucketIndex, ConnectionIndex, Http2FutureIndex, IoIndex, QueueIndex};
use crate::{index::IoResources, reactor, ALLOCATOR, ARENA_INDEX_EXECUTOR};
use crate::{ARENA_INDEX_UNINITIALIZED, CURRENT_ARENA};

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

    #[allow(unused)]
    pub async fn handshake(
        &self,
        bucket_index: BucketIndex,
        host: &str,
        port: u16,
    ) -> std::io::Result<hyper::client::conn::http2::SendRequest<String>> {
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
    pub fn get_or_init(bucket_count: usize, io_resources: IoResources) -> Self {
        CURRENT_ARENA.with(|a| a.store(ARENA_INDEX_EXECUTOR, Ordering::Relaxed));

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

impl<F> Inner<F>
where
    F: Future<Output = ()> + Send,
{
    fn run(&self) {
        let thread = std::thread::current().id();

        'outer: loop {
            log::warn!("{thread:?}: top of the loop",);
            let (queue_index, mut guard) = self.queue.blocking_dequeue();
            log::warn!("{thread:?}: found new job {}", queue_index.index);

            // configure the
            {
                let bucket_index = queue_index.to_bucket_index(self.io_resources);
                CURRENT_ARENA.with(|a| a.store(bucket_index.index, Ordering::Release));

                log::warn!(
                    "{thread:?}: arena index is now {}",
                    CURRENT_ARENA.with(|x| x.load(Ordering::Relaxed))
                );
            }

            {
                let io_index = IoIndex::from_index(self.io_resources, queue_index);

                // if this job get enqueue'd again while we're processing it, we just continue
                // processing it on this worker thread.
                'current_job: loop {
                    let is_reference_counted;
                    let poll = match io_index {
                        IoIndex::InputStream(bucket_index) => {
                            is_reference_counted = true;
                            self.poll_input_stream(queue_index, bucket_index)
                        }
                        IoIndex::CustomStream(_) => todo!(),
                        IoIndex::HttpConnection(connection_index) => {
                            is_reference_counted = false;
                            self.poll_http_connection(queue_index, connection_index)
                        }
                        IoIndex::Http2Future(future_index) => {
                            is_reference_counted = true;
                            self.poll_http2_future(queue_index, future_index)
                        }
                    };

                    let action = if is_reference_counted {
                        match poll {
                            Poll::Ready(()) => {
                                let bucket_index = queue_index.to_bucket_index(self.io_resources);
                                let current_refcount = self.getref(bucket_index);

                                match current_refcount {
                                    1 => {
                                        assert_eq!(*guard, IN_PROGRESS_BIT, "must be in progress");

                                        *guard = 0;

                                        // we're done with this arena now, it can be free'd
                                        let bucket_index =
                                            queue_index.to_bucket_index(self.io_resources);
                                        ALLOCATOR.0.clear_bucket(bucket_index.index as usize);

                                        {
                                            CURRENT_ARENA.with(|a| {
                                                a.store(
                                                    ARENA_INDEX_UNINITIALIZED,
                                                    Ordering::Release,
                                                )
                                            });
                                            let thread = std::thread::current().id();
                                            log::warn!(
                                                "{thread:?}: ---------- {} is done forever",
                                                queue_index.index
                                            );
                                        }

                                        assert_eq!(self.decref(bucket_index), 0);

                                        continue 'outer;
                                    }
                                    _ => match self.queue.done_with_item(&mut guard) {
                                        DoneWithItem::Done => {
                                            break 'current_job;
                                        }
                                        DoneWithItem::GoAgain => {
                                            // do NOT decrement the refcount
                                            continue 'current_job;
                                        }
                                    },
                                }
                            }
                            Poll::Pending => {
                                // keep the future in the slab
                                self.queue.done_with_item(&mut guard)
                            }
                        }
                    } else {
                        self.queue.done_with_item(&mut guard)
                    };

                    match action {
                        DoneWithItem::Done => break,
                        DoneWithItem::GoAgain => continue,
                    }
                }
            }

            {
                let thread = std::thread::current().id();

                CURRENT_ARENA.with(|a| a.store(ARENA_INDEX_UNINITIALIZED, Ordering::Release));

                log::warn!("{thread:?}: mark {} as done for now", queue_index.index);

                *guard &= !IN_PROGRESS_BIT;

                log::warn!("{thread:?}: {} is done for now", queue_index.index);
            }

            // only now give up the guard
            drop(guard);
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
        assert!(index < 1000);
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

    fn getref(&self, index: BucketIndex) -> u32 {
        (self.refcounts[index.index as usize].0).load(Ordering::Relaxed)
    }

    fn decref(&self, index: BucketIndex) -> u32 {
        let refcount = &self.refcounts[index.index as usize].0;

        // value was one, so after subtracting 1, it will be zero, and we can free this slot
        let old = refcount.fetch_sub(1, Ordering::Relaxed);
        if old == 1 {
            // handler is done, free up its spot in the slab
            // TODO here
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
            // let old = slot.swap(0, Ordering::Relaxed);
            let old = std::mem::take(slot.lock().unwrap().deref_mut());

            assert_eq!(old, 0, "old {old:b}");
        }

        self.refcounts[index.index as usize].decref();

        log::trace!("cleared index {}", index.index)
    }
}

const ENQUEUED_BIT: u8 = 0b0001;
const IN_PROGRESS_BIT: u8 = 0b0010;

struct ComplexQueue {
    queue: Box<[Mutex<u8>]>,
    index: AtomicUsize,
    // number of currently enqueued items
    jobs_in_queue: Refcount1,
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

    fn wait_active(&self) {
        let mut guard = self.active_mutex.lock().unwrap();
        while *guard == 0 {
            guard = self.active_condvar.wait(guard).unwrap();
        }
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
            let thread = std::thread::current().id();
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
            let thread = std::thread::current().id();
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
enum DoneWithItem {
    Done,
    GoAgain,
}

impl ComplexQueue {
    fn with_capacity(capacity: usize) -> Self {
        let queue = std::iter::repeat_with(|| Mutex::new(0))
            .take(capacity)
            .collect();

        Self {
            queue,
            index: AtomicUsize::new(0),
            jobs_in_queue: Refcount1::new(),
        }
    }

    fn increment_active(&self) {
        self.jobs_in_queue.increment_active()
    }

    fn decrement_active(&self) {
        self.jobs_in_queue.decrement_active()
    }

    fn wait_active(&self) {
        self.jobs_in_queue.wait_active()
    }

    fn enqueue(&self, index: QueueIndex) -> Result<(), QueueIndex> {
        let current = &self.queue[index.index as usize];

        // eagerly increment (but don't wake anyone yet)
        self.increment_active();

        // mark this slot as enqueue'd
        let mut guard = current.try_lock().unwrap();

        // let old_value = current.fetch_or(ENQUEUED_BIT, Ordering::Relaxed);
        let old_value = *guard;
        *guard |= ENQUEUED_BIT;

        if old_value & ENQUEUED_BIT == 0 && old_value & IN_PROGRESS_BIT == 0 {
            // this is a new job, notify a worker
            self.jobs_in_queue.wake_one();
        } else {
            // the job was already enqueue'd, revert
            self.decrement_active();
        }

        let thread = std::thread::current().id();
        log::warn!("{thread:?}: ++++++++++ job {} is in the queue", index.index);

        Ok(())
    }

    #[must_use]
    fn done_with_item(&self, guard: &mut MutexGuard<u8>) -> DoneWithItem {
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

    fn try_dequeue(&self) -> Option<(QueueIndex, MutexGuard<u8>)> {
        // first, try to find something
        let index = self.index.load(Ordering::Relaxed);

        let (later, first) = self.queue.split_at(index);
        let it = first.iter().chain(later).enumerate();

        for (_, value) in it {
            // find a task that is enqueued but not yet in progress
            let mut guard = match value.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Poisoned(e)) => panic!("{e:?}"),
            };

            if *guard != ENQUEUED_BIT {
                continue;
            }

            *guard = IN_PROGRESS_BIT;

            // let new_index = (index + i) % self.queue.len();
            let new_index = 0;

            let thread = std::thread::current().id();
            log::warn!("{thread:?}: picked {new_index}");

            self.index.store(new_index, Ordering::Relaxed);

            self.decrement_active();

            let index = QueueIndex {
                index: new_index as u32,
                identifier: 0,
            };

            return Some((index, guard));
        }

        None
    }

    fn blocking_dequeue(&self) -> (QueueIndex, MutexGuard<u8>) {
        let thread = std::thread::current().id();

        loop {
            self.wait_active();
            log::warn!("{thread:?}: done waiting");

            if let Some((index, guard)) = self.try_dequeue() {
                return (index, guard);
            }
        }
    }
}
