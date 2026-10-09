use std::ffi::c_long;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;

use libc::{
    _SC_OPEN_MAX, _exit, SIGCHLD, STDIN_FILENO, STDOUT_FILENO, SYS_clone, SYS_close_range, close,
    dup2, execve, setpgid, syscall, sysconf,
};
use rustix::process::{Signal, getpgrp, getpid, kill_process_group};

/// The guard shares the worker's group and reserves its id after the leader exits. Only
/// maki holds the lifetime socket's other endpoint.
pub fn bind(command: &mut Command) -> io::Result<UnixStream> {
    fs::metadata("/bin/sh").map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("process guard needs /bin/sh: {error}"),
        )
    })?;
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
            if getpgrp() != getpid() && setpgid(0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            let group = getpid();
            // Inherited atfork handlers can deadlock inside pre_exec.
            #[cfg(not(target_arch = "s390x"))]
            let pid = syscall(SYS_clone, SIGCHLD, 0usize, 0usize, 0usize, 0usize);
            #[cfg(target_arch = "s390x")]
            let pid = syscall(SYS_clone, 0usize, SIGCHLD, 0usize, 0usize, 0usize);
            match pid {
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

#[cfg(test)]
mod tests {
    use super::bind;
    use rustix::process::getpgrp;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    const CHILD: &str = "echo ready; exec sleep 30";
    const READY: &str = "ready\n";

    #[test]
    fn guard_establishes_its_own_group_without_caller_setup() {
        let parent_group = getpgrp();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", CHILD]).stdout(Stdio::piped());
        let lifetime = bind(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, READY);
        drop(lifetime);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_eq!(getpgrp(), parent_group);
    }
}
