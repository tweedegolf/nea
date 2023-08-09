use std::ptr::NonNull;

struct AppAlloc;

unsafe impl Sync for AppAlloc {}

#[global_allocator]
static ALLOCATOR: AppAlloc = AppAlloc;

unsafe impl std::alloc::GlobalAlloc for AppAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        extern "C" {
            fn roc_alloc(size: usize, alignment: u32) -> NonNull<u8>;
        }

        roc_alloc(layout.size(), layout.align() as u32).as_ptr()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
        // do not bother
    }
}

#[no_mangle]
pub extern "C-unwind" fn roc_main(input: String) -> String {
    format!("Hello, {input}")
}
