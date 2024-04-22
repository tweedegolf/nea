// https://github.com/Hexilee/async-io-demo/blob/master/src/executor.rs
// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use std::{
    future::Future,
    ops::DerefMut,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    pin::Pin,
    sync::{
        atomic::{AtomicI32, AtomicU32, Ordering},
        Mutex, MutexGuard, OnceLock, TryLockError, TryLockResult,
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use shared::setjmp_longjmp::{setjmp, JumpBuf};

pub use crate::index::{BucketIndex, ConnectionIndex, Http2FutureIndex, IoIndex, QueueIndex};
use crate::{
    index::IoResources,
    queue::{ComplexQueue, NextStep, QueueSlot},
    reactor, ARENA_INDEX_EXECUTOR,
};
use crate::{ALLOCATOR, CURRENT_ARENA};

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

        let queue_index = QueueIndex::from_ptr(ptr);
        log::trace!("{thread:?}: wake queue index {}", queue_index.index);

        // NOTE: the unit here is a lie! we just don't know the correct type for the future here
        let executor = Executor::<()>::get().unwrap();

        executor.inner.queue.wake(queue_index);

        log::trace!("{thread:?}: woke queue index {}", queue_index.index);
    }

    RawWakerVTable::new(clone_raw, wake, wake_by_ref, drop_raw)
};

pub struct Executor<F: 'static> {
    inner: &'static Inner<F>,
}

type Http1Connection = hyper::client::conn::http1::Connection<reactor::TcpStream, String>;

struct Inner<F> {
    /// How many queue slots are there per bucket
    io_resources: IoResources,
    /// number of active queue slots for each bucket
    refcounts: Box<[Mutex<u8>]>,
    futures: Box<[Mutex<Option<F>>]>,
    tcp_streams: Box<[AtomicI32]>,
    connections: Box<[Mutex<Slot<Http1Connection>>]>,
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

                let queue = ComplexQueue::with_capacity(bucket_count, io_resources);

                let tcp_streams = std::iter::repeat_with(|| AtomicI32::new(0))
                    .take(bucket_count)
                    .collect();

                let inner = Inner {
                    io_resources,
                    futures,
                    tcp_streams,
                    refcounts,
                    connections,
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

    pub fn execute(&self, fd: OwnedFd, bucket_index: BucketIndex, fut: F) {
        self.inner.set(fd, bucket_index, fut);

        let io_resources = self.inner.io_resources;
        let queue_index = QueueIndex::from_bucket_index(io_resources, bucket_index);

        // TODO: this assert fails in practice, and is very expensive
        // let range = self.inner.io_resources.queue_slots(bucket_index);
        // assert!(self.inner.queue.is_range_empty(range));

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

        let io_resources = self.inner.io_resources;
        let queue_index = QueueIndex::from_connection_index(io_resources, connection_index);
        let stream = reactor.register(queue_index, stream).unwrap();

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

enum DoneWithBucket {
    ForNow,
    Forever,
    OutOfMemory,
    Panic,
}

enum CleanupReason {
    OutOfMemory,
    Panic,
}

fn send_error_response(
    mut socket: std::net::TcpStream,
    reason: CleanupReason,
) -> std::io::Result<()> {
    use std::io::Write;

    socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n")?;
    socket.write_all(b"Content-Type: text/plain\r\n")?;
    socket.write_all(b"\r\n")?;
    match reason {
        CleanupReason::OutOfMemory => {
            socket.write_all(b"Internal Server Error: Out of Memory\r\n")?;
        }
        CleanupReason::Panic => {
            socket.write_all(b"Internal Server Error: Panic\r\n")?;
        }
    }
    socket.flush()?;

    // a proper shutdown of the connection. We don't need a Content-Length header
    // if we close the connection in this way
    socket.shutdown(std::net::Shutdown::Write)
}

impl<F> Inner<F>
where
    F: Future<Output = ()> + Send,
{
    fn run_bucket(&self, bucket_index: BucketIndex, queue_slot: &QueueSlot) -> DoneWithBucket {
        log::trace!("processing bucket {}", bucket_index.index);

        'bucket: loop {
            let Some(bucket_offset) = queue_slot.dequeue() else {
                log::trace!("no more work (for now) bucket {}", bucket_index.index);
                return DoneWithBucket::ForNow;
            };

            let queue_index = QueueIndex {
                index: bucket_index.index * self.io_resources.per_bucket() as u32
                    + bucket_offset as u32,
                identifier: 0,
            };

            log::trace!("processing queue index {}", queue_index.index);

            let io_index = IoIndex::from_index(self.io_resources, queue_index);

            let poll_result = match io_index {
                IoIndex::InputStream(bucket_index) => {
                    let task_mut = self.futures[bucket_index.index as usize]
                        .try_lock()
                        .unwrap();

                    let mut jmp_buf = JumpBuf::new();
                    match unsafe { setjmp(&mut jmp_buf) } {
                        0 => {
                            crate::JMP_BUFFER.with_borrow_mut(|jb| *jb = jmp_buf);
                            match std::panic::catch_unwind(|| {
                                self.poll_input_stream(task_mut, queue_index)
                            }) {
                                Err(_) => {
                                    /* the application went panicked */
                                    let thread = std::thread::current().id();
                                    let bucket = bucket_index.index;
                                    log::error!("{thread:?}: emptying bucket {bucket} after panic");

                                    // Clear the poison, since we only gonna clean up anyway...
                                    self.futures[bucket_index.index as usize].clear_poison();

                                    self.cleanup_bucket(bucket_index, CleanupReason::Panic);

                                    // everything has been cleaned up. This thread is ready for the next request
                                    return DoneWithBucket::Panic;
                                }
                                Ok(poll) => poll,
                            }
                        }
                        _ => {
                            /* the application went Out Of Memory */
                            let thread = std::thread::current().id();
                            let bucket = bucket_index.index;
                            log::info!("{thread:?}: emptying bucket {bucket} after OOM");

                            // free the mutex so cleanup can clean it up
                            drop(task_mut);

                            self.cleanup_bucket(bucket_index, CleanupReason::OutOfMemory);

                            // everything has been cleaned up. This thread is ready for the next request
                            return DoneWithBucket::OutOfMemory;
                        }
                    }
                }
                IoIndex::HttpConnection(connection_index) => {
                    let task_mut = self.connections[connection_index.index as usize]
                        .lock()
                        .unwrap();

                    let mut jmp_buf = JumpBuf::new();
                    match unsafe { setjmp(&mut jmp_buf) } {
                        0 => {
                            crate::JMP_BUFFER.with_borrow_mut(|jb| *jb = jmp_buf);

                            match self.poll_http_connection(task_mut, queue_index) {
                                Poll::Ready(HyperWake::Legit) => Poll::Ready(()),
                                Poll::Ready(HyperWake::AfterDeath) => continue 'bucket,
                                Poll::Pending => Poll::Pending,
                            }
                        }
                        _ => {
                            /* the application went Out Of Memory */

                            // free the mutex so cleanup can clean it up
                            drop(task_mut);

                            self.cleanup_bucket(bucket_index, CleanupReason::OutOfMemory);

                            // everything has been cleaned up. This thread is ready for the next request
                            return DoneWithBucket::OutOfMemory;
                        }
                    }
                }
            };

            match poll_result {
                Poll::Pending => {
                    // done with this queue index, but another job in this bucket may be ready for
                    // processing
                    continue 'bucket;
                }
                Poll::Ready(()) => {
                    // NOTE: the future is already dropped by the polling functions

                    // must block if there is a race condition with the executor trying to find a free slot
                    let mut bucket_guard =
                        self.refcounts[bucket_index.index as usize].lock().unwrap();

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

                        return DoneWithBucket::Forever;
                    } else {
                        *bucket_guard -= 1;

                        continue 'bucket;
                    }
                }
            }
        }
    }

    fn cleanup_bucket(&self, bucket_index: BucketIndex, reason: CleanupReason) {
        // send back an http error response
        let socket_index = QueueIndex::from_bucket_index(self.io_resources, bucket_index);
        let raw_fd = self.tcp_streams[socket_index.index as usize].load(Ordering::Relaxed);

        // SAFETY: We just turned this into a raw fd from an OwnedFd
        let socket_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        let socket = std::net::TcpStream::from(socket_fd);
        let _ = send_error_response(socket, reason);

        // drop all the http connections (should clean up mio registration?)
        for index in self.io_resources.http_connections(bucket_index) {
            *self.connections[index].try_lock().unwrap() = Slot::Empty;
        }

        // drop the main future
        let _ = self.futures[socket_index.index as usize]
            .lock()
            .unwrap()
            .take();

        // clean up the memory
        ALLOCATOR.0.clear_bucket(bucket_index.index as usize);

        // signals that this bucket is now free for the next request
        *self.refcounts[bucket_index.index as usize].lock().unwrap() = 0;
    }

    fn run_worker(&self) -> ! {
        loop {
            let (bucket_index, queue_slot) = self.queue.blocking_dequeue();

            // configure the allocator
            CURRENT_ARENA.with(|a| a.store(bucket_index.index, Ordering::Release));

            loop {
                match self.run_bucket(bucket_index, queue_slot) {
                    DoneWithBucket::Forever | DoneWithBucket::ForNow => {
                        match self.queue.done_with_bucket(queue_slot) {
                            NextStep::Done => {
                                log::trace!(
                                    "bucket {} marked as not enqueued, not in progress",
                                    bucket_index.index
                                );
                                break;
                            }
                            NextStep::GoAgain => {
                                log::trace!("bucket {} marked as not enqueued", bucket_index.index);

                                continue;
                            }
                        }
                    }
                    DoneWithBucket::OutOfMemory | DoneWithBucket::Panic => continue,
                }
            }
        }
    }

    fn poll_input_stream(
        &self,
        mut task_mut: MutexGuard<'_, Option<F>>,
        queue_index: QueueIndex,
    ) -> Poll<()> {
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
        mut task_mut: MutexGuard<'_, Slot<Http1Connection>>,
        queue_index: QueueIndex,
    ) -> Poll<HyperWake> {
        let thread = std::thread::current().id();

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

        log::debug!("{thread:?}: queue index {} cleared", queue_index.index);

        // only stores PhandomData<TcpStream>. the actual stream is not in here
        drop(connection);

        if let Err(e) = result {
            log::warn!("error in http connection {e:?}");
        }

        Poll::Ready(HyperWake::Legit)
    }

    fn set(&self, fd: OwnedFd, index: BucketIndex, fut: F) {
        let old_value = self.futures[index.index as usize]
            .try_lock()
            .unwrap()
            .replace(fut);

        self.tcp_streams[index.index as usize].store(fd.into_raw_fd(), Ordering::Relaxed);

        // NOTE try_claim already set the refcount of this slot to 1

        assert!(old_value.is_none(), "Bucket {index:?} was empty!")
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
