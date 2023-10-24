// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use std::{
    future::Future,
    ops::DerefMut,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex, MutexGuard, OnceLock,
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

pub use crate::index::{BucketIndex, ConnectionIndex, Http2FutureIndex, IoIndex, QueueIndex};
use crate::{
    index::IoResources,
    queue::{ComplexQueue, DoneWithItem, IN_PROGRESS_BIT},
    reactor, ALLOCATOR, ARENA_INDEX_EXECUTOR,
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

// type Http2Connection = hyper::client::conn::http2::Connection<reactor::TcpStream, String, HyperExecutor>;

type PinBoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct Inner<F> {
    io_resources: IoResources,
    //    refcounts: Box<[Refcount]>,
    futures: Box<[Mutex<Option<F>>]>,
    //    connections: Box<[Mutex<Slot<Http2Connection>>]>,
    //    http2_futures: Box<[Mutex<Slot<PinBoxFuture>>]>,
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

                let queue = ComplexQueue::with_capacity(queue_capacity);

                let inner = Inner {
                    io_resources,
                    futures,
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
                                // must block if there is a race condition with the executor trying to find a free slot
                                let mut future_guard =
                                    self.futures[bucket_index.index as usize].lock().unwrap();

                                // clear the memory
                                ALLOCATOR.0.clear_bucket(bucket_index.index as usize);

                                // this future is now done
                                let old = future_guard.take();
                                assert!(old.is_some());

                                drop(old);

                                let _ = self.queue.done_with_item_forever(&mut queue_guard);

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
                IoIndex::HttpConnection(_) => todo!(),
                IoIndex::Http2Future(_) => todo!(),
            };
        }
    }

    fn poll_input_stream(&self, queue_index: QueueIndex, bucket_index: BucketIndex) -> Poll<()> {
        // must block if there is a race condition with the executor trying to find a free slot
        let mut task_mut = self.futures[bucket_index.index as usize].lock().unwrap();

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

    fn set(&self, index: BucketIndex, fut: F) {
        let old_value = self.futures[index.index as usize]
            .try_lock()
            .unwrap()
            .replace(fut);

        assert!(old_value.is_none())
    }

    fn try_claim(&self) -> Option<BucketIndex> {
        let bucket_index = self.futures.iter().position(|f| match f.try_lock() {
            Ok(slot) => slot.is_none(),
            Err(_) => false,
        })?;

        let index = bucket_index as u32;
        let identifier = 0;
        Some(BucketIndex { identifier, index })
    }
}
