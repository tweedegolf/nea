use std::{
    alloc::GlobalAlloc,
    cell::RefCell,
    io::ErrorKind,
    os::fd::AsRawFd,
    ptr::NonNull,
    sync::atomic::{AtomicU32, Ordering},
};

mod config;
mod executor;
pub mod index;
mod queue;
mod reactor;

use config::Config;
use executor::{BucketIndex, Executor};
use index::QueueIndex;
use reactor::Reactor;
use shared::setjmp_longjmp::{longjmp, JumpBuf};

pub mod net {
    pub use crate::reactor::TcpStream;
}

pub mod http1 {
    pub async fn handshake(
        bucket_index: crate::index::BucketIndex,
        host: &str,
        port: u16,
    ) -> std::io::Result<hyper::client::conn::http1::SendRequest<String>> {
        crate::executor::Executor::<()>::get()
            .unwrap()
            .handshake(bucket_index, host, port)
            .await
    }
}

thread_local! {
    /// The buffer used to store the state of registers before calling application code.core
    ///
    /// An error in the application code (out of memory, a panic, a segfault) will restore the
    /// non-volatile registers to their previous state, so that a thread can recover from an error
    /// in the application.
    static JMP_BUFFER: RefCell<JumpBuf> = const { RefCell::new(JumpBuf::new()) };
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
    pub(crate) static CURRENT_ARENA: AtomicU32 = const { AtomicU32::new(ARENA_INDEX_BEFORE_MAIN) };
}

/// Function called when the applications hits a (from its perspective) unrecoverable error.
///
/// For instance: out of memory, an assert, division by zero
///
/// # Safety
///
/// The message_ptr argument must be a valid CStr
pub unsafe extern "C" fn roc_panic(message_ptr: *const i8, panic_tag: u32) -> ! {
    let thread_id = std::thread::current().id();

    assert!(!message_ptr.is_null());

    let message_cstr = unsafe { std::ffi::CStr::from_ptr(message_ptr) };
    let message = message_cstr.to_str().unwrap();

    eprintln!("thread {thread_id:?} called roc_panic {panic_tag}: {message}");

    let jmp_buf = JMP_BUFFER.with_borrow(|jmp_buf| *jmp_buf);
    unsafe { longjmp(&jmp_buf, 1) }
}

/// Core primitive for the application's allocator.
///
/// # Safety
///
/// Should only be called after a thread has set its arena
pub unsafe extern "C" fn roc_alloc(size: usize, alignment: u32) -> NonNull<u8> {
    let bucket_index = CURRENT_ARENA.with(|v| v.load(Ordering::Relaxed)) as usize;

    let layout = std::alloc::Layout::from_size_align(size, alignment as usize).unwrap();

    match ALLOCATOR.0.try_allocate_in_bucket(layout, bucket_index) {
        None => {
            let msg = b"out of memory\0";
            let panic_tag = 1;
            roc_panic(msg.map(|x| x as std::ffi::c_char).as_ptr(), panic_tag)
        }
        Some(non_null) => non_null,
    }
}

/// # Safety
/// Same as the [GlobalAlloc::realloc]
pub unsafe extern "C" fn roc_realloc(
    ptr: *mut u8,
    new_size: usize,
    old_size: usize,
    alignment: u32,
) -> NonNull<u8> {
    let layout = std::alloc::Layout::from_size_align(old_size, alignment as usize).unwrap();

    match NonNull::new(ALLOCATOR.realloc(ptr, layout, new_size)) {
        None => {
            let msg = b"out of memory\0";
            let panic_tag = 1;
            roc_panic(msg.map(|x| x as std::ffi::c_char).as_ptr(), panic_tag)
        }
        Some(ptr) => ptr,
    }
}

/// # Safety
/// Same as the [GlobalAlloc::dealloc]
pub unsafe extern "C" fn roc_dealloc(_ptr: *mut u8, _alignment: u32) {
    /* do absolutely nothing */
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
                #[cfg(target_os = "linux")]
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

                #[cfg(target_os = "linux")]
                {
                    let id = std::thread::current().id();
                    log::info!("thread {id:?}: bucket {bucket_name}: allocating {_size} bytes",);
                }

                match self.0.try_allocate_in_initial_bucket(layout) {
                    None => {
                        // an allocation that is too big for these buckets is almost certainly a
                        // panic. We want a good backtrace message, so do use the system allocator
                        // here to collect/store all the information to format the panic message
                        std::alloc::System.alloc(layout)
                    }
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

pub fn run_request_handler<FUNC, FUT>(func: FUNC) -> std::io::Result<()>
where
    FUNC: Fn(BucketIndex, crate::reactor::TcpStream) -> FUT + Send + 'static + Copy,
    FUT: std::future::Future<Output = std::io::Result<()>> + Send + 'static,
{
    log::init();

    let config = Config::load();

    ALLOCATOR
        .0
        .initialize_buckets(config.bucket_count, 4096 * 128)?;

    let executor = Executor::get_or_init(config.bucket_count, config.io_resources);
    let reactor = Reactor::get_or_init(config.bucket_count, config.io_resources).unwrap();

    let _handle1 = executor.spawn_worker().unwrap();
    let _handle2 = executor.spawn_worker().unwrap();
    let _handle2 = executor.spawn_worker().unwrap();
    let _handle2 = executor.spawn_worker().unwrap();

    let addr = format!("{}:{}", config.host, config.port);
    let listener = std::net::TcpListener::bind(&addr)?;
    let id = std::thread::current().id();
    println!("listening on http://{addr} on thread {id:?}");

    // crate::IS_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);

    // accept connections and process them serially
    for (i, stream) in listener.incoming().enumerate() {
        let stream = stream.unwrap();
        stream.set_nonblocking(true).unwrap();

        // Clone the TcpStream socket, so that we still have a handle to send an error response in
        // case of panic or OOM
        let fd = stream.try_clone().expect("we never run out of FDs").into();

        match executor.try_claim() {
            None => {
                // no space in the queue
                log::warn!("main: no space in the queue");
                stream.shutdown(std::net::Shutdown::Both).unwrap();
            }
            Some(bucket_index) => {
                executor.execute(fd, bucket_index, async move {
                    log::info!(
                        "new connection {i} (index = {}, fd = {})",
                        bucket_index.index,
                        stream.as_raw_fd()
                    );

                    let queue_index =
                        QueueIndex::from_bucket_index(config.io_resources, bucket_index);
                    let tcp_stream = reactor.register(queue_index, stream).unwrap();

                    match func(bucket_index, tcp_stream).await {
                        Ok(()) => {}
                        Err(e) => match e.kind() {
                            ErrorKind::NotConnected => {}
                            _ => panic!("{e:?}"),
                        },
                    }
                });
            }
        };

        log::info!("main spawned future for connection {i}");
    }

    Ok(())
}
