//! Platform-specific thread and process Quality-of-Service (QoS) helpers
//! to steer background work onto Efficiency Cores (E-cores) and reduce
//! battery consumption on hybrid CPUs (Windows EcoQoS, macOS Apple Silicon
//! QoS classes, Linux `uclamp_max` + `nice`).

/// Marks the current thread as an efficiency/utility worker so the OS
/// scheduler prefers Efficiency Cores (E-cores) when available.
pub fn set_current_thread_efficiency() {
    #[cfg(windows)]
    windows_impl::set_thread_eco_qos(false);

    #[cfg(target_os = "macos")]
    macos_impl::set_thread_qos_utility();

    #[cfg(target_os = "linux")]
    linux_impl::set_thread_efficiency(384, 5);
}

/// Marks the current thread as a low-priority background worker (strictly
/// preferring E-cores and minimal power states).
pub fn set_current_thread_background() {
    #[cfg(windows)]
    windows_impl::set_thread_eco_qos(true);

    #[cfg(target_os = "macos")]
    macos_impl::set_thread_qos_background();

    #[cfg(target_os = "linux")]
    linux_impl::set_thread_efficiency(256, 10);
}

/// Toggles process-wide efficiency mode (EcoQoS on Windows).
/// Useful when the application is hidden to the system tray.
pub fn set_process_efficiency_mode(enabled: bool) {
    #[cfg(windows)]
    windows_impl::set_process_eco_qos(enabled);

    #[cfg(not(windows))]
    let _ = enabled;
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::c_void;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation, SetThreadInformation, SetThreadPriority,
        THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST,
        ThreadPowerThrottling,
    };

    pub fn set_thread_eco_qos(background: bool) {
        let state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        };
        // SAFETY: `state` is a valid, initialized `THREAD_POWER_THROTTLING_STATE`
        // passed to the current thread handle with its exact byte size.
        unsafe {
            let thread = GetCurrentThread();
            let _ = SetThreadInformation(
                thread,
                ThreadPowerThrottling,
                (&raw const state).cast::<c_void>(),
                std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            );
            let priority = if background {
                THREAD_PRIORITY_LOWEST
            } else {
                THREAD_PRIORITY_BELOW_NORMAL
            };
            let _ = SetThreadPriority(thread, priority);
        }
    }

    pub fn set_process_eco_qos(enabled: bool) {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if enabled {
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            } else {
                0
            },
        };
        // SAFETY: `state` is a valid, initialized `PROCESS_POWER_THROTTLING_STATE`
        // passed to the current process handle with its exact byte size.
        unsafe {
            let _ = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const state).cast::<c_void>(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_impl {
    pub fn set_thread_qos_utility() {
        // SAFETY: `pthread_set_qos_class_self_np` operates only on the calling thread.
        unsafe {
            let _ = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
        }
    }

    pub fn set_thread_qos_background() {
        // SAFETY: `pthread_set_qos_class_self_np` operates only on the calling thread.
        unsafe {
            let _ = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    /// Linux `struct sched_attr` layout for `SYS_sched_setattr` (kernel >= 5.3).
    #[repr(C)]
    struct SchedAttr {
        size: u32,
        sched_policy: u32,
        sched_flags: u64,
        sched_nice: i32,
        sched_priority: u32,
        sched_runtime: u64,
        sched_deadline: u64,
        sched_period: u64,
        sched_util_min: u32,
        sched_util_max: u32,
    }

    const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
    const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
    const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;

    pub fn set_thread_efficiency(uclamp_max: u32, nice: i32) {
        // SAFETY: `setpriority` with `PRIO_PROCESS` and `0` adjusts only the calling thread's
        // nice value on Linux (NPTL). Failures (e.g. permissions) are safely ignored.
        unsafe {
            let _ = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
        }

        let attr = SchedAttr {
            size: std::mem::size_of::<SchedAttr>() as u32,
            sched_policy: 0,
            sched_flags: SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_UTIL_CLAMP_MAX,
            sched_nice: 0,
            sched_priority: 0,
            sched_runtime: 0,
            sched_deadline: 0,
            sched_period: 0,
            sched_util_min: 0,
            sched_util_max: uclamp_max.min(1024),
        };
        // SAFETY: `SYS_sched_setattr` with pid 0 applies `attr` to the calling thread.
        // Kernels without uclamp support simply return EINVAL/ENOSYS which we ignore.
        unsafe {
            let _ = libc::syscall(
                libc::SYS_sched_setattr,
                0 as libc::pid_t,
                (&raw const attr).cast::<libc::c_void>(),
                0u32,
            );
        }
    }
}
