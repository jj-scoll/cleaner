//! Process-level helpers.

/// nice 10 and the idle IO class (spec §6.8); failures are ignored. On Linux
/// both apply to the calling thread only, so worker threads call this too.
pub fn lower_priority() {
    // SAFETY: plain syscalls on the calling thread with constant arguments.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        // ioprio_set(IOPRIO_WHO_PROCESS, self, IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT)
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
    }
}
