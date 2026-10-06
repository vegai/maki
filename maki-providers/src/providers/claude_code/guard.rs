use std::ffi::c_long;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

use libc::{
    _SC_OPEN_MAX, _exit, STDIN_FILENO, STDOUT_FILENO, SYS_close_range, close, dup2, fork, syscall,
    sysconf,
};
use rustix::io::{Errno, read};
use rustix::process::{Signal, getpid, kill_process_group};

/// The guard shares the worker's group and reserves its id after the leader exits. Only
/// maki holds the lifetime socket's other endpoint.
pub(super) fn bind(command: &mut Command) -> io::Result<UnixStream> {
    let (reader, lifetime) = UnixStream::pair()?;
    // SAFETY: sysconf has no pointer arguments.
    let max_fd = unsafe { sysconf(_SC_OPEN_MAX) };
    if max_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the child uses only syscalls and async-signal-safe functions.
    // It never allocates, unwinds or runs inherited destructors.
    unsafe {
        command.pre_exec(move || {
            let group = getpid();
            match fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => {
                    if dup2(reader.as_raw_fd(), STDIN_FILENO) >= 0 {
                        // The spawn error pipe must close before the lifetime wait. Otherwise
                        // the parent cannot complete the spawn.
                        if syscall(SYS_close_range, STDOUT_FILENO as u32, u32::MAX, 0u32) < 0 {
                            for fd in c_long::from(STDOUT_FILENO)..max_fd {
                                close(fd as i32);
                            }
                        }
                        let input = BorrowedFd::borrow_raw(STDIN_FILENO);
                        let mut byte = [0u8];
                        loop {
                            match read(input, &mut byte) {
                                Ok(0) => break,
                                Ok(_) | Err(Errno::INTR) => {}
                                Err(_) => break,
                            }
                        }
                    }
                    let _ = kill_process_group(group, Signal::KILL);
                    _exit(0);
                }
                _ => {}
            }
            Ok(())
        });
    }
    Ok(lifetime)
}
