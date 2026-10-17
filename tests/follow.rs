//! End-to-end tests for follow mode: spawn rtail, change files underneath it
//! and wait for the expected output.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(10);

/// A running rtail whose stdout and stderr are collected in the background.
struct Running {
    child: Child,
    stdout: Output,
    stderr: Output,
}

struct Output {
    rx: Receiver<Vec<u8>>,
    seen: Vec<u8>,
}

impl Output {
    fn collect(mut pipe: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            rx,
            seen: Vec::new(),
        }
    }

    /// Waits until everything received so far equals `expected`.
    fn wait_for(&mut self, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while self.seen != expected.as_bytes() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
                Ok(chunk) => self.seen.extend(chunk),
                Err(_) => panic!(
                    "timed out waiting for {expected:?}; got {:?}",
                    String::from_utf8_lossy(&self.seen)
                ),
            }
        }
    }

    /// Asserts nothing else arrives within a short grace period.
    fn assert_quiet(&mut self) {
        if let Ok(chunk) = self.rx.recv_timeout(Duration::from_millis(300)) {
            panic!("unexpected output: {:?}", String::from_utf8_lossy(&chunk));
        }
    }
}

impl Running {
    fn start(args: &[&str], files: &[&Path]) -> Self {
        let mut child = Command::new(cargo_bin("rtail"))
            .args(args)
            .args(files)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = Output::collect(child.stdout.take().unwrap());
        let stderr = Output::collect(child.stderr.take().unwrap());
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "rtail did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn file_with(dir: &TempDir, name: &str, content: &str) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, content).unwrap();
    path
}

fn append(path: &Path, content: &str) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(content.as_bytes()).unwrap();
}

#[test]
fn prints_appended_lines() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "one\ntwo\n");
    let mut rtail = Running::start(&["-f", "-n", "1"], &[&log]);

    rtail.stdout.wait_for("two\n");
    append(&log, "three\n");
    rtail.stdout.wait_for("two\nthree\n");
    append(&log, "four");
    rtail.stdout.wait_for("two\nthree\nfour");
}

#[test]
fn polling_backend_prints_appended_lines() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "");
    let mut rtail = Running::start(&["-f", "--use-polling", "-s", "0.05"], &[&log]);

    append(&log, "polled\n");
    rtail.stdout.wait_for("polled\n");
}

#[test]
fn headers_switch_between_files() {
    let dir = TempDir::new().unwrap();
    let a = file_with(&dir, "a", "a1\n");
    let b = file_with(&dir, "b", "b1\n");
    let mut rtail = Running::start(&["-f"], &[&a, &b]);

    let (a_name, b_name) = (a.display(), b.display());
    let mut expected = format!("==> {a_name} <==\na1\n\n==> {b_name} <==\nb1\n");
    rtail.stdout.wait_for(&expected);

    // Still on b: no new header.
    append(&b, "b2\n");
    expected += "b2\n";
    rtail.stdout.wait_for(&expected);

    append(&a, "a2\n");
    expected += &format!("\n==> {a_name} <==\na2\n");
    rtail.stdout.wait_for(&expected);
}

#[test]
fn reports_truncation_and_prints_from_start() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "a fairly long first line\n");
    let mut rtail = Running::start(&["-f"], &[&log]);
    rtail.stdout.wait_for("a fairly long first line\n");

    File::create(&log).unwrap().write_all(b"short\n").unwrap();
    rtail.stdout.wait_for("a fairly long first line\nshort\n");
    rtail
        .stderr
        .wait_for(&format!("rtail: {}: file truncated\n", log.display()));
}

#[test]
fn descriptor_mode_keeps_reading_a_renamed_file() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "ready\n");
    let mut writer = OpenOptions::new().append(true).open(&log).unwrap();
    let mut rtail = Running::start(&["-f"], &[&log]);
    // Once the initial tail is printed, rtail holds the original file open.
    rtail.stdout.wait_for("ready\n");

    fs::rename(&log, dir.path().join("log.1")).unwrap();
    fs::write(&log, "new file, not followed in descriptor mode\n").unwrap();
    writer.write_all(b"written after rename\n").unwrap();

    rtail.stdout.wait_for("ready\nwritten after rename\n");
    rtail.stdout.assert_quiet();
}

#[test]
fn exits_when_pid_dies() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "start\n");
    let mut writer = Command::new("sleep").arg("0.5").spawn().unwrap();
    let pid = writer.id().to_string();
    let mut rtail = Running::start(&["-f", "-s", "0.1", "--pid", &pid], &[&log]);

    rtail.stdout.wait_for("start\n");
    // Reap the child, otherwise it lingers as a zombie that kill(pid, 0) still sees.
    writer.wait().unwrap();
    assert!(rtail.wait_exit().success());
}

#[test]
fn stdin_pipe_is_not_followed() {
    let mut child = Command::new(cargo_bin("rtail"))
        .arg("-f")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"piped\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"piped\n");
}

#[test]
fn no_files_remaining() {
    let dir = TempDir::new().unwrap();
    let output = Command::new(cargo_bin("rtail"))
        .arg("-f")
        .arg(dir.path().join("missing"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).ends_with("rtail: no files remaining\n"));
}

/// Waits until stderr has produced every line in `expected`, in any order,
/// since a rotation can be seen as "inaccessible then appeared" or as
/// "replaced" depending on timing.
fn wait_for_stderr_containing(rtail: &mut Running, expected: &[&str]) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = String::from_utf8_lossy(&rtail.stderr.seen).into_owned();
        if expected.iter().all(|e| text.contains(e)) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rtail.stderr.rx.recv_timeout(remaining) {
            Ok(chunk) => rtail.stderr.seen.extend(chunk),
            Err(_) => panic!("timed out waiting for {expected:?} on stderr; got {text:?}"),
        }
    }
}

#[test]
fn name_mode_follows_rename_and_create_rotation() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "gen1\n");
    let mut old_writer = OpenOptions::new().append(true).open(&log).unwrap();
    let mut rtail = Running::start(&["-F", "-s", "0.1"], &[&log]);
    rtail.stdout.wait_for("gen1\n");

    // logrotate's default `create` mode: the logger's last line lands in the
    // file that is about to be renamed, then a new file takes the name.
    old_writer
        .write_all(b"last line before rotation\n")
        .unwrap();
    fs::rename(&log, dir.path().join("log.1")).unwrap();
    fs::write(&log, "gen2\n").unwrap();

    rtail
        .stdout
        .wait_for("gen1\nlast line before rotation\ngen2\n");
    wait_for_stderr_containing(&mut rtail, &["following new file"]);

    append(&log, "gen2 continues\n");
    rtail
        .stdout
        .wait_for("gen1\nlast line before rotation\ngen2\ngen2 continues\n");
}

#[test]
fn name_mode_reads_old_file_after_rename_is_noticed() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "gen1\n");
    let mut old_writer = OpenOptions::new().append(true).open(&log).unwrap();
    let mut rtail = Running::start(&["-F", "-s", "0.1"], &[&log]);
    rtail.stdout.wait_for("gen1\n");

    // The rename is reported at once by kqueue/inotify, before the logger has
    // written its last line to the renamed file and reopened the path.
    fs::rename(&log, dir.path().join("log.1")).unwrap();
    wait_for_stderr_containing(&mut rtail, &["has become inaccessible"]);
    old_writer.write_all(b"late\n").unwrap();
    rtail.stdout.wait_for("gen1\nlate\n");

    fs::write(&log, "gen2\n").unwrap();
    rtail.stdout.wait_for("gen1\nlate\ngen2\n");
    wait_for_stderr_containing(&mut rtail, &["has appeared;  following new file"]);
}

#[test]
fn name_mode_follows_atomic_replace() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "old\n");
    let mut rtail = Running::start(&["-F", "-s", "0.1"], &[&log]);
    rtail.stdout.wait_for("old\n");

    let staged = file_with(&dir, "log.tmp", "replacement\n");
    fs::rename(&staged, &log).unwrap();

    rtail.stdout.wait_for("old\nreplacement\n");
    wait_for_stderr_containing(
        &mut rtail,
        &[&format!(
            "'{}' has been replaced;  following new file",
            log.display()
        )],
    );
}

#[test]
fn name_mode_handles_copytruncate() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "before copytruncate\n");
    let mut rtail = Running::start(&["-F", "-s", "0.1"], &[&log]);
    rtail.stdout.wait_for("before copytruncate\n");

    fs::copy(&log, dir.path().join("log.1")).unwrap();
    File::create(&log).unwrap();
    rtail
        .stderr
        .wait_for(&format!("rtail: {}: file truncated\n", log.display()));
    append(&log, "after\n");
    rtail.stdout.wait_for("before copytruncate\nafter\n");
}

#[test]
fn name_mode_survives_delete_and_recreate() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "first\n");
    let mut rtail = Running::start(&["-F", "-s", "0.1"], &[&log]);
    rtail.stdout.wait_for("first\n");

    fs::remove_file(&log).unwrap();
    wait_for_stderr_containing(
        &mut rtail,
        &[&format!(
            "'{}' has become inaccessible: No such file or directory",
            log.display()
        )],
    );
    fs::write(&log, "second\n").unwrap();

    rtail.stdout.wait_for("first\nsecond\n");
    wait_for_stderr_containing(
        &mut rtail,
        &[&format!(
            "'{}' has appeared;  following new file",
            log.display()
        )],
    );
}

#[test]
fn retry_waits_for_a_file_that_does_not_exist_yet() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("not-yet");
    let mut rtail = Running::start(&["-F", "-s", "0.1"], &[&log]);
    wait_for_stderr_containing(&mut rtail, &["cannot open"]);

    fs::write(&log, "hello\n").unwrap();
    rtail.stdout.wait_for("hello\n");
    wait_for_stderr_containing(&mut rtail, &["has appeared;  following new file"]);
}

#[test]
fn name_mode_without_retry_gives_up() {
    let dir = TempDir::new().unwrap();
    let log = file_with(&dir, "log", "x\n");
    let mut rtail = Running::start(&["--follow=name", "-s", "0.1"], &[&log]);
    rtail.stdout.wait_for("x\n");

    fs::remove_file(&log).unwrap();
    assert_eq!(rtail.wait_exit().code(), Some(1));
    wait_for_stderr_containing(
        &mut rtail,
        &["has become inaccessible", "rtail: no files remaining"],
    );
}
