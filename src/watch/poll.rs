//! Fallback for platforms or filesystems without usable notifications (for
//! example NFS, where inotify misses changes made by other hosts).

use std::fs::File;
use std::io;
use std::thread;
use std::time::Duration;

use super::Watcher;

pub struct Poll {
    interval: Duration,
}

impl Poll {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Watcher for Poll {
    fn add(&mut self, _token: usize, _file: &File) -> io::Result<()> {
        Ok(())
    }

    fn remove(&mut self, _token: usize) {}

    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Vec<usize>> {
        let interval = timeout.map_or(self.interval, |t| t.min(self.interval));
        thread::sleep(interval);
        Ok(Vec::new())
    }

    fn is_native(&self) -> bool {
        false
    }
}
