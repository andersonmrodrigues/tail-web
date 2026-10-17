//! macOS backend: one `EVFILT_VNODE` filter per open file descriptor.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::time::Duration;

use super::{Watcher, check};

const MAX_EVENTS: usize = 64;

pub struct Kqueue {
    kq: OwnedFd,
}

impl Kqueue {
    pub fn new() -> io::Result<Self> {
        // SAFETY: kqueue() takes no arguments and returns a new descriptor or -1.
        let fd = check(unsafe { libc::kqueue() })?;
        // SAFETY: `fd` was just returned by kqueue() and is owned by nobody else.
        let kq = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { kq })
    }
}

impl Watcher for Kqueue {
    fn add(&mut self, token: usize, file: &File) -> io::Result<()> {
        let change = libc::kevent {
            ident: file.as_raw_fd() as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            // EV_CLEAR resets the state after each delivery, so a busy file
            // wakes us once per wait instead of on every kevent() call.
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE
                | libc::NOTE_EXTEND
                | libc::NOTE_ATTRIB
                | libc::NOTE_DELETE
                | libc::NOTE_RENAME
                | libc::NOTE_REVOKE,
            data: 0,
            udata: token as *mut libc::c_void,
        };
        // SAFETY: `change` is a valid kevent, the change list has length 1 and
        // the event list is empty, so the kernel writes nothing back.
        check(unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                &change,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        })?;
        Ok(())
    }

    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Vec<usize>> {
        let timeout = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs().try_into().unwrap_or(libc::time_t::MAX),
            tv_nsec: d.subsec_nanos().into(),
        });
        let timeout_ptr = timeout
            .as_ref()
            .map_or(ptr::null(), |t| t as *const libc::timespec);

        // SAFETY: kevent is plain old data, so the all-zero value is valid.
        let mut events: [libc::kevent; MAX_EVENTS] = unsafe { std::mem::zeroed() };
        // SAFETY: the event list points to MAX_EVENTS writable kevents and
        // `timeout_ptr` is null or points to a timespec that outlives the call.
        let ret = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                ptr::null(),
                0,
                events.as_mut_ptr(),
                MAX_EVENTS as libc::c_int,
                timeout_ptr,
            )
        };
        let n = match check(ret) {
            Ok(n) => n as usize,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        let mut tokens: Vec<usize> = events[..n]
            .iter()
            .map(|event| event.udata as usize)
            .collect();
        tokens.sort_unstable();
        tokens.dedup();
        Ok(tokens)
    }

    fn is_native(&self) -> bool {
        true
    }
}
