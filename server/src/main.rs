use std::{
    cell::UnsafeCell,
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use shared::indexer::Indexer;
use shared::queue::LockFreeQueue;
use shared::setjmp_longjmp::{longjmp, setjmp, JumpBuf};

thread_local! {
    /// The buffer used to store the state of registers before calling application code.core
    ///
    /// An error in the application code (out of memory, a panic, a segfault) will restore the
    /// non-volatile registers to their previous state, so that a thread can recover from an error
    /// in the application.
    static JMP_BUFFER: UnsafeCell<JumpBuf> = UnsafeCell::new(JumpBuf::new());

    /// Which bucket the current thread allocates into. By default this is bucket "infinity", a
    /// statically allocated buffer meant for allocations before `main` and for initializing the
    /// worker threads. After the worker threads have been spawned, nothing should be allocated in
    /// the "infinity" bucket.
    ///
    /// The value is an atomic because that makes updating the value easier. This is a
    /// thread-local, so race conditions are not possible!
    static CURRENT_BUCKET: AtomicUsize = AtomicUsize::new(usize::MAX);
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
    let bucket_index = CURRENT_BUCKET.with(|v| v.load(Ordering::Relaxed));

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

/// Global Allocator for the Server
///
/// This is where the magic happens
struct ServerAlloc(shared::allocator::ServerAlloc);

#[global_allocator]
static ALLOCATOR: ServerAlloc = ServerAlloc(shared::allocator::ServerAlloc::new());

unsafe impl std::alloc::GlobalAlloc for ServerAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let _size = layout.size();

        match CURRENT_BUCKET.with(|v| v.load(Ordering::Relaxed)) {
            usize::MAX => {
                // NOTE: this piece of code runs before main, so before the logger is setup
                // hence we cannot use `log::*` here!
                eprintln!("bucket ∞: allocating {_size} bytes",);

                match self.0.try_allocate_in_initial_bucket(layout) {
                    None => std::ptr::null_mut(), // a panic would be UB!
                    Some(non_null) => non_null.as_ptr(),
                }
            }
            bucket_index => {
                log::trace!("bucket {bucket_index}: allocating {_size} bytes",);

                let msg = b"out of memory\0";
                let panic_tag = 1;

                match self.0.try_allocate_in_bucket(layout, bucket_index) {
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

fn worker<const N: usize>(queue: &LockFreeQueue<TcpStream, N>) -> std::io::Result<()> {
    loop {
        // sleep until there is a request for us to handle
        let stream = queue.blocking_dequeue();

        // claim a bucket for this request
        let bucket_index = INDEXER.insert(stream.as_raw_fd()).unwrap();
        CURRENT_BUCKET.with(|old_index| old_index.store(bucket_index, Ordering::Relaxed));

        log::trace!("bucket {} claimed!", bucket_index);

        // Store the current state of the program
        //
        // # Safety
        //
        // The call to setjmp is the scrutinee of an `if` expression.
        let env = JMP_BUFFER.with(|env| env.get());
        if unsafe { setjmp(env) } == 0 {
            handle_stream(&stream)?;

            log::info!("responed to request");

            stream.shutdown(std::net::Shutdown::Both)?;
        } else {
            log::warn!("request failed");

            // TODO send an error 500 here

            stream.shutdown(std::net::Shutdown::Both)?;
        }

        // empty this bucket, and mark it as available again
        ALLOCATOR.0.clear_bucket(bucket_index);
        INDEXER.remove(bucket_index);

        log::trace!("bucket {} free'd!", bucket_index);
    }
}

fn handle_stream(mut stream: &TcpStream) -> std::io::Result<()> {
    use std::io::{Read, Write};

    let mut buf = vec![0; 1024];
    let n = stream.read(&mut buf)?;

    buf.resize(n, 0);

    let Ok(input) = String::from_utf8(buf) else {
        log::warn!("input not utf8");
        stream.write_all(b"internal server error")?;
        return Ok(());
    };

    let response = match std::panic::catch_unwind(|| unsafe { roc_main(input) }) {
        Err(e) => {
            log::warn!("a request handler hit a panic: {:?}", e);
            stream.write_all(b"a request handler hit a panic")?;
            return Ok(());
        }
        Ok(response) => response,
    };

    stream.write_all(response.as_bytes())?;

    Ok(())
}

static INDEXER: Indexer = Indexer::new();

fn main() {
    env_logger::init();

    const QUEUE_SIZE: usize = 16;

    let number_of_buckets = 64;
    let bucket_size = 8 * 1024;

    ALLOCATOR
        .0
        .initialize_buckets(number_of_buckets, bucket_size)
        .unwrap();

    INDEXER.initialize(QUEUE_SIZE);

    let queue = LockFreeQueue::<TcpStream, QUEUE_SIZE>::new();
    let queue = &queue;

    std::thread::scope(|spawner| {
        for _ in 0..2 {
            spawner.spawn(|| worker(queue));

            // TODO handle any panics in the workers
        }

        let listener = TcpListener::bind("127.0.0.1:8000").unwrap();
        println!("listening on 127.0.0.1:8000");

        for stream in listener.incoming() {
            match queue.enqueue(stream.unwrap()) {
                Ok(()) => {}
                Err(_stream) => {
                    // TODO respond with some status code that indicates that the server is busy
                    log::warn!("dropping");
                }
            }
        }
    });
}
