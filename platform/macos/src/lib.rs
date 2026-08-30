//! tsp-macos
//!
//! macOS platform layer. QoS hints keep CPU-pool workers on performance
//! cores — heterogeneous M-series machines otherwise park some workers on
//! efficiency cores, adding tail latency to uniform parallel workloads.

/// Ask the scheduler to run the CURRENT thread on performance cores.
pub fn prefer_performance_cores() {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

/// JIT memory (Apple Silicon W^X discipline): MAP_JIT pages, per-thread
/// write/exec toggle, icache flush. The only three functions allowed to
/// touch JIT page state.
#[cfg(target_os = "macos")]
pub mod jit {
    pub fn map(len: usize) -> *mut u8 {
        unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            );
            assert!(p != libc::MAP_FAILED, "MAP_JIT mmap failed");
            p as *mut u8
        }
    }

    /// true = this thread may WRITE JIT pages (and must not execute them);
    /// false = executable again.
    pub fn writable(w: bool) {
        unsafe { pthread_jit_write_protect_np(if w { 0 } else { 1 }) }
    }

    pub fn flush(ptr: *const u8, len: usize) {
        unsafe { sys_icache_invalidate(ptr as *mut core::ffi::c_void, len) }
    }

    unsafe extern "C" {
        fn pthread_jit_write_protect_np(enabled: core::ffi::c_int);
        fn sys_icache_invalidate(start: *mut core::ffi::c_void, len: usize);
    }
}
