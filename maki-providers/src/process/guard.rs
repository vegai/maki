use std::io;
use std::mem::zeroed;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;

use libc::{
    _exit, AF_UNIX, EINTR, O_CLOEXEC, O_DIRECTORY, O_RDONLY, SIG_IGN, SIGCHLD, SIGHUP, SIGINT,
    SIGQUIT, SIGTERM, SOCK_CLOEXEC, SOCK_STREAM, SYS_clone, SYS_close_range, SYS_getdents64,
    SYS_socketpair, close, open, read, setpgid, sigaction, sigemptyset, syscall, write,
};
use rustix::io::Errno;
use rustix::process::{Signal, getpgrp, getpid, kill_process_group};

const FD_DIRECTORY: &[u8] = b"/proc/self/fd\0";
const DIRECTORY_BUFFER: usize = 4096;
const RECORD_LENGTH_OFFSET: usize = 16;
const RECORD_NAME_OFFSET: usize = 19;

/// The guard shares the worker's group and reserves its id after the leader exits. Only
/// maki holds the lifetime socket's other endpoint.
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
            let group = getpid();
            // Inherited atfork handlers can deadlock inside pre_exec.
            #[cfg(not(target_arch = "s390x"))]
            let pid = syscall(SYS_clone, SIGCHLD, 0usize, 0usize, 0usize, 0usize);
            #[cfg(target_arch = "s390x")]
            let pid = syscall(SYS_clone, 0usize, SIGCHLD, 0usize, 0usize, 0usize);
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
                    let error = setup(reader, ready)
                        .err()
                        .and_then(|error| error.raw_os_error())
                        .unwrap_or(0);
                    let sent = loop {
                        let sent = write(ready, (&error as *const i32).cast(), size_of::<i32>());
                        if sent >= 0 || io::Error::last_os_error().raw_os_error() != Some(EINTR) {
                            break sent;
                        }
                    };
                    if sent != size_of::<i32>() as isize || error != 0 {
                        _exit(1);
                    }
                    close(ready);
                    let mut byte = 0u8;
                    loop {
                        match read(reader, (&mut byte as *mut u8).cast(), 1) {
                            0 => break,
                            -1 if io::Error::last_os_error().raw_os_error() == Some(EINTR) => {}
                            -1 => break,
                            _ => {}
                        }
                    }
                    let _ = kill_process_group(group, Signal::KILL);
                    _exit(0);
                }
                _ => {
                    close(ready[1]);
                    let result = read_ready(ready[0]);
                    close(ready[0]);
                    result?;
                }
            }
            Ok(())
        });
    }
    Ok(lifetime)
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
    // SAFETY: all arguments are integer descriptors and flags.
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
    use std::io;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    const CHILD: &str = "echo ready; exec sleep 30";
    const IGNORES_TERM: &str = "trap '' TERM; echo ready; exec sleep 30";
    const READY: &str = "ready\n";

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
