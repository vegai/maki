use std::ffi::c_long;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;

use libc::{
    _SC_OPEN_MAX, _exit, STDIN_FILENO, STDOUT_FILENO, SYS_close_range, close, dup2, execve, fork,
    syscall, sysconf,
};
use rustix::process::{Signal, getpid, kill_process_group};

/// The guard shares the worker's group and reserves its id after the leader exits. Only
/// maki holds the lifetime socket's other endpoint.
pub fn bind(command: &mut Command) -> io::Result<UnixStream> {
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
                        let argv = [
                            c"sh".as_ptr(),
                            c"-c".as_ptr(),
                            c"while IFS= read -r line; do :; done; kill -KILL 0".as_ptr(),
                            ptr::null(),
                        ];
                        let environ = [ptr::null()];
                        execve(c"/bin/sh".as_ptr(), argv.as_ptr(), environ.as_ptr());
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
