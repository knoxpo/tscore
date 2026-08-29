//! tsp-linux
//!
//! Linux platform layer. QoS no-op today: CFS handles placement; niceness
//! or sched_setaffinity hooks land with the NUMA milestone.

pub fn prefer_performance_cores() {}
