// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use std::{
    future::Future,
    ops::DerefMut,
    pin::Pin,
    sync::{atomic::Ordering, Mutex, MutexGuard, OnceLock},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

pub use crate::index::{BucketIndex, ConnectionIndex, Http2FutureIndex, IoIndex, QueueIndex};
use crate::{
    index::IoResources,
    queue::{ComplexQueue, DoneWithItem, IN_PROGRESS_BIT},
    reactor::{self, Reactor},
    ALLOCATOR, ARENA_INDEX_EXECUTOR,
};
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
        let thread = std::thread::current().id();

        let index = QueueIndex::from_ptr(ptr);
        log::warn!("{thread:?}: wake {}", index.index);

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

        if executor.inner.queue.enqueue(queue_index).is_err() {
            log::warn!("connection cannot be started because the queue is full! ");
        }
    }
}

type PinBoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct Inner<F> {
    /// How many queue slots are there per bucket
    io_resources: IoResources,
    /// number of active queue slots for each bucket
    refcounts: Box<[Mutex<u8>]>,
    futures: Box<[Mutex<Option<F>>]>,
    connections: Box<[Mutex<Option<Http2Connection>>]>,
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
                    .take(queue_capacity)
                    .collect();

                let connection_count = bucket_count * io_resources.http_connections;
                let connections = std::iter::repeat_with(|| Mutex::new(None))
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

    pub fn execute(&self, index: BucketIndex, fut: F) {
        self.inner.set(index, fut);

        let queue_index = QueueIndex::from_bucket_index(self.inner.io_resources, index);
        match self.inner.queue.enqueue(queue_index) {
            Ok(()) => (),
            Err(_) => unreachable!("we claimed a spot!"),
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

        {
            let mut bucket_guard = self.inner.refcounts[bucket_index.index as usize]
                .lock()
                .unwrap();

            if *bucket_guard == 0 {
                // this
                log::error!("main future is already done; dropping handshake");
                panic!("main future is already done; dropping handshake");
            } else {
                *bucket_guard += 1;
            }

            drop(bucket_guard);
        }

        for (i, slot) in self.inner.connections[indices].iter().enumerate() {
            let Ok(mut slot) = slot.try_lock() else {
                continue;
            };

            match slot.deref_mut() {
                None => *slot = None,
                Some(_) => continue,
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
        eprintln!("handshake");
        let stream = reactor.register(queue_index, stream).unwrap();

        // let (sender, conn) = hyper::client::conn::http1::handshake(stream).await.unwrap();
        let hyper_executor = HyperExecutor { bucket_index };
        let (sender, conn) = hyper::client::conn::http2::handshake(hyper_executor, stream)
            .await
            .unwrap();

        *self.inner.connections[connection_index.index as usize]
            .try_lock()
            .unwrap() = Some(conn);

        if self.inner.queue.enqueue(queue_index).is_err() {
            log::warn!("connection cannot be started because the queue is full! ");
        }

        Ok(sender)
    }
}

impl<F> Inner<F>
where
    F: Future<Output = ()> + Send,
{
    fn run_worker(&self) -> ! {
        'outer: loop {
            let (queue_index, mut queue_guard) = self.queue.blocking_dequeue();

            // configure the allocator
            let bucket_index = queue_index.to_bucket_index(self.io_resources);
            CURRENT_ARENA.with(|a| a.store(bucket_index.index, Ordering::Release));

            let io_index = IoIndex::from_index(self.io_resources, queue_index);
            match io_index {
                IoIndex::InputStream(bucket_index) => {
                    'enqueued_while_processing: loop {
                        match self.poll_input_stream(queue_index, bucket_index) {
                            Poll::Ready(()) => {
                                {
                                    let mut future_guard = self.futures
                                        [bucket_index.index as usize]
                                        .try_lock()
                                        .unwrap();

                                    // this future is now done
                                    let old = future_guard.take();
                                    assert!(old.is_some());

                                    drop(old);

                                    drop(future_guard);
                                }

                                // must block if there is a race condition with the executor trying to find a free slot
                                let mut bucket_guard =
                                    self.refcounts[bucket_index.index as usize].lock().unwrap();

                                log::warn!(
                                    "1. 💀 job {} (rc now {})",
                                    queue_index.index,
                                    bucket_guard.saturating_sub(1),
                                );

                                // this queue index is now done

                                if *bucket_guard == 1 {
                                    // clear the memory
                                    ALLOCATOR.0.clear_bucket(bucket_index.index as usize);

                                    let _ = self.queue.done_with_item_forever(&mut queue_guard);

                                    *bucket_guard = 0;
                                } else {
                                    *bucket_guard -= 1;

                                    assert!(matches!(
                                        self.queue.done_with_item(&mut queue_guard),
                                        DoneWithItem::Done
                                    ));
                                }

                                *queue_guard &= !IN_PROGRESS_BIT;
                                continue 'outer;
                            }
                            Poll::Pending => {
                                //
                                match self.queue.done_with_item(&mut queue_guard) {
                                    DoneWithItem::Done => {
                                        *queue_guard &= !IN_PROGRESS_BIT;
                                        break 'enqueued_while_processing;
                                    }
                                    DoneWithItem::GoAgain => {
                                        continue 'enqueued_while_processing;
                                    }
                                }
                            }
                        }
                    }
                }
                IoIndex::CustomStream(_) => todo!(),
                IoIndex::HttpConnection(connection_index) => {
                    'enqueued_while_processing: loop {
                        match self.poll_http_connection(queue_index, connection_index) {
                            Poll::Ready(()) => {
                                // NOTE: the future is already dropped by poll_http_connection

                                // must block if there is a race condition with the executor trying to find a free slot
                                let mut bucket_guard =
                                    self.refcounts[bucket_index.index as usize].lock().unwrap();

                                log::warn!(
                                    "2. 💀 job {} (rc now {})",
                                    queue_index.index,
                                    bucket_guard.saturating_sub(1),
                                );

                                // this queue index is now done

                                if *bucket_guard == 1 {
                                    // clear the memory
                                    ALLOCATOR.0.clear_bucket(bucket_index.index as usize);

                                    let _ = self.queue.done_with_item_forever(&mut queue_guard);

                                    *bucket_guard = 0;
                                } else {
                                    *bucket_guard -= 1;

                                    assert!(matches!(
                                        self.queue.done_with_item(&mut queue_guard),
                                        DoneWithItem::Done
                                    ));
                                }

                                *queue_guard &= !IN_PROGRESS_BIT;
                                continue 'outer;
                            }
                            Poll::Pending => {
                                //
                                match self.queue.done_with_item(&mut queue_guard) {
                                    DoneWithItem::Done => {
                                        *queue_guard &= !IN_PROGRESS_BIT;
                                        break 'enqueued_while_processing;
                                    }
                                    DoneWithItem::GoAgain => {
                                        continue 'enqueued_while_processing;
                                    }
                                }
                            }
                        }
                    }
                }
                IoIndex::Http2Future(future_index) => {
                    'enqueued_while_processing: loop {
                        match self.poll_http2_future(queue_index, future_index) {
                            Poll::Ready(()) => {
                                // NOTE: the future is already dropped by poll_http2_future

                                // must block if there is a race condition with the executor trying to find a free slot
                                let mut bucket_guard =
                                    self.refcounts[bucket_index.index as usize].lock().unwrap();

                                log::warn!(
                                    "3. 💀 job {} (rc now {})",
                                    queue_index.index,
                                    bucket_guard.saturating_sub(1),
                                );

                                // this queue index is now done
                                match *bucket_guard {
                                    0 => {
                                        // for some reason this can happen with http2 futures
                                    }
                                    1 => {
                                        self.queue.done_with_item(&mut queue_guard);
                                        // clear the memory
                                        ALLOCATOR.0.clear_bucket(bucket_index.index as usize);

                                        let _ = self.queue.done_with_item_forever(&mut queue_guard);

                                        *bucket_guard = 0;
                                    }
                                    _ => {
                                        *bucket_guard -= 1;

                                        assert!(matches!(
                                            self.queue.done_with_item(&mut queue_guard),
                                            DoneWithItem::Done
                                        ));
                                    }
                                };

                                *queue_guard &= !IN_PROGRESS_BIT;
                                continue 'outer;
                            }
                            Poll::Pending => {
                                //
                                match self.queue.done_with_item(&mut queue_guard) {
                                    DoneWithItem::Done => {
                                        *queue_guard &= !IN_PROGRESS_BIT;
                                        break 'enqueued_while_processing;
                                    }
                                    DoneWithItem::GoAgain => {
                                        continue 'enqueued_while_processing;
                                    }
                                }
                            }
                        }
                    }
                }
            };
        }
    }

    fn poll_input_stream(&self, queue_index: QueueIndex, bucket_index: BucketIndex) -> Poll<()> {
        let mut task_mut = self.futures[bucket_index.index as usize]
            .try_lock()
            .unwrap();

        let fut = match task_mut.as_mut() {
            None => panic!("race condition"),
            Some(inner) => inner,
        };

        let pinned = unsafe { Pin::new_unchecked(fut) };

        let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        pinned.poll(&mut cx)
    }

    fn poll_http_connection(
        &self,
        queue_index: QueueIndex,
        connection_index: ConnectionIndex,
    ) -> Poll<()> {
        let mut task_mut = self.connections[connection_index.index as usize]
            .lock()
            .unwrap();

        let Some(connection) = task_mut.deref_mut() else {
            panic!("race condition")
        };

        let pinned = unsafe { Pin::new_unchecked(connection) };

        let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        match pinned.poll(&mut cx) {
            std::task::Poll::Ready(result) => {
                let Some(connection) = task_mut.take() else {
                    panic!("no connection");
                };

                // only stores PhandomData<TcpStream>. the actual stream is not in here
                drop(connection);

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

        let Some(connection) = task_mut.deref_mut() else {
            log::error!("http2 future in the queue, but there is no future");
            return Poll::Ready(());
        };

        let raw_waker = RawWaker::new(queue_index.to_ptr(), &RAW_WAKER_V_TABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        match connection.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(_) => {
                let Some(fut) = task_mut.take() else {
                    panic!("no http2 future");
                };

                // this _should_ close the underlying TCP stream
                std::mem::forget(fut);

                Poll::Ready(())
            }
            std::task::Poll::Pending => Poll::Pending,
        }
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
        Some(BucketIndex { identifier, index })
    }
}
