//! The header strip: CPU load, memory pressure and swap.

use std::ffi::CStr;
use std::mem::size_of;

use crate::model::{MemoryPressure, Pulse, Reading, Swap};

unsafe extern "C" {
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::c_int;
}

/// CPU ticks (user, system, idle, nice) as `host_cpu_load_info` counts them.
pub type Ticks = [u32; 4];

/// Busy share of all cores between two tick readings. `None` when no time passed.
pub fn cpu_percent(prev: Ticks, now: Ticks) -> Option<f32> {
    let d: Vec<u64> = (0..4)
        .map(|i| u64::from(now[i].wrapping_sub(prev[i])))
        .collect();
    let total: u64 = d.iter().sum();
    if total == 0 {
        return None;
    }
    let idle = d[libc::CPU_STATE_IDLE as usize];
    Some(((total - idle) as f64 * 100.0 / total as f64) as f32)
}

/// `kern.memorystatus_vm_pressure_level` values from <sys/kern_memorystatus.h>.
pub fn pressure_level(v: i32) -> Reading<MemoryPressure> {
    match v {
        1 => Reading::Ready(MemoryPressure::Normal),
        2 => Reading::Ready(MemoryPressure::Warn),
        4 => Reading::Ready(MemoryPressure::Critical),
        _ => Reading::Unavailable(format!("unknown pressure level {v}")),
    }
}

/// Keeps the previous CPU reading, because load is a difference between two of them. Holds
/// a send right to the host port, released on drop.
pub struct PulseSampler {
    host: libc::mach_port_t,
    prev: Option<Ticks>,
}

impl Default for PulseSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl PulseSampler {
    pub fn new() -> Self {
        #[allow(deprecated)]
        let host = unsafe { libc::mach_host_self() };
        PulseSampler { host, prev: None }
    }

    pub fn sample(&mut self) -> Pulse {
        let cpu_percent = match self.ticks() {
            Some(now) => {
                let r = match self.prev.and_then(|p| cpu_percent(p, now)) {
                    Some(v) => Reading::Ready(v),
                    None => Reading::WarmingUp,
                };
                self.prev = Some(now);
                r
            }
            None => Reading::Unavailable("host_statistics64 failed".into()),
        };
        Pulse {
            cpu_percent,
            memory: match sysctl_value::<i32>(c"kern.memorystatus_vm_pressure_level") {
                Some(v) => pressure_level(v),
                None => Reading::Unavailable("kern.memorystatus_vm_pressure_level".into()),
            },
            swap: match sysctl_value::<libc::xsw_usage>(c"vm.swapusage") {
                Some(u) => Reading::Ready(Swap {
                    used: u.xsu_used,
                    total: u.xsu_total,
                }),
                None => Reading::Unavailable("vm.swapusage".into()),
            },
        }
    }

    #[allow(deprecated)]
    fn ticks(&self) -> Option<Ticks> {
        let mut info = libc::host_cpu_load_info { cpu_ticks: [0; 4] };
        let mut count = (size_of::<libc::host_cpu_load_info>() / size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        let rc = unsafe {
            libc::host_statistics64(
                self.host,
                libc::HOST_CPU_LOAD_INFO,
                (&mut info as *mut libc::host_cpu_load_info).cast(),
                &mut count,
            )
        };
        (rc == libc::KERN_SUCCESS).then_some(info.cpu_ticks)
    }
}

impl Drop for PulseSampler {
    fn drop(&mut self) {
        #[allow(deprecated)]
        unsafe {
            mach_port_deallocate(libc::mach_task_self(), self.host);
        }
    }
}

fn sysctl_value<T: Copy>(name: &CStr) -> Option<T> {
    let mut v = std::mem::MaybeUninit::<T>::zeroed();
    let mut len = size_of::<T>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            v.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == size_of::<T>()).then(|| unsafe { v.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_the_busy_share_of_the_tick_delta() {
        assert_eq!(cpu_percent([10, 10, 80, 0], [40, 20, 140, 0]), Some(40.0));
        assert_eq!(cpu_percent([1, 1, 1, 1], [1, 1, 1, 1]), None);
        assert_eq!(cpu_percent([u32::MAX, 0, 0, 0], [9, 0, 10, 0]), Some(50.0));
    }

    #[test]
    fn pressure_levels_map_and_unknown_ones_say_so() {
        assert_eq!(pressure_level(1), Reading::Ready(MemoryPressure::Normal));
        assert_eq!(pressure_level(4), Reading::Ready(MemoryPressure::Critical));
        assert!(matches!(pressure_level(3), Reading::Unavailable(_)));
    }

    #[test]
    fn dropping_a_sampler_releases_its_host_port() {
        unsafe extern "C" {
            fn mach_port_get_refs(
                task: libc::mach_port_t,
                name: libc::mach_port_t,
                right: u32,
                refs: *mut u32,
            ) -> libc::c_int;
        }
        const MACH_PORT_RIGHT_SEND: u32 = 0;
        let held = PulseSampler::new();
        let host = held.host;
        let refs = || {
            let mut n = 0;
            #[allow(deprecated)]
            let rc = unsafe {
                mach_port_get_refs(libc::mach_task_self(), host, MACH_PORT_RIGHT_SEND, &mut n)
            };
            assert_eq!(rc, libc::KERN_SUCCESS);
            n
        };
        let before = refs();
        for _ in 0..200 {
            drop(PulseSampler::new());
        }
        // Other tests may hold a sampler meanwhile, so allow a few; a leak would add 200.
        assert!(refs() < before + 20, "{before} -> {}", refs());
        drop(held);
    }

    #[test]
    fn first_cpu_reading_warms_up_and_the_second_is_ready() {
        let mut s = PulseSampler::new();
        let first = s.sample();
        assert_eq!(first.cpu_percent, Reading::WarmingUp);
        assert!(
            matches!(first.memory, Reading::Ready(_)),
            "{:?}",
            first.memory
        );
        assert!(matches!(first.swap, Reading::Ready(_)), "{:?}", first.swap);
        // The kernel's tick counters move in steps, and on a loaded machine a fixed wait can
        // end before the next one. Wait for them to move, then the next reading must be ready.
        let first_ticks = s.prev.expect("the first reading kept its ticks");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.ticks() == Some(first_ticks) {
            assert!(std::time::Instant::now() < until, "CPU ticks never moved");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        match s.sample().cpu_percent {
            Reading::Ready(v) => assert!((0.0..=100.0).contains(&v)),
            other => panic!("second reading {other:?}"),
        }
    }
}
