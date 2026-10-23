//! Mach time and realtime thread scheduling.

use std::os::raw::{c_int, c_uint};

#[repr(C)]
struct TimebaseInfo {
    numer: u32,
    denom: u32,
}

#[repr(C)]
struct TimeConstraintPolicy {
    period: u32,
    computation: u32,
    constraint: u32,
    preemptible: c_int,
}

const THREAD_TIME_CONSTRAINT_POLICY: c_uint = 2;
const THREAD_TIME_CONSTRAINT_POLICY_COUNT: c_uint = 4;

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut TimebaseInfo) -> c_int;
    fn mach_thread_self() -> c_uint;
    fn thread_policy_set(thread: c_uint, flavor: c_uint, policy: *mut c_int, count: c_uint) -> c_int;
}

pub fn now_ticks() -> u64 {
    // SAFETY: no arguments, always valid.
    unsafe { mach_absolute_time() }
}

/// Nanoseconds per mach tick (41.67 on Apple Silicon, 1.0 on Intel).
pub fn ns_per_tick() -> f64 {
    let mut tb = TimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: out-pointer to a valid struct.
    unsafe { mach_timebase_info(&mut tb) };
    tb.numer as f64 / tb.denom as f64
}

/// Puts the calling thread in the realtime band. `period_ns` is the expected wake
/// interval, `computation_ns` the work per wake. Returns false if the kernel refused.
pub fn make_realtime(period_ns: u64, computation_ns: u64) -> bool {
    let tpn = ns_per_tick();
    let to_ticks = |ns: u64| (ns as f64 / tpn) as u32;
    let mut p = TimeConstraintPolicy {
        period: to_ticks(period_ns),
        computation: to_ticks(computation_ns),
        constraint: to_ticks(period_ns),
        preemptible: 1,
    };
    // SAFETY: policy struct matches thread_time_constraint_policy_data_t.
    let r = unsafe {
        thread_policy_set(
            mach_thread_self(),
            THREAD_TIME_CONSTRAINT_POLICY,
            &mut p as *mut TimeConstraintPolicy as *mut c_int,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    r == 0
}
