//! Linux backend: one inotify watch per followed inode.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use super::{Watcher, check};

const MASK: u32 = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_MOVE_SELF | libc::IN_DELETE_SELF;

/// Event buffer aligned for `inotify_event`, sized for many events per read.
#[repr(C, align(8))]
struct EventBuffer([u8; 16 * 1024]);

pub struct Inotify {
    fd: OwnedFd,
    /// inotify returns the same watch descriptor for every path that names
    /// an inode already watched, so one descriptor can map to several files.
    tokens: HashMap<libc::c_int, Vec<usize>>,
    buffer: Box<EventBuffer>,
}

impl Inotify {
    pub fn new() -> io::Result<Self> {
        // SAFETY: inotify_init1 takes only flags and returns a descriptor or -1.
        let fd = check(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) })?;
        // SAFETY: `fd` was just returned by inotify_init1 and is owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            fd,
            tokens: HashMap::new(),
            buffer: Box::new(EventBuffer([0; 16 * 1024])),
        })
    }

    /// Reads every queued event without blocking. Returns `None` when the
    /// kernel queue overflowed and events were lost.
    fn drain(&mut self) -> io::Result<Option<Vec<usize>>> {
        let mut tokens = Vec::new();
        let mut overflowed = false;
        loop {
            let buf = &mut self.buffer.0;
            // SAFETY: `buf` is a writable buffer of `buf.len()` bytes.
            let ret =
                unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            let len = match ret {
                n if n > 0 => n as usize,
                0 => break,
                _ => match io::Error::last_os_error() {
                    e if e.kind() == io::ErrorKind::WouldBlock => break,
                    e if e.kind() == io::ErrorKind::Interrupted => continue,
                    e => return Err(e),
                },
            };

            let mut offset = 0;
            while offset + size_of::<libc::inotify_event>() <= len {
                // SAFETY: the kernel wrote a complete inotify_event header at
                // `offset`; read_unaligned does not rely on its alignment.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast()) };
                offset += size_of::<libc::inotify_event>() + event.len as usize;

                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    overflowed = true;
                } else if event.mask & libc::IN_IGNORED != 0 {
                    // The inode is gone and the kernel removed the watch.
                    self.tokens.remove(&event.wd);
                } else if let Some(watchers) = self.tokens.get(&event.wd) {
                    tokens.extend_from_slice(watchers);
                }
            }
        }

        if overflowed {
            return Ok(None);
        }
        tokens.sort_unstable();
        tokens.dedup();
        Ok(Some(tokens))
    }
}

impl Watcher for Inotify {
    fn add(&mut self, token: usize, file: &File) -> io::Result<()> {
        // Watching the /proc magic link targets the inode we have open, even
        // when the original path was renamed, replaced or never existed (stdin).
        let path = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .expect("formatted path contains no NUL bytes");
        // SAFETY: `path` is a valid NUL-terminated string for the whole call.
        let wd =
            check(unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), MASK) })?;
        self.tokens.entry(wd).or_default().push(token);
        Ok(())
    }

    /// The watch is on the inode, which outlives our descriptor (a rotated
    /// log.1 keeps getting written), so it must be removed explicitly.
    fn remove(&mut self, token: usize) {
        let Some((&wd, _)) = self
            .tokens
            .iter()
            .find(|(_, tokens)| tokens.contains(&token))
        else {
            return;
        };
        let tokens = self
            .tokens
            .get_mut(&wd)
            .expect("watch descriptor was just found");
        tokens.retain(|&t| t != token);
        if tokens.is_empty() {
            self.tokens.remove(&wd);
            // SAFETY: plain syscall on our inotify descriptor. A failure means
            // the kernel already dropped the watch (IN_IGNORED is queued).
            unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
        }
    }

    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Vec<usize>> {
        let timeout_ms = timeout.map_or(-1, |d| {
            d.as_millis().min(libc::c_int::MAX as u128) as libc::c_int
        });
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` is a single valid pollfd that outlives the call.
        match check(unsafe { libc::poll(&mut pollfd, 1, timeout_ms) }) {
            Ok(0) => return Ok(Vec::new()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(Vec::new()),
            Err(e) => return Err(e),
        }
        // On overflow every file may have changed; an empty list says so.
        Ok(self.drain()?.unwrap_or_default())
    }

    fn is_native(&self) -> bool {
        true
    }
}
