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
    static mach_task_self_: c_uint;
    fn task_threads(task: c_uint, list: *mut *mut c_uint, count: *mut c_uint) -> c_int;
    fn mach_port_deallocate(task: c_uint, name: c_uint) -> c_int;
    fn vm_deallocate(task: c_uint, address: usize, size: usize) -> c_int;
    fn pthread_from_mach_thread_np(thread: c_uint) -> libc::pthread_t;
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
    // SAFETY: no arguments; the port names the calling thread.
    set_time_constraint(unsafe { mach_thread_self() }, period_ns, computation_ns)
}

/// Puts every thread of this process named `name` in the realtime band, as
/// `make_realtime` does for the calling thread. Returns how many were changed.
pub fn make_named_realtime(name: &str, period_ns: u64, computation_ns: u64) -> usize {
    let mut list: *mut c_uint = std::ptr::null_mut();
    let mut count: c_uint = 0;
    // SAFETY: out-pointers to valid locals; on success the kernel hands us `count` thread
    // ports in a vm allocation, both released below.
    unsafe {
        let task = mach_task_self_;
        if task_threads(task, &mut list, &mut count) != 0 {
            return 0;
        }
        let mut changed = 0;
        for &port in std::slice::from_raw_parts(list, count as usize) {
            let pt = pthread_from_mach_thread_np(port);
            let mut buf = [0 as libc::c_char; 64];
            if pt != 0
                && libc::pthread_getname_np(pt, buf.as_mut_ptr(), buf.len()) == 0
                && std::ffi::CStr::from_ptr(buf.as_ptr()).to_bytes() == name.as_bytes()
                && set_time_constraint(port, period_ns, computation_ns)
            {
                changed += 1;
            }
            mach_port_deallocate(task, port);
        }
        vm_deallocate(task, list as usize, count as usize * std::mem::size_of::<c_uint>());
        changed
    }
}

fn set_time_constraint(thread: c_uint, period_ns: u64, computation_ns: u64) -> bool {
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
            thread,
            THREAD_TIME_CONSTRAINT_POLICY,
            &mut p as *mut TimeConstraintPolicy as *mut c_int,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    r == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn named_thread_becomes_realtime() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::Builder::new().name("rt-test-thread".into()).spawn(move || rx.recv()).unwrap();
        // The name is set from inside the new thread; give it a moment.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(super::make_named_realtime("rt-test-thread", 1_000_000, 300_000), 1);
        assert_eq!(super::make_named_realtime("no-such-thread", 1_000_000, 300_000), 0);
        tx.send(()).unwrap();
        t.join().unwrap().unwrap();
    }
}
