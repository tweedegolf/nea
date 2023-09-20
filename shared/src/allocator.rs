use std::{
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

const INITIAL_ARENA_SIZE: usize = 4096 * 128;
const MAX_SUPPORTED_ALIGN: usize = 4096;
#[repr(C, align(4096))] // 4096 == MAX_SUPPORTED_ALIGN
pub struct ServerAlloc {
    initial_arena: UnsafeCell<[u8; INITIAL_ARENA_SIZE]>,
    initial_arena_remaining: AtomicUsize, // allocate from the top, counting down
    buckets: UnsafeCell<&'static [Bucket]>,
}

unsafe impl Sync for ServerAlloc {}

impl ServerAlloc {
    pub const fn new() -> Self {
        Self {
            initial_arena: UnsafeCell::new([0xAA; INITIAL_ARENA_SIZE]),
            initial_arena_remaining: AtomicUsize::new(INITIAL_ARENA_SIZE),
            buckets: UnsafeCell::new(&[]),
        }
    }

    pub fn try_allocate_in_initial_bucket(
        &self,
        layout: std::alloc::Layout,
    ) -> Option<NonNull<u8>> {
        let non_null = NonNull::new(self.initial_arena.get().cast()).unwrap();
        try_alloc_help(layout, non_null, &self.initial_arena_remaining)
    }

    pub fn try_allocate_in_bucket(
        &self,
        layout: std::alloc::Layout,
        bucket_index: usize,
    ) -> Option<NonNull<u8>> {
        let buckets = unsafe { *self.buckets.get() };
        let Some(bucket) = buckets.get(bucket_index) else {
            log::error!("bucket index {bucket_index} is out of range");
            panic!();
        };

        try_alloc_help(layout, bucket.start, &bucket.remaining)
    }

    pub fn clear_bucket(&self, bucket_index: usize) {
        let buckets = unsafe { *self.buckets.get() };
        let bucket = &buckets[bucket_index];

        bucket.remaining.store(bucket.size, Ordering::Relaxed);
    }

    pub fn initialize_buckets(
        &self,
        number_of_buckets: usize,
        bucket_size: usize,
    ) -> std::io::Result<()> {
        let ptr = mmap(number_of_buckets, bucket_size)?;

        let buckets: Vec<_> = (0..number_of_buckets)
            .map(|i| Bucket {
                start: unsafe {
                    NonNull::new(ptr.as_ptr().add(i * bucket_size)).unwrap_unchecked()
                },
                size: bucket_size,
                remaining: AtomicUsize::new(bucket_size),
            })
            .collect();

        let buckets = buckets.leak();

        unsafe { std::ptr::write(self.buckets.get(), buckets) };

        Ok(())
    }
}

fn try_alloc_help(
    layout: std::alloc::Layout,
    origin: NonNull<u8>,
    remaining: &AtomicUsize,
) -> Option<NonNull<u8>> {
    let size = layout.size();
    let align = layout.align();

    // `Layout` contract forbids making a `Layout` with align=0, or align not power of 2.
    // So we can safely use a mask to ensure alignment without worrying about UB.
    let align_mask_to_round_down = !(align - 1);

    if align > MAX_SUPPORTED_ALIGN {
        return None;
    }

    let mut allocated = 0;
    let result = remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |mut remaining| {
        if size > remaining {
            log::error!("bucket has only {remaining} bytes left, not enough for {size} bytes");
            return None;
        }
        remaining -= size;
        remaining &= align_mask_to_round_down;
        allocated = remaining;
        Some(remaining)
    });

    match result {
        Err(_) => None,
        Ok(_) => NonNull::new(unsafe { origin.as_ptr().add(allocated) }),
    }
}

/// # Design
struct Bucket {
    start: NonNull<u8>,
    size: usize,
    remaining: AtomicUsize,
}

#[cfg(unix)]
fn mmap(number_of_buckets: usize, bucket_size: usize) -> std::io::Result<NonNull<u8>> {
    let len = number_of_buckets * bucket_size;

    let prot = libc::PROT_READ | libc::PROT_WRITE;

    // not backed by any file (we're just allocating memory)
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    let fd = -1;
    let offset = 0;

    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, offset) };

    if ptr as isize == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        match NonNull::new(ptr.cast()) {
            None => panic!("mmap should give an error, or a valid pointer!"),
            Some(non_null) => Ok(non_null),
        }
    }
}
