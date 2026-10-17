use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use crate::reader::{Count, Mode};

const DEFAULT_LINES: u64 = 10;

#[derive(Parser, Debug)]
#[command(
    name = "rtail",
    version,
    // Like GNU tail, a repeated or competing option silently wins over the earlier one.
    args_override_self = true,
    about = "Print the last part of files",
    long_about = "Print the last 10 lines of each FILE to standard output.\n\
                  With more than one FILE, precede each with a header giving the file name.\n\
                  With no FILE, or when FILE is -, read standard input."
)]
pub struct Args {
    /// Output the last NUM lines, or use +NUM to output starting with line NUM
    #[arg(
        short = 'n',
        long = "lines",
        value_name = "NUM",
        allow_hyphen_values = true,
        value_parser = parse_count,
        overrides_with = "bytes"
    )]
    lines: Option<Count>,

    /// Output the last NUM bytes, or use +NUM to output starting with byte NUM
    #[arg(
        short = 'c',
        long = "bytes",
        value_name = "NUM",
        allow_hyphen_values = true,
        value_parser = parse_count,
        overrides_with = "lines"
    )]
    bytes: Option<Count>,

    /// Never print headers giving file names
    #[arg(short, long, visible_alias = "silent", overrides_with = "verbose")]
    pub quiet: bool,

    /// Always print headers giving file names
    #[arg(short, long, overrides_with = "quiet")]
    pub verbose: bool,

    /// Output appended data as the file grows
    #[arg(
        short = 'f',
        long = "follow",
        value_name = "HOW",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "descriptor",
        value_enum
    )]
    pub follow: Option<FollowMode>,

    /// With -f, wake at least every N seconds to check --pid, or to poll when
    /// native notifications are unavailable
    #[arg(
        short = 's',
        long = "sleep-interval",
        value_name = "N",
        default_value = "1",
        value_parser = parse_interval
    )]
    pub sleep_interval: Duration,

    /// With -f, stop after process PID exits
    #[arg(long, value_name = "PID")]
    pub pid: Option<libc::pid_t>,

    /// With -f, poll at the -s interval instead of using kqueue or inotify
    #[arg(long)]
    pub use_polling: bool,

    /// Files to read; - means standard input
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,
}

impl Args {
    /// Parses the process arguments. Usage errors exit with status 1, as in
    /// GNU and BSD tail, instead of clap's default of 2.
    pub fn parse() -> Self {
        match Self::try_parse_from(expand_obsolete_count(std::env::args_os())) {
            Ok(args) => args,
            Err(e) => {
                let code = if e.use_stderr() { 1 } else { 0 };
                // Printing can only fail if the terminal is gone; exit regardless.
                let _ = e.print();
                std::process::exit(code);
            }
        }
    }

    pub fn mode(&self) -> Mode {
        match (self.lines, self.bytes) {
            (_, Some(count)) => Mode::Bytes(count),
            (Some(count), None) => Mode::Lines(count),
            (None, None) => Mode::Lines(Count::Last(DEFAULT_LINES)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FollowMode {
    /// Keep reading the open file, even after it is renamed
    Descriptor,
}

fn parse_interval(value: &str) -> Result<Duration, String> {
    value
        .parse::<f64>()
        .ok()
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .ok_or_else(|| format!("invalid number of seconds: '{value}'"))
}

/// Accepts `NUM`, `-NUM` (both mean "last NUM") and `+NUM` ("starting at NUM").
fn parse_count(value: &str) -> Result<Count, String> {
    let (digits, from_start) = match value.strip_prefix('+') {
        Some(rest) => (rest, true),
        None => (value.strip_prefix('-').unwrap_or(value), false),
    };
    // `u64::from_str` accepts a leading '+', which would let "-+5" through.
    let n: u64 = Some(digits)
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| format!("invalid number: '{value}'"))?;
    Ok(if from_start {
        Count::From(n)
    } else {
        Count::Last(n)
    })
}

/// Rewrites the historical `tail -5 file` form into `tail -n 5 file`. Like GNU
/// tail, this is only recognised as the first argument.
fn expand_obsolete_count(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.into_iter().collect();
    let obsolete = args.get(1).and_then(|arg| arg.to_str()).and_then(|arg| {
        let digits = arg.strip_prefix('-')?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.to_owned())
    });
    if let Some(digits) = obsolete {
        args.splice(1..2, [OsString::from("-n"), OsString::from(digits)]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        let argv = std::iter::once("rtail")
            .chain(args.iter().copied())
            .map(OsString::from);
        Args::try_parse_from(expand_obsolete_count(argv)).expect("arguments should parse")
    }

    #[test]
    fn defaults_to_last_ten_lines() {
        assert_eq!(parse(&[]).mode(), Mode::Lines(Count::Last(10)));
    }

    #[test]
    fn parses_count_forms() {
        assert_eq!(parse(&["-n", "5"]).mode(), Mode::Lines(Count::Last(5)));
        assert_eq!(parse(&["-n", "-5"]).mode(), Mode::Lines(Count::Last(5)));
        assert_eq!(parse(&["-n", "+5"]).mode(), Mode::Lines(Count::From(5)));
        assert_eq!(parse(&["-c", "+0"]).mode(), Mode::Bytes(Count::From(0)));
        assert_eq!(parse(&["--bytes=7"]).mode(), Mode::Bytes(Count::Last(7)));
    }

    #[test]
    fn rejects_invalid_counts() {
        for bad in ["", "+", "-", "abc", "5x", "+-5", "-+5"] {
            assert!(parse_count(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn expands_obsolete_form_only_in_first_position() {
        let args = parse(&["-3", "file"]);
        assert_eq!(args.mode(), Mode::Lines(Count::Last(3)));
        assert_eq!(args.files, [PathBuf::from("file")]);

        let argv = ["rtail", "file", "-3"].map(OsString::from);
        assert_eq!(expand_obsolete_count(argv.clone()), argv);
    }

    #[test]
    fn parses_follow_options() {
        let args = parse(&["-f", "-s", "0.25", "--pid", "42", "log"]);
        assert_eq!(args.follow, Some(FollowMode::Descriptor));
        assert_eq!(args.sleep_interval, Duration::from_millis(250));
        assert_eq!(args.pid, Some(42));
        // `-f` takes its value only with `=`, so `log` stays a file operand.
        assert_eq!(args.files, [PathBuf::from("log")]);

        assert_eq!(
            parse(&["--follow=descriptor"]).follow,
            Some(FollowMode::Descriptor)
        );
        assert_eq!(parse(&[]).follow, None);
        assert_eq!(parse(&[]).sleep_interval, Duration::from_secs(1));
    }

    #[test]
    fn rejects_invalid_intervals() {
        for bad in ["-1", "abc", "inf", "NaN"] {
            assert!(parse_interval(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn last_option_wins() {
        assert_eq!(
            parse(&["-n", "1", "-c", "2"]).mode(),
            Mode::Bytes(Count::Last(2))
        );
        assert_eq!(
            parse(&["-c", "2", "-n", "1"]).mode(),
            Mode::Lines(Count::Last(1))
        );
        assert_eq!(
            parse(&["-n", "5", "-n", "1"]).mode(),
            Mode::Lines(Count::Last(1))
        );

        let args = parse(&["-q", "-v"]);
        assert!(args.verbose && !args.quiet);
        let args = parse(&["-v", "-q"]);
        assert!(args.quiet && !args.verbose);
    }
}
