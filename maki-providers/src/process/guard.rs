use std::ffi::c_long;
use std::io;
use std::mem::zeroed;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;

use libc::{
    _exit, AF_UNIX, EINTR, F_GETFD, O_CLOEXEC, O_DIRECTORY, O_RDONLY, SIG_IGN, SIGCHLD, SIGHUP,
    SIGINT, SIGQUIT, SIGTERM, SOCK_CLOEXEC, SOCK_STREAM, SYS_clone, SYS_close_range,
    SYS_getdents64, SYS_socketpair, close, dup2, execve, fcntl, open, read, setpgid, sigaction,
    sigemptyset, syscall, waitpid, write,
};
use rustix::io::Errno;
use rustix::process::{getpgrp, getpid};

const SHELL: &[u8] = b"/bin/sh\0";
const SHELL_NAME: &[u8] = b"sh\0";
const SHELL_FLAG: &[u8] = b"-c\0";
const WATCH: &[u8] = b"printf '\\0\\0\\0\\0'; exec 1>&-; IFS= read -r lifetime; kill -KILL 0\0";
const FD_DIRECTORY: &[u8] = b"/proc/self/fd\0";
const DIRECTORY_BUFFER: usize = 4096;
const RECORD_LENGTH_OFFSET: usize = 16;
const RECORD_NAME_OFFSET: usize = 19;

/// The guard shares the worker's group and reserves its id after the leader exits. Only
/// maki holds the lifetime socket's other endpoint.
/// Keep the returned socket alive until cleanup finishes. Dropping it stops
/// the worker's process group, even if the leader has already exited.
pub fn bind(command: &mut Command) -> io::Result<UnixStream> {
    bind_with_setup(command, watcher_setup)
}

fn bind_with_setup(
    command: &mut Command,
    setup: unsafe fn(i32, i32) -> io::Result<()>,
) -> io::Result<UnixStream> {
    let (reader, lifetime) = UnixStream::pair()?;
    // SAFETY: the child uses only syscalls and async-signal-safe functions.
    // It never allocates, unwinds or runs inherited destructors.
    unsafe {
        command.pre_exec(move || {
            if getpgrp() != getpid() && setpgid(0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut ready = [0i32; 2];
            if syscall(
                SYS_socketpair,
                AF_UNIX,
                SOCK_STREAM | SOCK_CLOEXEC,
                0,
                ready.as_mut_ptr(),
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            // Inherited atfork handlers can deadlock inside pre_exec.
            let pid = syscall_clone();
            match pid {
                -1 => {
                    let error = io::Error::last_os_error();
                    close(ready[0]);
                    close(ready[1]);
                    return Err(error);
                }
                0 => {
                    let reader = reader.as_raw_fd();
                    let ready = ready[1];
                    let error: io::Result<()> = setup(reader, ready).and_then(|()| {
                        // Reparent the helper so a worker can wait for all of its children.
                        let detached = syscall_clone();
                        if detached < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        if detached > 0 {
                            _exit(0);
                        }
                        if dup2(reader, 0) < 0 || dup2(ready, 1) < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        close_inherited(0, 1)?;
                        let args = [
                            SHELL_NAME.as_ptr(),
                            SHELL_FLAG.as_ptr(),
                            WATCH.as_ptr(),
                            ptr::null(),
                        ];
                        let env = [ptr::null::<u8>()];
                        execve(
                            SHELL.as_ptr().cast(),
                            args.as_ptr().cast(),
                            env.as_ptr().cast(),
                        );
                        Err(io::Error::last_os_error())
                    });
                    let error = error
                        .err()
                        .and_then(|error| error.raw_os_error())
                        .unwrap_or(0);
                    // Setup failures happen before descriptor duplication; exec failures after it.
                    let reply = if fcntl(ready, F_GETFD) >= 0 { ready } else { 1 };
                    let _ = write(reply, (&error as *const i32).cast(), size_of::<i32>());
                    _exit(1);
                }
                _ => {
                    close(ready[1]);
                    let result = read_ready(ready[0]);
                    close(ready[0]);
                    while waitpid(pid as i32, ptr::null_mut(), 0) < 0 {
                        if io::Error::last_os_error().raw_os_error() != Some(EINTR) {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    result?;
                }
            }
            Ok(())
        });
    }
    Ok(lifetime)
}

unsafe fn syscall_clone() -> c_long {
    // SAFETY: this raw fork avoids inherited atfork handlers; callers use only async-signal-safe operations.
    #[cfg(not(target_arch = "s390x"))]
    unsafe {
        syscall(SYS_clone, SIGCHLD, 0usize, 0usize, 0usize, 0usize)
    }
    #[cfg(target_arch = "s390x")]
    unsafe {
        syscall(SYS_clone, 0usize, SIGCHLD, 0usize, 0usize, 0usize)
    }
}

unsafe fn read_ready(fd: i32) -> io::Result<()> {
    // SAFETY: the destination is the remaining bytes of a stack integer.
    unsafe {
        let mut error = 0i32;
        let mut received = 0;
        while received < size_of::<i32>() {
            let destination = (&mut error as *mut i32).cast::<u8>().add(received);
            match read(fd, destination.cast(), size_of::<i32>() - received) {
                0 => return Err(io::Error::from_raw_os_error(Errno::IO.raw_os_error())),
                -1 if io::Error::last_os_error().raw_os_error() == Some(EINTR) => continue,
                -1 => return Err(io::Error::last_os_error()),
                count => received += count as usize,
            }
        }
        if error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error))
        }
    }
}

unsafe fn watcher_setup(reader: i32, ready: i32) -> io::Result<()> {
    // SAFETY: sigaction reads a fully initialized stack record and the remaining calls use
    // integer descriptors. This child cannot unwind through the caller.
    unsafe {
        let mut action: sigaction = zeroed();
        action.sa_sigaction = SIG_IGN;
        sigemptyset(&mut action.sa_mask);
        for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
            if sigaction(signal, &action, ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        close_inherited(reader, ready)
    }
}

// The watcher must close output pipes and the spawn error pipe before waiting. Preserve
// only its lifetime reader and startup reply. Old kernels scan open descriptors instead
// of closing every possible descriptor up to RLIMIT_NOFILE.
unsafe fn close_inherited(reader: i32, ready: i32) -> io::Result<()> {
    let (first, last) = (reader.min(ready) as u32, reader.max(ready) as u32);
    // SAFETY: close_range uses integer descriptors and flags. getdents64 writes at most
    // buffer.len() bytes into the live buffer and accepts byte alignment. Records are read
    // as bytes, with their lengths and name bounds checked before indexing.
    unsafe {
        if (first == 0 || syscall(SYS_close_range, 0u32, first - 1, 0u32) == 0)
            && (first + 1 == last || syscall(SYS_close_range, first + 1, last - 1, 0u32) == 0)
            && syscall(SYS_close_range, last + 1, u32::MAX, 0u32) == 0
        {
            return Ok(());
        }
        let directory = open(
            FD_DIRECTORY.as_ptr().cast(),
            O_RDONLY | O_DIRECTORY | O_CLOEXEC,
        );
        if directory < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = [0u8; DIRECTORY_BUFFER];
        let result = loop {
            let bytes = syscall(SYS_getdents64, directory, buffer.as_mut_ptr(), buffer.len());
            if bytes == 0 {
                break Ok(());
            }
            if bytes < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                    continue;
                }
                break Err(io::Error::last_os_error());
            }
            let mut offset = 0;
            while offset + RECORD_NAME_OFFSET < bytes as usize {
                let length = u16::from_ne_bytes([
                    buffer[offset + RECORD_LENGTH_OFFSET],
                    buffer[offset + RECORD_LENGTH_OFFSET + 1],
                ]) as usize;
                if length <= RECORD_NAME_OFFSET || offset + length > bytes as usize {
                    close(directory);
                    return Err(io::Error::from_raw_os_error(Errno::IO.raw_os_error()));
                }
                let name = &buffer[offset + RECORD_NAME_OFFSET..offset + length];
                let mut fd = 0i32;
                let mut digits = false;
                for &byte in name.iter().take_while(|&&byte| byte != 0) {
                    if !byte.is_ascii_digit() {
                        digits = false;
                        break;
                    }
                    digits = true;
                    fd = fd.saturating_mul(10).saturating_add(i32::from(byte - b'0'));
                }
                if digits && fd != reader && fd != ready && fd != directory {
                    close(fd);
                }
                offset += length;
            }
        };
        close(directory);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{bind, bind_with_setup};
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, getpgrp, kill_process_group};
    use std::env;
    use std::io;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::ptr;
    use std::time::Duration;
    use wait_timeout::ChildExt;

    const CHILD: &str = "echo ready; exec sleep 30";
    const IGNORES_TERM: &str = "trap '' TERM; echo ready; exec sleep 30";
    const READY: &str = "ready\n";
    const WAIT_CHILD: &str = "process::guard::tests::worker_without_children";
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    unsafe fn exits_before_ready(_: i32, _: i32) -> io::Result<()> {
        // SAFETY: this hook runs only in the forked watcher.
        unsafe {
            libc::_exit(1);
        }
    }

    #[test]
    fn a_watcher_that_exits_during_setup_refuses_the_spawn() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", CHILD]).stdout(Stdio::piped());
        let _lifetime = bind_with_setup(&mut command, exits_before_ready).unwrap();
        let error = command.spawn().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
    }

    #[test]
    fn a_termination_signal_cannot_disable_the_lifetime_guard() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", IGNORES_TERM]).stdout(Stdio::piped());
        let lifetime = bind(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, READY);
        kill_process_group(Pid::from_raw(child.id() as i32).unwrap(), Signal::TERM).unwrap();
        drop(lifetime);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[test]
    #[ignore = "subprocess fixture for a_worker_can_wait_for_all_its_children"]
    fn worker_without_children() {
        // SAFETY: this isolated fixture checks whether pre_exec left it any children.
        let waited = unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(waited, -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(Errno::CHILD.raw_os_error())
        );
    }

    #[test]
    fn a_worker_can_wait_for_all_its_children() {
        let mut command = Command::new(env::current_exe().unwrap());
        command.args(["--ignored", "--exact", WAIT_CHILD]);
        let lifetime = bind(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let status = child.wait_timeout(TEST_TIMEOUT).unwrap();
        if status.is_none() {
            drop(lifetime);
            let _ = child.wait();
            panic!("worker waited for the process guard");
        }
        assert!(status.unwrap().success());
        drop(lifetime);
    }

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
