// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use hyper::rt::ReadBufCursor;
use std::io::Error;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::{
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    task::{Context, Poll, Waker},
};

use mio::{Events, Interest, Token};

use crate::index::{IoResources, QueueIndex};
use crate::CURRENT_ARENA;

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Read = 0,
    Write = 1,
}

struct Source {
    // these wakers are None when the application is not awaiting events on this source,
    // and Some when the application is (e.g. doing a `tcp_stream.read(&mut buf).await;`
    interest: Mutex<[Option<Waker>; 2]>,
    triggered: [AtomicBool; 2],
}

#[derive(Clone, Copy)]
pub struct Reactor {
    shared: &'static Shared,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

impl Reactor {
    pub fn get() -> Option<Self> {
        SHARED.get().map(|shared| Reactor { shared })
    }

    pub fn get_or_init(bucket_count: usize, io_resources: IoResources) -> std::io::Result<Self> {
        match SHARED.get() {
            None => {
                let queue_capacity = bucket_count * io_resources.per_bucket();
                let poll = mio::Poll::new()?;

                let shared = Shared {
                    registry: poll.registry().try_clone()?,
                    sources: std::iter::repeat_with(|| Mutex::new(None))
                        .take(queue_capacity)
                        .collect(),
                    io_resources,
                };

                let shared = SHARED.get_or_init(|| shared);

                std::thread::Builder::new()
                    .name(String::from("nea-reactor"))
                    .spawn(move || shared.run(poll))?;

                Ok(Reactor { shared })
            }
            Some(shared) => {
                // nothing to do
                Ok(Reactor { shared })
            }
        }
    }

    pub fn register(
        &self,
        index: QueueIndex,
        tcp_stream: std::net::TcpStream,
    ) -> std::io::Result<TcpStream> {
        tcp_stream.set_nonblocking(true).unwrap();

        let mut tcp_stream = mio::net::TcpStream::from_std(tcp_stream);
        let token = Token(index.to_usize());

        let source = Source {
            interest: Default::default(),
            triggered: Default::default(),
        };

        let old = self.shared.sources[index.index as usize]
            .lock()
            .unwrap()
            .replace(source);
        debug_assert!(old.is_none());

        // IMPORTANT: only register when everything is in place to handle events on this fd
        self.shared
            .registry
            .register(
                &mut tcp_stream,
                token,
                Interest::READABLE | Interest::WRITABLE,
            )
            .unwrap();

        log::debug!("queue index {} added to poll", index.index);

        let tcp_stream = TcpStream {
            tcp_stream,
            reactor: *self,
            token,
        };

        Ok(tcp_stream)
    }

    fn poll_ready(
        &self,
        source: &Source,
        direction: Direction,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        if source.triggered[direction as usize].load(Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }

        // update the waker if required
        {
            let mut interest = source.interest.lock().expect("poll_ready to take the lock");

            match &mut interest[direction as usize] {
                Some(existing) if existing.will_wake(cx.waker()) => {
                    /* has the right waker already */
                }
                other => {
                    *other = Some(cx.waker().clone());
                }
            }
        }

        // check if anything changed while we were registering our waker
        if source.triggered[direction as usize].load(Ordering::Acquire) {
            // just wake up immediately
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }

    fn clear_trigger(&self, source: &Source, direction: Direction) {
        source.triggered[direction as usize].store(false, Ordering::Release);
    }
}

struct Shared {
    registry: mio::Registry,
    sources: Vec<Mutex<Option<Source>>>,
    io_resources: IoResources,
}

unsafe impl Sync for Shared {}

impl Shared {
    fn run(&self, mut poll: mio::Poll) -> std::io::Result<()> {
        CURRENT_ARENA.with(|a| a.store(crate::ARENA_INDEX_REACTOR, Ordering::Relaxed));

        let queue_capacity = self.sources.len() * self.io_resources.per_bucket();
        let mut events = Events::with_capacity(queue_capacity);
        let mut wakers = Vec::with_capacity(queue_capacity);

        loop {
            if let Err(err) = self.poll(&mut poll, &mut events, &mut wakers) {
                log::warn!("Failed to poll reactor: {}", err);
            }

            events.clear()
        }
    }

    fn poll(
        &self,
        poll: &mut mio::Poll,
        events: &mut Events,
        wakers: &mut Vec<Waker>,
    ) -> std::io::Result<()> {
        // TODO figure out why this is specifically?
        if let Err(err) = poll.poll(events, None) {
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }

            log::warn!("mio poll hit an error {:?}", err);

            return Ok(());
        }

        for event in events.iter() {
            let queue_index = QueueIndex::from_usize(event.token().0);
            let index = queue_index.index as usize;
            let source = self.sources[index].lock().unwrap();

            let source = match source.as_ref() {
                None => continue,
                Some(source) => source,
            };

            let mut interest = source.interest.lock().expect("event loop");

            if event.is_readable() {
                // TODO when is this waker None?
                if let Some(waker) = interest[Direction::Read as usize].take() {
                    wakers.push(waker);
                }

                // wakers.push(crate::executor::waker_for(queue_index));

                // TODO why release?
                source.triggered[Direction::Read as usize].store(true, Ordering::Release);
            }

            if event.is_writable() {
                // TODO when is this waker None?
                if let Some(waker) = interest[Direction::Write as usize].take() {
                    wakers.push(waker);
                }

                // wakers.push(crate::executor::waker_for(queue_index));

                // TODO why release?
                source.triggered[Direction::Write as usize].store(true, Ordering::Release);
            }
        }

        for waker in wakers.drain(..) {
            waker.wake();
        }

        Ok(())
    }
}

pub struct TcpStream {
    tcp_stream: mio::net::TcpStream,
    reactor: Reactor,
    token: Token,
}

impl TcpStream {
    pub fn poll_io<T>(
        &self,
        direction: Direction,
        mut f: impl FnMut() -> std::io::Result<T>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<T>> {
        loop {
            let index = QueueIndex::from_usize(self.token.0);

            let source_guard = self.reactor.shared.sources[index.index as usize]
                .lock()
                .unwrap();

            let Some(source) = source_guard.as_ref() else {
                return Poll::Pending;
            };

            std::task::ready!(self.reactor.poll_ready(source, direction, cx))?;

            match f() {
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    self.reactor.clear_trigger(source, direction);
                }
                val => {
                    return Poll::Ready(val);
                }
            }
        }
    }

    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Read;

        std::future::poll_fn(|cx| {
            self.poll_io(Direction::Read, || (&self.tcp_stream).read(buf), cx)
        })
        .await
    }

    pub async fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        std::future::poll_fn(|cx| {
            self.poll_io(Direction::Write, || (&self.tcp_stream).write(buf), cx)
        })
        .await
    }

    pub async fn flush(&self) -> std::io::Result<()> {
        std::future::poll_fn(|cx| self.poll_io(Direction::Write, || (&self.tcp_stream).flush(), cx))
            .await
    }
}

impl hyper::rt::Read for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        const TMP_BUF_LEN: usize = 1024;
        use std::io::Read;

        let buf_mut = unsafe { buf.as_mut() };
        let mut tmp_buf = [0; TMP_BUF_LEN];
        let remaining = buf_mut.len().min(TMP_BUF_LEN);

        let poll = self.poll_io(
            Direction::Read,
            || (&self.tcp_stream).read(&mut tmp_buf[..remaining]),
            cx,
        );

        let n = std::task::ready!(poll)?;
        let tmp_buf = tmp_buf.map(MaybeUninit::new);

        buf_mut[..n].copy_from_slice(&tmp_buf[..n]);

        unsafe {
            buf.advance(n);
        }

        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Write for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        let n = std::task::ready!(self.poll_io(
            Direction::Write,
            || (&self.tcp_stream).write(buf),
            cx
        ))?;

        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.poll_io(Direction::Write, || (&self.tcp_stream).flush(), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.poll_io(
            Direction::Write,
            || self.tcp_stream.shutdown(std::net::Shutdown::Both),
            cx,
        )
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let index = QueueIndex::from_usize(self.token.0).index as usize;
        log::debug!("queue index {} removed from poll", index);

        let _ = self
            .reactor
            .shared
            .registry
            .deregister(&mut self.tcp_stream);

        let old = self.reactor.shared.sources[index].lock().unwrap().take();
        assert!(old.is_some());
    }
}
