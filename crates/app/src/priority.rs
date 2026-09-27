//! Scheduling hints that keep transfers at full speed. Network speed itself is out of any process's
//! hands; what the OS can take away is CPU time for TLS decryption and prompt wake-ups when data
//! arrives, which is what a busy machine (a game, a build) or power saving would otherwise do.

/// Windows 11 puts background processes (RDM in the notification area) under "EcoQoS": efficiency
/// cores, lowered clock. Opted out: sustained high-speed transfers need the CPU at full speed.
pub fn full_speed_in_background() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
        };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0, // controlled, and off
        };
        // SAFETY: our own process pseudo-handle and a correctly sized, initialised struct. Fails
        // harmlessly on Windows 10 before 1709.
        unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

/// Transfer threads (network and disk): above normal priority on Windows, so arriving data is
/// handled at once even when the machine is busy — the TCP windows stay open and the server keeps
/// sending. Linux does not let unprivileged processes raise a priority; nothing to do there.
pub fn transfer_thread() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        // SAFETY: pseudo-handle of the calling thread.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        }
    }
}
