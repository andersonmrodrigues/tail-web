//! File change notification. Each followed file is registered under a token
//! (its index); `wait` blocks until the OS reports activity on one of them.

use std::fs::File;
use std::io;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod inotify;
#[cfg(target_os = "macos")]
mod kqueue;
mod poll;

pub trait Watcher: Send {
    /// Starts watching the file behind `file`. The watch follows the open
    /// file (its inode), not its path, so a renamed file is still watched.
    /// `file` must stay open for as long as it is watched: kqueue drops the
    /// registration when the descriptor is closed.
    fn add(&mut self, token: usize, file: &File) -> io::Result<()>;

    /// Blocks until a watched file may have changed or `timeout` elapses
    /// (`None` waits indefinitely). Returns the tokens that saw activity; an
    /// empty list means "unknown", and the caller should check every file.
    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Vec<usize>>;

    /// Whether events arrive from the OS rather than from periodic checks.
    fn is_native(&self) -> bool;
}

/// Returns the native backend for this OS, or the polling one when it is not
/// available or `use_polling` asks for it.
pub fn new(use_polling: bool, interval: Duration) -> Box<dyn Watcher> {
    if !use_polling {
        match native() {
            Ok(Some(watcher)) => return watcher,
            Ok(None) => {}
            Err(e) => {
                eprintln!("rtail: cannot use native file notifications, polling instead: {e}")
            }
        }
    }
    Box::new(poll::Poll::new(interval))
}

#[cfg(target_os = "macos")]
fn native() -> io::Result<Option<Box<dyn Watcher>>> {
    Ok(Some(Box::new(kqueue::Kqueue::new()?)))
}

#[cfg(target_os = "linux")]
fn native() -> io::Result<Option<Box<dyn Watcher>>> {
    Ok(Some(Box::new(inotify::Inotify::new()?)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native() -> io::Result<Option<Box<dyn Watcher>>> {
    Ok(None)
}

/// Maps a negative libc return value to the current `errno`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    #[test]
    fn native_backend_reports_appends() {
        let mut watcher = new(false, Duration::from_secs(1));
        if !watcher.is_native() {
            return;
        }
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let reader = File::open(file.path()).unwrap();
        watcher.add(7, &reader).unwrap();

        // Nothing has happened yet, so a short wait times out empty.
        assert!(
            watcher
                .wait(Some(Duration::from_millis(50)))
                .unwrap()
                .is_empty()
        );

        file.write_all(b"hello\n").unwrap();
        file.flush().unwrap();
        let started = Instant::now();
        let tokens = watcher.wait(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(tokens, [7]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn native_backend_keeps_watching_after_rename() {
        let mut watcher = new(false, Duration::from_secs(1));
        if !watcher.is_native() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut writer = File::create(&path).unwrap();
        let reader = File::open(&path).unwrap();
        watcher.add(0, &reader).unwrap();

        std::fs::rename(&path, dir.path().join("log.1")).unwrap();
        while !watcher
            .wait(Some(Duration::from_millis(100)))
            .unwrap()
            .is_empty()
        {}

        writer.write_all(b"after rename\n").unwrap();
        assert_eq!(watcher.wait(Some(Duration::from_secs(5))).unwrap(), [0]);
    }

    #[test]
    fn polling_backend_wakes_after_interval() {
        let mut watcher = new(true, Duration::from_millis(20));
        assert!(!watcher.is_native());
        let started = Instant::now();
        assert!(watcher.wait(None).unwrap().is_empty());
        assert!(started.elapsed() >= Duration::from_millis(20));
    }
}
