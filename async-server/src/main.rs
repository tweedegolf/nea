use std::{
    cell::UnsafeCell,
    io::{Cursor, ErrorKind},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

mod config;
mod executor;
mod index;
mod queue;
mod reactor;

use config::Config;
use executor::{BucketIndex, Executor};
use index::QueueIndex;
use reactor::Reactor;
use shared::setjmp_longjmp::{longjmp, setjmp, JumpBuf};

thread_local! {
    /// The buffer used to store the state of registers before calling application code.core
    ///
    /// An error in the application code (out of memory, a panic, a segfault) will restore the
    /// non-volatile registers to their previous state, so that a thread can recover from an error
    /// in the application.
    static JMP_BUFFER: UnsafeCell<JumpBuf> = UnsafeCell::new(JumpBuf::new());

    static CURRENT_BUCKET: AtomicUsize = AtomicUsize::new(usize::MAX);
}

const ARENA_INDEX_BEFORE_MAIN: u32 = u32::MAX;
pub(crate) const ARENA_INDEX_EXECUTOR: u32 = u32::MAX - 1;
pub(crate) const ARENA_INDEX_REACTOR: u32 = u32::MAX - 2;
const ARENA_INDEX_UNINITIALIZED: u32 = u32::MAX - 3;

// pub(crate) static IS_RUNNING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Which arena the current thread allocates into. By default this is bucket "infinity", a
    /// statically allocated buffer meant for allocations before `main` and for initializing the
    /// worker threads. After the worker threads have been spawned, nothing should be allocated in
    /// the "infinity" bucket.
    ///
    /// The value is an atomic because that makes updating the value easier. This is a
    /// thread-local, so race conditions are not possible!
    pub(crate) static CURRENT_ARENA: AtomicU32 = AtomicU32::new(ARENA_INDEX_BEFORE_MAIN);
}

extern "C-unwind" {
    // The application main function.
    //
    // Because this is a rust app, we use the `C-unwind` calling convention. That makes it possible
    // for the app to panic, and for the server to catch that panic and recover. `C-unwind` will
    // not be useful with roc applications, but it will still be correct.
    #[allow(improper_ctypes)]
    fn roc_main(input: String) -> String;
}

/// Function called when the applications hits a (from its perspective) unrecoverable error.
///
/// For instance: out of memory, an assert, division by zero
pub unsafe extern "C" fn roc_panic(message_ptr: *const i8, panic_tag: u32) -> ! {
    let thread_id = std::thread::current().id();

    assert!(!message_ptr.is_null());

    let message_cstr = unsafe { std::ffi::CStr::from_ptr(message_ptr) };
    let message = message_cstr.to_str().unwrap();

    eprintln!("thread {thread_id:?} hit a panic {panic_tag}: {message}");

    JMP_BUFFER.with(|env| unsafe { longjmp(env.get(), 1) })
}

/// Core primitive for the application's allocator.
pub unsafe extern "C" fn roc_alloc(size: usize, alignment: u32) -> NonNull<u8> {
    let bucket_index = CURRENT_ARENA.with(|v| v.load(Ordering::Relaxed)) as usize;
    assert!(bucket_index < 1000);

    let layout = std::alloc::Layout::from_size_align(size, alignment as usize).unwrap();

    let size = layout.size();

    eprintln!("arena {bucket_index}: allocating {size} bytes",);

    match ALLOCATOR.0.try_allocate_in_bucket(layout, bucket_index) {
        None => {
            let msg = b"out of memory\0";
            let panic_tag = 1;
            roc_panic(msg.map(|x| x as std::ffi::c_char).as_ptr(), panic_tag)
        }
        Some(non_null) => non_null,
    }
}

#[global_allocator]
static ALLOCATOR: ServerAlloc = ServerAlloc(shared::allocator::ServerAlloc::new());

/// Global Allocator for the Server
///
/// This is where the magic happens
struct ServerAlloc(shared::allocator::ServerAlloc);

unsafe impl std::alloc::GlobalAlloc for ServerAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let _size = layout.size();

        match CURRENT_ARENA.with(|v| v.load(Ordering::Relaxed)) {
            self::ARENA_INDEX_BEFORE_MAIN => {
                log::info!("bucket ∞: allocating {_size} bytes",);

                match self.0.try_allocate_in_initial_bucket(layout) {
                    None => std::ptr::null_mut(), // a panic would be UB!
                    Some(non_null) => non_null.as_ptr(),
                }
            }
            index @ (self::ARENA_INDEX_EXECUTOR | self::ARENA_INDEX_REACTOR) => {
                let bucket_name = match index {
                    self::ARENA_INDEX_EXECUTOR => "executor",
                    self::ARENA_INDEX_REACTOR => "reactor",
                    _ => unreachable!(),
                };

                let id = std::thread::current().id();

                // NOTE: this piece of code runs before main, so before the logger is setup
                // hence we cannot use `log::*` here!
                log::info!("thread {id:?}: bucket {bucket_name}: allocating {_size} bytes",);

                match self.0.try_allocate_in_initial_bucket(layout) {
                    None => std::ptr::null_mut(), // a panic would be UB!
                    Some(non_null) => non_null.as_ptr(),
                }
            }
            self::ARENA_INDEX_UNINITIALIZED => {
                let thread = std::thread::current().id();
                eprintln!("{thread:?}: about to allocate in uninitialized bucket");
                std::ptr::null_mut()
            }
            bucket_index => {
                log::trace!("bucket {bucket_index}: allocating {_size} bytes",);

                let msg = b"out of memory\0";
                let panic_tag = 1;

                match self.0.try_allocate_in_bucket(layout, bucket_index as usize) {
                    None => roc_panic(msg.map(|x| x as std::ffi::c_char).as_ptr(), panic_tag),
                    Some(non_null) => non_null.as_ptr(),
                }
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: std::alloc::Layout) {
        /* do nothing */
    }
}

fn main() -> std::io::Result<()> {
    log::init();

    let config = Config::load();

    ALLOCATOR
        .0
        .initialize_buckets(config.bucket_count, 4096 * 128)?;

    let executor = Executor::get_or_init(config.bucket_count, config.io_resources);
    let reactor = Reactor::get_or_init(config.bucket_count, config.io_resources).unwrap();

    let _handle1 = executor.spawn_worker().unwrap();
    let _handle2 = executor.spawn_worker().unwrap();
    // let _handle3 = executor.spawn_worker().unwrap();

    let addr = format!("{}:{}", config.host, config.port);
    let listener = std::net::TcpListener::bind(&addr)?;
    let id = std::thread::current().id();
    println!("listening on http://{addr} on thread {id:?}");

    // crate::IS_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);

    // accept connections and process them serially
    for (i, stream) in listener.incoming().enumerate() {
        let stream = stream.unwrap();
        stream.set_nonblocking(true).unwrap();
        let fd = stream.as_raw_fd();

        match executor.try_claim() {
            None => {
                // no space in the queue
                log::warn!("main: no space in the queue");
                stream.shutdown(std::net::Shutdown::Both).unwrap();
            }
            Some(bucket_index) => {
                executor.execute(bucket_index, async move {
                    log::info!(
                        "new connection {i} (index = {}, fd = {fd})",
                        bucket_index.index
                    );

                    let queue_index =
                        QueueIndex::from_bucket_index(config.io_resources, bucket_index);
                    let tcp_stream = reactor.register(queue_index, stream).unwrap();

                    match hyper_app(bucket_index, tcp_stream).await {
                        Ok(()) => {}
                        Err(e) => match e.kind() {
                            ErrorKind::NotConnected => {}
                            _ => Err(e).unwrap(),
                        },
                    }
                });
            }
        };

        log::info!("main spawned future for connection {i}");
    }

    Ok(())
}

#[derive(Debug)]
enum Cmd {
    Read,
    Write(Vec<u8>),
    Done,
}

#[derive(Debug)]
enum Msg {
    Nothing,
    Read(Vec<u8>),
}

#[derive(Debug)]
enum Model {
    Initial,
    Writing,
}

fn roc_handler(model: Model, msg: Msg) -> (Model, Cmd) {
    match (model, msg) {
        (Model::Initial, Msg::Nothing) => {
            // let's read something
            (Model::Initial, Cmd::Read)
        }
        (Model::Initial, Msg::Read(buf)) => {
            //
            drop(buf);

            use std::io::Write;
            let input = "foobar";
            let mut buffer = [0u8; 1024];
            let mut response = Cursor::new(&mut buffer[..]);
            let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
            let _ = response.write_all(b"Content-Type: text/html\r\n");
            let _ = response.write_all(b"\r\n");
            let _ = response.write_fmt(format_args!("<html>{input}</html>"));
            let n = response.position() as usize;

            let buf = response.get_ref()[..n].to_vec();

            (Model::Writing, Cmd::Write(buf))
        }
        (Model::Writing, _) => (Model::Writing, Cmd::Done),
    }
}

async fn handler(
    _bucket_index: BucketIndex,
    tcp_stream: crate::reactor::TcpStream,
) -> std::io::Result<()> {
    let mut msg = Msg::Nothing;
    let mut model = Model::Initial;

    loop {
        let (new_model, cmd) = roc_handler(model, msg);
        model = new_model;

        match cmd {
            Cmd::Read => {
                let mut buf = vec![0u8; 1024];
                let n = tcp_stream.read(&mut buf).await?;

                buf.truncate(n);
                msg = Msg::Read(buf);
            }
            Cmd::Write(buf) => {
                let mut remaining = buf.len();
                while remaining > 0 {
                    remaining -= tcp_stream.write(&buf).await?;
                }

                msg = Msg::Nothing;
            }
            Cmd::Done => break,
        }
    }

    Ok(())
}

#[allow(unused)]
async fn hyper_app(
    bucket_index: BucketIndex,
    tcp_stream: reactor::TcpStream,
) -> std::io::Result<()> {
    use std::io::Write;

    let mut buffer = [0; 1024];
    let n = tcp_stream.read(&mut buffer).await?;

    let input = &buffer[..n];
    let input = std::str::from_utf8(input).unwrap();

    let url = "http://0.0.0.0:8000/build.rs"
        .parse::<hyper::Uri>()
        .unwrap();
    let host = url.host().expect("uri has no host");
    let port = url.port_u16().unwrap_or(80);

    let mut sender = executor::Executor::<()>::get()
        .unwrap()
        .handshake(bucket_index, host, port)
        .await
        .unwrap();

    log::info!("performed handshake {}", bucket_index.index);

    let authority = url.authority().unwrap().clone();

    let req = hyper::Request::builder()
        .header("Host", "example.com")
        .method("GET")
        .body(String::new())
        .unwrap();

    log::info!("built request");

    let mut res = sender.send_request(req).await.unwrap();
    log::info!("sent request");

    // Stream the body, writing each frame to stdout as it arrives
    use http_body_util::BodyExt;
    while let Some(next) = res.frame().await {
        let frame = next.unwrap();
    }

    // std::mem::forget(res);

    // std::mem::forget(res);
    log::info!("into body");

    let mut buffer = [0u8; 1024];
    let mut response = Cursor::new(&mut buffer[..]);
    let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
    let _ = response.write_all(b"Content-Type: text/html\r\n");
    let _ = response.write_all(b"\r\n");
    let _ = response.write_fmt(format_args!("<html>{input}</html>"));
    let n = response.position() as usize;

    tcp_stream.write(&response.get_ref()[..n]).await?;

    tcp_stream.flush().await?;

    log::info!("handled a request");

    Ok(())
}
