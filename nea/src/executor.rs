// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use std::{
    future::Future,
    ops::DerefMut,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex, OnceLock,
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

pub use crate::index::{BucketIndex, ConnectionIndex, Http2FutureIndex, IoIndex, QueueIndex};
use crate::{
    index::IoResources,
    queue::{ComplexQueue, NextStep},
    reactor, ARENA_INDEX_EXECUTOR,
};
use crate::{ALLOCATOR, CURRENT_ARENA};

use shared::setjmp_longjmp::setjmp;

pub(crate) fn waker_for(queue_index: QueueIndex) -> Waker {
    let raw_waker = std::task::RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
    unsafe { Waker::from_raw(raw_waker) }
}

pub(crate) const RAW_WAKER_V_TABLE: RawWakerVTable = {
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
        let thread = std::thread::current().id();

        let index = QueueIndex::from_ptr(ptr);
        log::trace!("{thread:?}: wake {}", index.index);

        // NOTE: the unit here is a lie! we just don't know the correct type for the future here
        let executor = Executor::<()>::get().unwrap();

        executor.inner.queue.wake(index);

        log::trace!("{thread:?}: woke {}", index.index);
    }

    RawWakerVTable::new(clone_raw, wake, wake_by_ref, drop_raw)
};

pub struct Executor<F: 'static> {
    inner: &'static Inner<F>,
}

type Http1Connection = hyper::client::conn::http1::Connection<reactor::TcpStream, String>;

#[allow(unused)]
type Http2Connection =
    hyper::client::conn::http2::Connection<reactor::TcpStream, String, HyperExecutor>;

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
                None => Some((i, slot)),
                Some(_) => None,
            });

        let Some((i, mut slot)) = lock else {
            todo!("insufficient space for another http2 future");
        };

        // may race with a worker finishing a job
        let mut bucket_guard = executor.inner.refcounts[self.bucket_index.index as usize]
            .lock()
            .unwrap();
        assert!(*bucket_guard > 0);
        *bucket_guard += 1;

        *slot = Some(pinned);

        let http2_future_index = Http2FutureIndex {
            identifier: self.bucket_index.identifier,
            index: (http2_future_start_index + i) as u32,
        };

        let queue_index =
            QueueIndex::from_http2_future_index(executor.inner.io_resources, http2_future_index);

        executor.inner.queue.initial_enqueue(queue_index);
    }
}

type PinBoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct Inner<F> {
    /// How many queue slots are there per bucket
    io_resources: IoResources,
    /// number of active queue slots for each bucket
    refcounts: Box<[Mutex<u8>]>,
    futures: Box<[Mutex<Option<F>>]>,
    connections: Box<[Mutex<Slot<Http1Connection>>]>,
    http2_futures: Box<[Mutex<Option<PinBoxFuture>>]>,
    queue: crate::queue::ComplexQueue,
}

static INNER: OnceLock<&()> = OnceLock::new();

impl<F> Executor<F>
where
    F: Future<Output = ()> + Send + 'static,
{
    pub fn get_or_init(bucket_count: usize, io_resources: IoResources) -> Self {
        CURRENT_ARENA.with(|a| a.store(ARENA_INDEX_EXECUTOR, Ordering::Relaxed));

        match INNER.get() {
            None => {
                let queue_capacity = bucket_count * io_resources.per_bucket();
                let futures = std::iter::repeat_with(|| Mutex::new(None))
                    .take(queue_capacity)
                    .collect();

                let refcounts = std::iter::repeat_with(|| Mutex::new(0))
                    .take(bucket_count)
                    .collect();

                let connection_count = bucket_count * io_resources.http_connections;
                let connections = std::iter::repeat_with(|| Mutex::new(Slot::Empty))
                    .take(connection_count)
                    .collect();

                let http2_future_count = bucket_count * io_resources.http2_futures;
                let http2_futures = std::iter::repeat_with(|| Mutex::new(None))
                    .take(http2_future_count)
                    .collect();

                let queue = ComplexQueue::with_capacity(queue_capacity);

                let inner = Inner {
                    io_resources,
                    futures,
                    refcounts,
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
            .spawn(|| inner.run_worker())
    }

    pub fn try_claim(&self) -> Option<BucketIndex> {
        self.inner.try_claim()
    }

    pub fn execute(&self, bucket_index: BucketIndex, fut: F) {
        self.inner.set(bucket_index, fut);

        let queue_index = QueueIndex::from_bucket_index(self.inner.io_resources, bucket_index);

        let range = self.inner.io_resources.queue_slots(bucket_index);
        assert!(self.inner.queue.is_range_empty(range));

        self.inner.queue.initial_enqueue(queue_index);
    }
}

enum Slot<T> {
    Empty,
    Reserved,
    Occupied(T),
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
    ) -> std::io::Result<hyper::client::conn::http1::SendRequest<String>> {
        let stream = std::net::TcpStream::connect(format!("{}:{}", host, port)).unwrap();
        stream.set_nonblocking(true)?;

        let indices = self.inner.io_resources.http_connections(bucket_index);
        let connection_index = indices.start;

        {
            let mut bucket_guard = self.inner.refcounts[bucket_index.index as usize]
                .lock()
                .unwrap();

            if *bucket_guard == 0 {
                log::warn!("main future is already done; dropping handshake");
            } else {
                *bucket_guard += 1;
            }

            drop(bucket_guard);
        }

        let opt_connection_offset = 'blk: {
            for (i, slot) in self.inner.connections[indices].iter().enumerate() {
                let Ok(mut slot) = slot.try_lock() else {
                    continue;
                };

                match slot.deref_mut() {
                    Slot::Reserved | Slot::Occupied(_) => {
                        continue;
                    }
                    Slot::Empty => {
                        *slot = Slot::Reserved;
                        break 'blk Some(i);
                    }
                }
            }

            None
        };

        let Some(i) = opt_connection_offset else {
            todo!("no free connection slot for bucket {}", bucket_index.index);
        };

        static CONNECTION_IDENTIFIER: AtomicU32 = AtomicU32::new(0);

        let identifier = CONNECTION_IDENTIFIER.fetch_add(1, Ordering::Relaxed);

        let connection_index = ConnectionIndex {
            identifier,
            index: (connection_index + i) as u32,
        };

        let reactor = reactor::Reactor::get().unwrap();

        let queue_index =
            QueueIndex::from_connection_index(self.inner.io_resources, connection_index);
        let stream = reactor.register(queue_index, stream).unwrap();

        let hyper_executor = HyperExecutor { bucket_index };
        let (sender, conn) = hyper::client::conn::http1::handshake(stream).await.unwrap();

        let mut connection_lock = self.inner.connections[connection_index.index as usize]
            .try_lock()
            .unwrap();

        assert!(matches!(*connection_lock, Slot::Reserved));
        *connection_lock = Slot::Occupied(conn);

        let thread = std::thread::current().id();
        log::info!("{thread:?}: connection {} populated", queue_index.index);

        drop(connection_lock);

        self.inner.queue.initial_enqueue(queue_index);

        Ok(sender)
    }
}

enum HyperWake {
    /// The task is now finished
    Legit,
    /// This task is already done
    AfterDeath,
}

impl<F> Inner<F>
where
    F: Future<Output = ()> + Send,
{
    fn run_worker(&self) -> ! {
        'outer: loop {
            let (queue_index, queue_slot) = self.queue.blocking_dequeue();

            // configure the allocator
            let bucket_index = queue_index.to_bucket_index(self.io_resources);
            CURRENT_ARENA.with(|a| a.store(bucket_index.index, Ordering::Release));

            match crate::JMP_BUFFER.with(|jmp_buffer| unsafe { setjmp(jmp_buffer.get()) }) {
                shared::setjmp_longjmp::SetJmp::Called => { /* fall through */ }
                shared::setjmp_longjmp::SetJmp::Jumped(_) => {
                    /* the application went Out Of Memory */
                }
            };

            let io_index = IoIndex::from_index(self.io_resources, queue_index);

            'enqueued_while_processing: loop {
                let poll_result = match io_index {
                    IoIndex::InputStream(bucket_index) => {
                        self.poll_input_stream(queue_index, bucket_index)
                    }
                    IoIndex::CustomStream(_) => todo!(),
                    IoIndex::HttpConnection(connection_index) => {
                        match self.poll_http_connection(queue_index, connection_index) {
                            Poll::Ready(HyperWake::Legit) => Poll::Ready(()),
                            Poll::Ready(HyperWake::AfterDeath) => {
                                // destructors in the main future (polled by poll_input_stream) can cause
                                // a wake of this task. But often this task is already complete, and there
                                // is nothing sensible to do.
                                let _ = self.queue.done_with_item(queue_slot);
                                continue 'outer;
                            }
                            Poll::Pending => Poll::Pending,
                        }
                    }
                    IoIndex::Http2Future(future_index) => {
                        self.poll_http2_future(queue_index, future_index)
                    }
                };

                match poll_result {
                    Poll::Ready(()) => {
                        // NOTE: the future is already dropped by the polling functions

                        // must block if there is a race condition with the executor trying to find a free slot
                        let mut bucket_guard =
                            self.refcounts[bucket_index.index as usize].lock().unwrap();

                        // this slot is now done (until we put a new connection into it). To
                        // prevent hyper from waking it again (it will do that), invalidate the
                        // identifier so that the `wake` function on queue will always reject it
                        queue_slot.id.fetch_sub(1, Ordering::Relaxed);

                        log::debug!(
                            "💀 job {} (rc now {})",
                            queue_index.index,
                            bucket_guard.saturating_sub(1),
                        );

                        // assert!(*bucket_guard != 0);

                        if *bucket_guard == 1 {
                            // clear the memory
                            ALLOCATOR.0.clear_bucket(bucket_index.index as usize);

                            *bucket_guard = 0;

                            queue_slot.mark_empty();

                            continue 'outer;
                        } else {
                            *bucket_guard -= 1;

                            // even if the task is enqueue'd again, there is nothing to do
                            queue_slot.mark_empty();

                            continue 'outer;
                        }
                    }
                    Poll::Pending => match self.queue.done_with_item(queue_slot) {
                        NextStep::Done => {
                            continue 'outer;
                        }
                        NextStep::GoAgain => {
                            continue 'enqueued_while_processing;
                        }
                    },
                }
            }
        }
    }

    fn poll_input_stream(&self, queue_index: QueueIndex, bucket_index: BucketIndex) -> Poll<()> {
        let mut task_mut = self.futures[bucket_index.index as usize]
            .try_lock()
            .unwrap();

        let fut = match task_mut.as_mut() {
            None => panic!("input stream race condition"),
            Some(inner) => inner,
        };

        let pinned = unsafe { Pin::new_unchecked(fut) };

        let waker = waker_for(queue_index);
        let mut cx = Context::from_waker(&waker);

        // early return if pending
        std::task::ready!(pinned.poll(&mut cx));

        // this future is now done
        let old = task_mut.take();
        assert!(old.is_some());

        Poll::Ready(())
    }

    fn poll_http_connection(
        &self,
        queue_index: QueueIndex,
        connection_index: ConnectionIndex,
    ) -> Poll<HyperWake> {
        let thread = std::thread::current().id();

        let mut task_mut = self.connections[connection_index.index as usize]
            .lock()
            .unwrap();

        // for some reason, Hyper wakes tasks that are already already complete. We can't have that!
        let connection = match task_mut.deref_mut() {
            Slot::Empty => return Poll::Ready(HyperWake::AfterDeath),
            Slot::Reserved => panic!("{thread:?}: race condition reserved"),
            Slot::Occupied(connection) => connection,
        };

        let pinned = unsafe { Pin::new_unchecked(connection) };
        let waker = waker_for(queue_index);
        let mut cx = Context::from_waker(&waker);

        // early return if pending
        let result = std::task::ready!(pinned.poll(&mut cx));

        let Slot::Occupied(connection) = std::mem::replace(task_mut.deref_mut(), Slot::Empty)
        else {
            panic!("no connection");
        };

        log::debug!("{thread:?}: {} cleared", queue_index.index);

        // only stores PhandomData<TcpStream>. the actual stream is not in here
        drop(connection);

        if let Err(e) = result {
            log::warn!("error in http connection {e:?}");
        }

        Poll::Ready(HyperWake::Legit)
    }

    fn poll_http2_future(
        &self,
        queue_index: QueueIndex,
        future_index: Http2FutureIndex,
    ) -> Poll<()> {
        let mut task_mut = self.http2_futures[future_index.index as usize]
            .lock()
            .unwrap();

        let Some(connection) = task_mut.deref_mut() else {
            log::error!("http2 future in the queue, but there is no future");
            return Poll::Ready(());
        };

        let waker = waker_for(queue_index);
        let mut cx = Context::from_waker(&waker);

        // early return when pending
        std::task::ready!(connection.as_mut().poll(&mut cx));

        let Some(fut) = task_mut.take() else {
            panic!("no http2 future");
        };

        // this _should_ close the underlying TCP stream
        std::mem::drop(fut);

        Poll::Ready(())
    }

    fn set(&self, index: BucketIndex, fut: F) {
        let old_value = self.futures[index.index as usize]
            .try_lock()
            .unwrap()
            .replace(fut);

        // NOTE try_claim already set the refcount of this slot to 1

        assert!(old_value.is_none())
    }

    fn try_claim(&self) -> Option<BucketIndex> {
        let bucket_index = self.refcounts.iter().position(|f| match f.try_lock() {
            Ok(mut slot) => {
                if *slot == 0 {
                    *slot = 1;
                    true
                } else {
                    false
                }
            }
            Err(_) => false,
        })?;

        let index = bucket_index as u32;
        let identifier = 0;
        let bucket_index = BucketIndex { identifier, index };

        Some(bucket_index)
    }
}
