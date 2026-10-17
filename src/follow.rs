//! Follows files after the initial tail has been printed, sleeping on OS
//! notifications until they grow.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::Duration;

use crate::watch;

const CHUNK: usize = 64 * 1024;

pub struct Options {
    pub sleep_interval: Duration,
    pub pid: Option<libc::pid_t>,
    pub use_polling: bool,
}

/// A file being followed, positioned right after the last byte printed.
pub struct Followed {
    pub name: String,
    file: File,
    offset: u64,
}

impl Followed {
    /// `file` must be positioned where the initial tail stopped printing.
    pub fn new(name: String, mut file: File) -> io::Result<Self> {
        let offset = file.stream_position()?;
        Ok(Self { name, file, offset })
    }
}

/// Where followed output goes: stdout, or the web server's subscribers.
pub trait Sink {
    fn write(&mut self, index: usize, name: &str, data: &[u8]) -> io::Result<()>;

    fn flush(&mut self) -> io::Result<()>;

    /// A non-fatal event about one file, such as truncation.
    fn notice(&mut self, _index: usize, name: &str, message: &str) {
        eprintln!("rtail: {name}: {message}");
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

/// Follows `files` until the process given by `--pid` exits, or forever.
/// Returns only on a sink error (such as a closed stdout) or that exit.
pub fn run(mut files: Vec<Followed>, opts: &Options, sink: &mut impl Sink) -> io::Result<()> {
    let mut watcher = watch::new(opts.use_polling, opts.sleep_interval);
    let mut poll_every_wake = !watcher.is_native();
    for (index, followed) in files.iter().enumerate() {
        if let Err(e) = watcher.add(index, &followed.file) {
            // Most likely the inotify watch limit; keep going by polling.
            sink.notice(
                index,
                &followed.name,
                &format!("cannot watch ({e}); polling instead"),
            );
            poll_every_wake = true;
        }
    }

    // Anything appended between the initial tail and the watches being
    // registered would otherwise wait for the next event.
    let mut buf = vec![0; CHUNK];
    drain_all(&mut files, sink, &mut buf)?;

    loop {
        if let Some(pid) = opts.pid
            && !process_alive(pid)
        {
            // Print what the writer managed to append before it exited.
            return drain_all(&mut files, sink, &mut buf);
        }

        let timeout = (poll_every_wake || opts.pid.is_some()).then_some(opts.sleep_interval);
        let tokens = watcher.wait(timeout)?;
        if tokens.is_empty() || poll_every_wake {
            drain_all(&mut files, sink, &mut buf)?;
        } else {
            for index in tokens {
                if let Some(followed) = files.get_mut(index) {
                    drain(index, followed, sink, &mut buf)?;
                }
            }
        }
    }
}

fn drain_all(files: &mut [Followed], sink: &mut impl Sink, buf: &mut [u8]) -> io::Result<()> {
    for (index, followed) in files.iter_mut().enumerate() {
        drain(index, followed, sink, buf)?;
    }
    Ok(())
}

/// Prints everything appended since the last call. A file that shrank below
/// the printed offset was truncated, so it is printed again from the start.
///
/// Read errors are reported and skipped, since the file may recover; only sink
/// errors are returned.
fn drain(
    index: usize,
    followed: &mut Followed,
    sink: &mut impl Sink,
    buf: &mut [u8],
) -> io::Result<()> {
    let len = match followed.file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(e) => {
            sink.notice(index, &followed.name, &format!("cannot stat: {e}"));
            return Ok(());
        }
    };
    if len < followed.offset {
        sink.notice(index, &followed.name, "file truncated");
        followed.offset = 0;
    }
    if len == followed.offset {
        return Ok(());
    }

    if let Err(e) = followed.file.seek(SeekFrom::Start(followed.offset)) {
        sink.notice(index, &followed.name, &format!("cannot seek: {e}"));
        return Ok(());
    }
    loop {
        let n = match followed.file.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                sink.notice(index, &followed.name, &format!("error reading: {e}"));
                break;
            }
        };
        sink.write(index, &followed.name, &buf[..n])?;
        followed.offset += n as u64;
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
        fn notice(&mut self, _index: usize, _name: &str, message: &str) {
            self.notices.push(message.to_owned());
        }
    }

    fn followed(path: &std::path::Path) -> Followed {
        let mut file = File::open(path).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        Followed::new("log".into(), file).unwrap()
    }

    #[test]
    fn drain_prints_only_appended_data() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"old\n").unwrap();
        let mut f = followed(tmp.path());
        let mut sink = Recorder::default();
        let mut buf = vec![0; 3]; // smaller than the data, to cover the read loop

        drain(0, &mut f, &mut sink, &mut buf).unwrap();
        assert!(sink.output.is_empty());

        let mut writer = OpenOptions::new().append(true).open(tmp.path()).unwrap();
        writer.write_all(b"new line\n").unwrap();
        drain(0, &mut f, &mut sink, &mut buf).unwrap();
        assert_eq!(sink.output, b"new line\n");
    }

    #[test]
    fn drain_restarts_after_truncation() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"a long first generation\n").unwrap();
        let mut f = followed(tmp.path());
        let mut sink = Recorder::default();

        std::fs::write(tmp.path(), b"short\n").unwrap();
        drain(0, &mut f, &mut sink, &mut vec![0; CHUNK]).unwrap();
        assert_eq!(sink.output, b"short\n");
        assert_eq!(sink.notices, ["file truncated"]);
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
