#[cfg(all(unix, target_arch = "x86_64"))]
pub use linux_x86_64::{longjmp, setjmp, JumpBuf};

#[derive(Debug)]
pub enum SetJmp {
    Called,
    Jumped(u32),
}

#[cfg(all(unix, target_arch = "x86_64"))]
mod linux_x86_64 {
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct JumpBuf([usize; 8]);

    impl JumpBuf {
        pub const fn new() -> Self {
            Self([0; 8])
        }
    }

    core::arch::global_asm!(
        r#"
    .global setjmp
    setjmp:
        mov [rdi],    rbx     ; // Store caller saved registers
        mov [rdi+8],  rbp     ; // ^
        mov [rdi+16], r12     ; // ^
        mov [rdi+24], r13     ; // ^
        mov [rdi+32], r14     ; // ^
        mov [rdi+40], r15     ; // ^
        lea rdx,      [rsp+8] ; // go one value up (as if setjmp wasn't called)
        mov [rdi+48], rdx     ; // Store the new rsp pointer in env[7]
        mov rdx,      [rsp]   ; // go one value up (as if setjmp wasn't called)
        mov [rdi+56], rdx     ; // Store the address we will resume at in env[8]
        xor eax,      eax     ; // Always return 0
        ret
    "#
    );

    extern "C-unwind" {
        #[link_name = "setjmp"]
        pub fn setjmp(env: *mut JumpBuf) -> u32;
    }

    //    /// # Safety
    //    ///
    //    /// Must receive exclusive mutable access to the jump buffer
    //    pub unsafe fn setjmp(env: *mut JumpBuf) -> SetJmp {
    //        // TODO perform the call using inline asm? Rust's compilation model cannot deal with setjmp
    //        // returning twice
    //
    //        match unsafe { setjmp_asm(env) } {
    //            0 => SetJmp::Called,
    //            n => SetJmp::Jumped(n),
    //        }
    //    }

    core::arch::global_asm!(
        r#"
    .global longjmp
    longjmp:
        xor eax, eax         ; // set eax to 0
        cmp esi, 1           ; // CF = val ? 0 : 1
        adc eax, esi         ; // eax = val + !val ; These two lines add one to ret if equals 0
        mov rbx, [rdi]       ; // Load in caller saved registers
        mov rbp, [rdi+8]     ; // ^
        mov r12, [rdi+16]    ; // ^
        mov r13, [rdi+24]    ; // ^
        mov r14, [rdi+32]    ; // ^
        mov r15, [rdi+40]    ; // ^
        mov rsp, [rdi+48]    ; // Value of rsp before setjmp call
        jmp [rdi+56]         ; // goto saved address without altering rsp
    "#
    );

    extern "C-unwind" {
        /// Performs transfer of execution to a location dynamically established by [`crate::setjmp`].
        ///
        /// Loads information from the [`crate::JumpBuf`] env. Returns the `ret` value to the caller of
        /// [`crate::setjmp`]. If `ret` was mistakenly given as 0, it is incremented to 1.
        ///
        /// Safety:
        ///
        /// Can only jump over any frames that are "Plain Old Frames," aka frames that can be trivially
        /// deallocated. POF frames do not contain any pending destructors (live `Drop` objects) or
        /// `catch_unwind` calls. (see c-unwind [RFC](https://github.com/rust-lang/rfcs/blob/master/text/2945-c-unwind-abi.md#plain-old-frames))
        pub fn longjmp(env: *const JumpBuf, ret: u32) -> !;
    }
}
