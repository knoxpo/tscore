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
