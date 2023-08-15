// https://github.com/ibraheemdev/astra/blob/53ad0859de7a1e2af90d8ae1a6666c9a7a276c03/src/net.rs#L13

use std::{
    collections::HashMap,
    net::Shutdown,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    task::{ready, Context, Poll, Waker},
};

use mio::{Events, Interest, Token};

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Read = 0,
    Write = 1,
}

struct Source {
    interest: Mutex<[Option<Waker>; 2]>,
    triggered: [AtomicBool; 2],
    token: Token,
}

#[derive(Clone, Copy)]
pub struct Reactor {
    shared: &'static Shared,
}

impl Reactor {
    const CAPACITY: usize = 64;

    pub fn get() -> std::io::Result<Self> {
        static SHARED: OnceLock<Shared> = OnceLock::new();

        match SHARED.get() {
            None => {
                let poll = mio::Poll::new()?;

                let shared = Shared {
                    registry: poll.registry().try_clone()?,
                    token: AtomicUsize::new(0),
                    sources: Mutex::new(HashMap::with_capacity(Self::CAPACITY)),
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

    pub fn register(&self, tcp_stream: std::net::TcpStream) -> std::io::Result<TcpStream> {
        tcp_stream.set_nonblocking(true)?;

        let mut tcp_stream = mio::net::TcpStream::from_std(tcp_stream);
        let token = Token(self.shared.token.fetch_add(1, Ordering::Relaxed));

        self.shared.registry.register(
            &mut tcp_stream,
            token,
            Interest::READABLE | Interest::WRITABLE,
        )?;

        let source = Arc::new(Source {
            interest: Default::default(),
            triggered: Default::default(),
            token,
        });

        {
            let mut sources = self.shared.sources.lock().unwrap();
            sources.insert(token, source.clone());
        }

        let tcp_stream = TcpStream {
            tcp_stream,
            reactor: self.clone(),
            _source: source,
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
            let mut interest = source.interest.lock().unwrap();

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
    token: AtomicUsize,
    sources: Mutex<HashMap<Token, Arc<Source>>>,
}

impl Shared {
    fn run(&self, mut poll: mio::Poll) -> std::io::Result<()> {
        let mut events = Events::with_capacity(Reactor::CAPACITY);
        let mut wakers = Vec::with_capacity(Reactor::CAPACITY);

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

            return Ok(());
        }

        for event in events.iter() {
            let source = {
                let sources = self.sources.lock().unwrap();

                match sources.get(&event.token()) {
                    Some(source) => source.clone(),
                    None => continue,
                }
            };

            let mut interest = source.interest.lock().unwrap();

            if event.is_readable() {
                if let Some(waker) = interest[Direction::Read as usize].take() {
                    wakers.push(waker);
                }

                // TODO why release?
                source.triggered[Direction::Read as usize].store(true, Ordering::Release);
            }

            if event.is_readable() {
                if let Some(waker) = interest[Direction::Write as usize].take() {
                    wakers.push(waker);
                }

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
    _source: Arc<Source>,
    token: Token,
}

impl TcpStream {
    pub fn poll_io<T>(
        &self,
        direction: Direction,
        mut f: impl FnMut() -> std::io::Result<T>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<T>> {
        log::trace!("----------> poll io");
        loop {
            let sources = self.reactor.shared.sources.lock().unwrap();
            let source = &sources[&self.token];
            // ready!(self.reactor.poll_ready(&source, direction, cx))?;
            match self.reactor.poll_ready(&source, direction, cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(_) => {}
            }

            match f() {
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    self.reactor.clear_trigger(&source, direction);
                }
                val => {
                    log::trace!("<---------- poll io");

                    return Poll::Ready(val);
                }
            }
        }
    }

    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        TcpStreamRead {
            tcp_stream: self,
            buf,
        }
        .await
    }

    pub async fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        TcpStreamWrite {
            tcp_stream: self,
            buf,
        }
        .await
    }

    pub async fn shutdown(&self) -> std::io::Result<()> {
        std::future::poll_fn(|cx| {
            self.poll_io(
                Direction::Read,
                || self.tcp_stream.shutdown(Shutdown::Read),
                cx,
            )
        })
        .await?;

        std::future::poll_fn(|cx| {
            self.poll_io(
                Direction::Write,
                || self.tcp_stream.shutdown(Shutdown::Both),
                cx,
            )
        })
        .await
    }

    fn poll_read(
        self: Pin<&mut &Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        use std::io::Read;

        self.poll_io(Direction::Read, || (&self.tcp_stream).read(buf), cx)
    }

    fn poll_write(
        self: Pin<&mut &Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        use std::io::Write;

        self.poll_io(Direction::Write, || (&self.tcp_stream).write(buf), cx)
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let mut sources = self.reactor.shared.sources.lock().unwrap();
        let _ = sources.remove(&self.token);
        let _ = self
            .reactor
            .shared
            .registry
            .deregister(&mut self.tcp_stream);
    }
}

struct TcpStreamRead<'a> {
    tcp_stream: &'a TcpStream,
    buf: &'a mut [u8],
}

impl<'a> std::future::Future for TcpStreamRead<'a> {
    type Output = std::io::Result<usize>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let unchecked_mut = unsafe { self.get_unchecked_mut() };

        let buf = &mut unchecked_mut.buf;
        let tcp_stream = unsafe { Pin::new_unchecked(&mut unchecked_mut.tcp_stream) };

        tcp_stream.poll_read(cx, buf)
    }
}

struct TcpStreamWrite<'a> {
    tcp_stream: &'a TcpStream,
    buf: &'a [u8],
}

impl<'a> std::future::Future for TcpStreamWrite<'a> {
    type Output = std::io::Result<usize>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let unchecked_mut = unsafe { self.get_unchecked_mut() };

        let buf = &mut unchecked_mut.buf;
        let tcp_stream = unsafe { Pin::new_unchecked(&mut unchecked_mut.tcp_stream) };

        tcp_stream.poll_write(cx, buf)
    }
}
