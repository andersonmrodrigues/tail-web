mod cli;
mod follow;
mod reader;
mod watch;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cli::{Args, FollowMode};
use follow::{Followed, WriterSink};
use reader::Mode;

fn main() -> ExitCode {
    let args = Args::parse();
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let result = run(&args, &mut out).and_then(|ok| out.flush().map(|()| ok));
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        // `rtail big.log | head` closes the pipe early; that is not an error.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rtail: write error: {}", describe(&e));
            ExitCode::FAILURE
        }
    }
}

/// Tails every file in order, then follows them if asked to. Problems with
/// one file are reported and the rest are still processed; the return value
/// says whether all succeeded. Only errors that make further output pointless
/// (a closed stdout) abort.
fn run(args: &Args, out: &mut impl Write) -> io::Result<bool> {
    let stdin_only = [Path::new("-").to_path_buf()];
    let files = if args.files.is_empty() {
        &stdin_only[..]
    } else {
        &args.files[..]
    };
    let headers = args.verbose || (!args.quiet && files.len() > 1);
    let mode = args.mode();
    let follow_mode = args.follow_mode();

    let mut all_ok = true;
    let mut first_header = true;
    let mut followed = Vec::new();
    // Index in `followed` of the file whose output was printed last, so
    // following it does not repeat the header that is already on screen.
    let mut last_printed = None;
    for path in files {
        let name = display_name(path);
        let input = Input::open(path);
        // A file that opened gets its header even if reading it then fails,
        // which is how both GNU and BSD tail report a directory.
        if !matches!(input, Err(Failure::Open(_))) {
            last_printed = None;
            if headers {
                if !first_header {
                    writeln!(out)?;
                }
                first_header = false;
                writeln!(out, "==> {name} <==")?;
            }
        }
        let result = input.and_then(|input| input.tail(mode, out));

        match result {
            // Pipes cannot be followed: their data is consumed by the initial
            // tail. GNU tail ignores -f for them too.
            Ok(None) => {}
            Ok(Some(file)) if follow_mode.is_some() => {
                match Followed::new(name, operand_path(path), file) {
                    Ok(f) => {
                        last_printed = Some(followed.len());
                        followed.push(f);
                    }
                    Err(e) => {
                        all_ok = false;
                        eprintln!(
                            "rtail: cannot follow '{}': {}",
                            path.display(),
                            describe(&e)
                        );
                    }
                }
            }
            Ok(Some(_)) => {}
            Err(Failure::Write(e)) => return Err(e),
            Err(failure) => {
                all_ok = false;
                out.flush()?;
                eprintln!("rtail: {}", failure.message(&name));
                // With --retry, a file that does not exist yet is watched for.
                if let (Some(_), true, Failure::Open(_), Some(path)) =
                    (follow_mode, args.retry(), &failure, operand_path(path))
                {
                    followed.push(Followed::missing(name, path));
                }
            }
        }
    }

    if let Some(follow_mode) = follow_mode {
        if !followed.is_empty() {
            out.flush()?;
            let opts = follow::Options {
                sleep_interval: args.sleep_interval,
                pid: args.pid,
                use_polling: args.use_polling,
                by_name: follow_mode == FollowMode::Name,
                retry: args.retry(),
            };
            let mut sink = WriterSink::new(out, headers, last_printed);
            all_ok &= follow::run(followed, &opts, &mut sink)?;
        } else if !all_ok {
            eprintln!("rtail: no files remaining");
        }
    }
    Ok(all_ok)
}

/// The path to reopen in name mode; standard input has none.
fn operand_path(path: &Path) -> Option<PathBuf> {
    (path != Path::new("-")).then(|| path.to_path_buf())
}

fn display_name(path: &Path) -> String {
    if path == Path::new("-") {
        "standard input".to_owned()
    } else {
        path.display().to_string()
    }
}

enum Input {
    /// A regular file: tailed by seeking backward from the end.
    Seekable(File),
    /// A FIFO, character device or similar: tailed by reading it through.
    Stream(File),
    Stdin(io::Stdin),
}

enum Failure {
    Open(io::Error),
    IsDirectory,
    Read(io::Error),
    Write(io::Error),
}

impl Failure {
    fn message(&self, name: &str) -> String {
        match self {
            Failure::Open(e) => format!("cannot open '{name}' for reading: {}", describe(e)),
            Failure::IsDirectory => format!("error reading '{name}': Is a directory"),
            Failure::Read(e) => format!("error reading '{name}': {}", describe(e)),
            Failure::Write(e) => format!("write error: {}", describe(e)),
        }
    }
}

impl Input {
    fn open(path: &Path) -> Result<Self, Failure> {
        if path == Path::new("-") {
            return Ok(Self::stdin());
        }
        let file = File::open(path).map_err(Failure::Open)?;
        let metadata = file.metadata().map_err(Failure::Open)?;
        if metadata.is_dir() {
            Err(Failure::IsDirectory)
        } else if metadata.is_file() {
            Ok(Input::Seekable(file))
        } else {
            Ok(Input::Stream(file))
        }
    }

    /// `rtail < big.log` hands us a regular file on fd 0. Seeking it directly
    /// keeps the fast path instead of reading the whole file through.
    fn stdin() -> Self {
        let stdin = io::stdin();
        let file = stdin
            .as_fd()
            .try_clone_to_owned()
            .map(File::from)
            .ok()
            .filter(|file| file.metadata().is_ok_and(|m| m.is_file()));
        match file {
            Some(file) => Input::Seekable(file),
            None => Input::Stdin(stdin),
        }
    }

    /// Prints the tail. Returns the file, positioned after the last byte
    /// printed, when it is a regular file that can be followed.
    fn tail(self, mode: Mode, out: &mut impl Write) -> Result<Option<File>, Failure> {
        let mut out = WriteTracker {
            inner: out,
            error: None,
        };
        let result = match self {
            Input::Seekable(mut file) => {
                reader::tail_seekable(&mut file, mode, &mut out).map(|()| Some(file))
            }
            Input::Stream(file) => reader::tail_stream(file, mode, &mut out).map(|()| None),
            Input::Stdin(stdin) => reader::tail_stream(stdin.lock(), mode, &mut out).map(|()| None),
        };
        match (result, out.error) {
            (_, Some(e)) => Err(Failure::Write(e)),
            (Err(e), None) => Err(Failure::Read(e)),
            (Ok(file), None) => Ok(file),
        }
    }
}

/// `io::copy` reports read and write failures the same way. Recording write
/// failures here lets the caller tell "this file is unreadable, move on" apart
/// from "stdout is gone, stop".
struct WriteTracker<'a, W: Write> {
    inner: &'a mut W,
    error: Option<io::Error>,
}

impl<W: Write> Write for WriteTracker<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf).inspect_err(|e| self.record(e))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().inspect_err(|e| self.record(e))
    }
}

impl<W: Write> WriteTracker<'_, W> {
    fn record(&mut self, e: &io::Error) {
        if self.error.is_none() && e.kind() != io::ErrorKind::Interrupted {
            self.error = Some(io::Error::new(e.kind(), e.to_string()));
        }
    }
}

/// `io::Error` renders as "No such file or directory (os error 2)"; tail
/// users expect just the strerror text.
pub(crate) fn describe(e: &io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(idx) => text[..idx].to_owned(),
        None => text,
    }
}
