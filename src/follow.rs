//! Follows files after the initial tail has been printed, sleeping on OS
//! notifications until they grow.
//!
//! In descriptor mode (`-f`) a file is followed through the descriptor opened
//! at start, wherever it is renamed to. In name mode (`-F`) the path is what
//! matters: after log rotation replaces the file, the new one is opened and
//! followed from its first byte.

use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;

use crate::describe;
use crate::watch::{self, Watcher};

const CHUNK: usize = 64 * 1024;

pub struct Options {
    pub sleep_interval: Duration,
    pub pid: Option<libc::pid_t>,
    pub use_polling: bool,
    /// Follow the path rather than the descriptor (`--follow=name`).
    pub by_name: bool,
    /// Keep trying paths that are missing or become inaccessible (`--retry`).
    pub retry: bool,
}

/// A followed file: its name for messages, its path (none for stdin) and, while
/// it is open, the descriptor positioned right after the last byte printed.
pub struct Followed {
    pub name: String,
    path: Option<PathBuf>,
    open: Option<OpenFile>,
    /// Without `--retry`, a file that becomes inaccessible is dropped.
    gone: bool,
}

struct OpenFile {
    file: File,
    offset: u64,
    id: FileId,
}

/// Device and inode: what tells a rotated file apart from the one we had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

impl Followed {
    /// `file` must be positioned where the initial tail stopped printing.
    pub fn new(name: String, path: Option<PathBuf>, mut file: File) -> io::Result<Self> {
        let offset = file.stream_position()?;
        let id = FileId::of(&file.metadata()?);
        Ok(Self {
            name,
            path,
            open: Some(OpenFile { file, offset, id }),
            gone: false,
        })
    }

    /// A path that could not be opened at start, to be retried until it appears.
    pub fn missing(name: String, path: PathBuf) -> Self {
        Self {
            name,
            path: Some(path),
            open: None,
            gone: false,
        }
    }
}

/// Where followed output goes: stdout, or the web server's subscribers.
pub trait Sink {
    fn write(&mut self, index: usize, name: &str, data: &[u8]) -> io::Result<()>;

    fn flush(&mut self) -> io::Result<()>;

    /// A non-fatal event about one file, such as truncation or rotation.
    fn notice(&mut self, _index: usize, message: &str) {
        eprintln!("rtail: {message}");
    }
}

/// Prints new data to a writer, with a `==> name <==` header whenever the
/// output switches to a different file, the way GNU tail does.
pub struct WriterSink<'a, W: Write> {
    out: &'a mut W,
    headers: bool,
    last: Option<usize>,
}

impl<'a, W: Write> WriterSink<'a, W> {
    /// `last` is the file whose output was printed most recently, so the first
    /// chunk from that same file does not repeat its header.
    pub fn new(out: &'a mut W, headers: bool, last: Option<usize>) -> Self {
        Self { out, headers, last }
    }
}

impl<W: Write> Sink for WriterSink<'_, W> {
    fn write(&mut self, index: usize, name: &str, data: &[u8]) -> io::Result<()> {
        if self.headers && self.last != Some(index) {
            write!(self.out, "\n==> {name} <==\n")?;
        }
        self.last = Some(index);
        self.out.write_all(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Follows `files` until the process given by `--pid` exits, every file is
/// gone, or forever. Returns `Ok(false)` when it stopped because no file was
/// left to follow, and an error only when the sink fails (a closed stdout).
pub fn run(files: Vec<Followed>, opts: &Options, sink: &mut impl Sink) -> io::Result<bool> {
    let watcher = watch::new(opts.use_polling, opts.sleep_interval);
    let mut follower = Follower {
        poll_every_wake: !watcher.is_native(),
        watcher,
        opts,
        buf: vec![0; CHUNK],
    };
    follower.run(files, sink)
}

struct Follower<'a> {
    watcher: Box<dyn Watcher>,
    opts: &'a Options,
    buf: Vec<u8>,
    /// Set when notifications are unavailable for some file, so every wake-up
    /// must check every file.
    poll_every_wake: bool,
}

impl Follower<'_> {
    fn run(&mut self, mut files: Vec<Followed>, sink: &mut impl Sink) -> io::Result<bool> {
        for (index, followed) in files.iter().enumerate() {
            if let Some(open) = &followed.open {
                self.watch(index, &followed.name, &open.file, sink);
            }
        }

        // Anything appended between the initial tail and the watches being
        // registered would otherwise wait for the next event.
        self.check_all(&mut files, sink)?;

        loop {
            if files.iter().all(|f| f.gone) {
                sink.notice(0, "no files remaining");
                return Ok(false);
            }
            if let Some(pid) = self.opts.pid
                && !process_alive(pid)
            {
                // Print what the writer managed to append before it exited.
                self.check_all(&mut files, sink)?;
                return Ok(true);
            }

            let tokens = self.watcher.wait(self.timeout(&files))?;
            if tokens.is_empty() || self.poll_every_wake {
                self.check_all(&mut files, sink)?;
            } else {
                for index in tokens {
                    if index < files.len() {
                        self.check(index, &mut files[index], sink)?;
                    }
                }
            }
        }
    }

    /// When to wake up even without an event. Native notifications only
    /// cover files we have open, so a missing path, or a rotation that has
    /// not yet produced the new file, is checked every `-s` interval. That is
    /// one stat per file per interval, which keeps idle cost negligible.
    fn timeout(&self, files: &[Followed]) -> Option<Duration> {
        let periodic = self.poll_every_wake
            || self.opts.pid.is_some()
            || self.opts.by_name
            || files.iter().any(|f| f.open.is_none() && !f.gone);
        periodic.then_some(self.opts.sleep_interval)
    }

    fn watch(&mut self, index: usize, name: &str, file: &File, sink: &mut impl Sink) {
        if let Err(e) = self.watcher.add(index, file) {
            // Most likely the inotify watch limit; keep going by polling.
            sink.notice(
                index,
                &format!("{name}: cannot watch ({e}); polling instead"),
            );
            self.poll_every_wake = true;
        }
    }

    fn check_all(&mut self, files: &mut [Followed], sink: &mut impl Sink) -> io::Result<()> {
        for (index, followed) in files.iter_mut().enumerate() {
            self.check(index, followed, sink)?;
        }
        Ok(())
    }

    fn check(
        &mut self,
        index: usize,
        followed: &mut Followed,
        sink: &mut impl Sink,
    ) -> io::Result<()> {
        if followed.gone {
            return Ok(());
        }
        // A path is looked up again in name mode, and in descriptor mode only
        // until it has been opened once (`--retry` on a missing file).
        if followed.path.is_some() && (self.opts.by_name || followed.open.is_none()) {
            self.reconcile(index, followed, sink)?;
        }
        if let Some(open) = &mut followed.open {
            drain(index, &followed.name, open, sink, &mut self.buf)?;
        }
        Ok(())
    }

    /// Compares what the path names now with the file we have open, and
    /// switches over on rotation. Before letting go of the old file, its
    /// remaining data is printed: a logger often writes a few more lines to
    /// the renamed file before it reopens the new one.
    fn reconcile(
        &mut self,
        index: usize,
        followed: &mut Followed,
        sink: &mut impl Sink,
    ) -> io::Result<()> {
        let Some(path) = followed.path.clone() else {
            return Ok(());
        };
        let current = fs::metadata(&path);

        let replaced = match (&current, &followed.open) {
            (Ok(metadata), Some(open)) if FileId::of(metadata) == open.id => return Ok(()),
            (Err(_), None) => return Ok(()),
            (Ok(metadata), open) if !metadata.is_file() => {
                if open.is_some() {
                    self.close(index, followed, sink)?;
                    sink.notice(
                        index,
                        &format!(
                            "'{}' has been replaced with an untailable file",
                            followed.name
                        ),
                    );
                    self.give_up_unless_retrying(followed);
                }
                return Ok(());
            }
            (Err(e), Some(_)) => {
                let message = format!(
                    "'{}' has become inaccessible: {}",
                    followed.name,
                    describe(e)
                );
                self.close(index, followed, sink)?;
                sink.notice(index, &message);
                self.give_up_unless_retrying(followed);
                return Ok(());
            }
            (Ok(_), open) => open.is_some(),
        };

        if replaced {
            self.close(index, followed, sink)?;
        }
        match File::open(&path).and_then(|file| file.metadata().map(|m| (file, m))) {
            Ok((file, metadata)) => {
                let verb = if replaced {
                    "has been replaced"
                } else {
                    "has appeared"
                };
                sink.notice(
                    index,
                    &format!("'{}' {verb};  following new file", followed.name),
                );
                self.watch(index, &followed.name, &file, sink);
                // Use the id of what we opened, not of the earlier stat, in
                // case the path changed again in between.
                followed.open = Some(OpenFile {
                    file,
                    offset: 0,
                    id: FileId::of(&metadata),
                });
            }
            // It vanished between the stat and the open; the next check retries.
            Err(_) if replaced => self.give_up_unless_retrying(followed),
            Err(_) => {}
        }
        Ok(())
    }

    /// Prints what is left of the open file, then stops watching it.
    fn close(
        &mut self,
        index: usize,
        followed: &mut Followed,
        sink: &mut impl Sink,
    ) -> io::Result<()> {
        if let Some(mut open) = followed.open.take() {
            drain(index, &followed.name, &mut open, sink, &mut self.buf)?;
            self.watcher.remove(index);
        }
        Ok(())
    }

    fn give_up_unless_retrying(&self, followed: &mut Followed) {
        followed.gone = !self.opts.retry;
    }
}

/// Prints everything appended since the last call. A file that shrank below
/// the printed offset was truncated, so it is printed again from the start.
///
/// Read errors are reported and skipped, since the file may recover; only sink
/// errors are returned.
fn drain(
    index: usize,
    name: &str,
    open: &mut OpenFile,
    sink: &mut impl Sink,
    buf: &mut [u8],
) -> io::Result<()> {
    let len = match open.file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(e) => {
            sink.notice(index, &format!("{name}: cannot stat: {}", describe(&e)));
            return Ok(());
        }
    };
    if len < open.offset {
        sink.notice(index, &format!("{name}: file truncated"));
        open.offset = 0;
    }
    if len == open.offset {
        return Ok(());
    }

    if let Err(e) = open.file.seek(SeekFrom::Start(open.offset)) {
        sink.notice(index, &format!("{name}: cannot seek: {}", describe(&e)));
        return Ok(());
    }
    loop {
        let n = match open.file.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                sink.notice(index, &format!("error reading '{name}': {}", describe(&e)));
                break;
            }
        };
        sink.write(index, name, &buf[..n])?;
        open.offset += n as u64;
    }
    sink.flush()
}

fn process_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs only the existence and permission check.
    let ret = unsafe { libc::kill(pid, 0) };
    // EPERM means the process exists but belongs to someone else.
    ret == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[derive(Default)]
    struct Recorder {
        output: Vec<u8>,
        notices: Vec<String>,
    }

    impl Sink for Recorder {
        fn write(&mut self, _index: usize, _name: &str, data: &[u8]) -> io::Result<()> {
            self.output.extend_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn notice(&mut self, _index: usize, message: &str) {
            self.notices.push(message.to_owned());
        }
    }

    fn open_at_end(path: &std::path::Path) -> OpenFile {
        let mut file = File::open(path).unwrap();
        let offset = file.seek(SeekFrom::End(0)).unwrap();
        let id = FileId::of(&file.metadata().unwrap());
        OpenFile { file, offset, id }
    }

    #[test]
    fn drain_prints_only_appended_data() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"old\n").unwrap();
        let mut open = open_at_end(tmp.path());
        let mut sink = Recorder::default();
        let mut buf = vec![0; 3]; // smaller than the data, to cover the read loop

        drain(0, "log", &mut open, &mut sink, &mut buf).unwrap();
        assert!(sink.output.is_empty());

        let mut writer = OpenOptions::new().append(true).open(tmp.path()).unwrap();
        writer.write_all(b"new line\n").unwrap();
        drain(0, "log", &mut open, &mut sink, &mut buf).unwrap();
        assert_eq!(sink.output, b"new line\n");
    }

    #[test]
    fn drain_restarts_after_truncation() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"a long first generation\n").unwrap();
        let mut open = open_at_end(tmp.path());
        let mut sink = Recorder::default();

        std::fs::write(tmp.path(), b"short\n").unwrap();
        drain(0, "log", &mut open, &mut sink, &mut vec![0; CHUNK]).unwrap();
        assert_eq!(sink.output, b"short\n");
        assert_eq!(sink.notices, ["log: file truncated"]);
    }

    #[test]
    fn name_mode_switches_to_the_rotated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"old\n").unwrap();
        let mut file = File::open(&path).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        let mut followed = Followed::new("log".into(), Some(path.clone()), file).unwrap();

        let opts = Options {
            sleep_interval: Duration::from_millis(10),
            pid: None,
            use_polling: true,
            by_name: true,
            retry: true,
        };
        let mut follower = Follower {
            watcher: watch::new(true, opts.sleep_interval),
            opts: &opts,
            buf: vec![0; CHUNK],
            poll_every_wake: true,
        };
        let mut sink = Recorder::default();

        // A last line lands in the old file after it is renamed away.
        let mut old_writer = OpenOptions::new().append(true).open(&path).unwrap();
        std::fs::rename(&path, dir.path().join("log.1")).unwrap();
        old_writer.write_all(b"late\n").unwrap();
        follower.check(0, &mut followed, &mut sink).unwrap();
        assert_eq!(sink.output, b"late\n");
        assert_eq!(
            sink.notices,
            ["'log' has become inaccessible: No such file or directory"]
        );

        std::fs::write(&path, b"fresh\n").unwrap();
        follower.check(0, &mut followed, &mut sink).unwrap();
        assert_eq!(sink.output, b"late\nfresh\n");
        assert_eq!(sink.notices[1], "'log' has appeared;  following new file");
        assert!(!followed.gone);
    }

    #[test]
    fn writer_sink_prints_header_on_switch() {
        let mut out = Vec::new();
        let mut sink = WriterSink::new(&mut out, true, Some(1));
        sink.write(1, "b", b"b1\n").unwrap();
        sink.write(0, "a", b"a1\n").unwrap();
        sink.write(0, "a", b"a2\n").unwrap();
        sink.write(1, "b", b"b2\n").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "b1\n\n==> a <==\na1\na2\n\n==> b <==\nb2\n"
        );
    }

    #[test]
    fn current_process_is_alive() {
        assert!(process_alive(std::process::id() as libc::pid_t));
    }
}
