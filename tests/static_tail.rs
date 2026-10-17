use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use assert_cmd::cargo::cargo_bin;
use tempfile::TempDir;

fn rtail() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("rtail").unwrap()
}

fn write(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, content).unwrap();
    path
}

/// Lines of varying length, including empty ones and some longer than the
/// 64 KiB read block, so backward reads cross block boundaries.
fn corpus() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..5000u32 {
        let len = match i % 97 {
            0 => 0,
            1 => 70_000,
            n => n as usize,
        };
        data.extend(std::iter::repeat_n(b'a' + (i % 26) as u8, len));
        data.push(b'\n');
    }
    data
}

fn system_tail(args: &[&str], path: &Path) -> Vec<u8> {
    let output = Command::new("tail").args(args).arg(path).output().unwrap();
    assert!(output.status.success(), "system tail failed for {args:?}");
    output.stdout
}

#[test]
fn matches_system_tail() {
    let dir = TempDir::new().unwrap();
    let mut unterminated = corpus();
    unterminated.extend_from_slice(b"no newline at end");
    let files = [
        write(&dir, "corpus", &corpus()),
        write(&dir, "unterminated", &unterminated),
        write(&dir, "empty", b""),
        write(&dir, "single", b"only line\n"),
    ];
    let cases: &[&[&str]] = &[
        &[],
        &["-n", "1"],
        &["-n", "0"],
        &["-n", "250"],
        &["-n", "100000"],
        &["-n", "+1"],
        &["-n", "+4990"],
        &["-c", "1"],
        &["-c", "100000"],
        &["-c", "+12345"],
        &["-7"],
    ];

    for path in &files {
        for args in cases {
            let expected = system_tail(args, path);
            let actual = rtail().args(*args).arg(path).output().unwrap();
            assert!(actual.status.success());
            assert!(
                actual.stdout == expected,
                "output differs from system tail for {args:?} on {}",
                path.display()
            );
        }
    }
}

#[test]
fn headers_for_multiple_files() {
    let dir = TempDir::new().unwrap();
    let a = write(&dir, "a", b"a1\na2\n");
    let b = write(&dir, "b", b"b1\n");
    let expected = format!(
        "==> {} <==\na2\n\n==> {} <==\nb1\n",
        a.display(),
        b.display()
    );

    rtail()
        .args(["-n", "1"])
        .arg(&a)
        .arg(&b)
        .assert()
        .success()
        .stdout(expected);
    rtail()
        .args(["-q", "-n", "1"])
        .arg(&a)
        .arg(&b)
        .assert()
        .success()
        .stdout("a2\nb1\n");
    rtail()
        .args(["-v", "-n", "1"])
        .arg(&b)
        .assert()
        .success()
        .stdout(format!("==> {} <==\nb1\n", b.display()));
}

#[test]
fn missing_file_is_reported_and_others_still_print() {
    let dir = TempDir::new().unwrap();
    let present = write(&dir, "present", b"here\n");
    let missing = dir.path().join("missing");

    let output = rtail().arg(&missing).arg(&present).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("==> {} <==\nhere\n", present.display())
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rtail: cannot open '{}' for reading: No such file or directory\n",
            missing.display()
        )
    );
}

#[test]
fn directory_is_an_error() {
    let dir = TempDir::new().unwrap();
    rtail().arg(dir.path()).assert().code(1).stderr(format!(
        "rtail: error reading '{}': Is a directory\n",
        dir.path().display()
    ));
}

#[test]
fn directory_among_files_still_gets_a_header() {
    let dir = TempDir::new().unwrap();
    let file = write(&dir, "file", b"x\n");
    let output = rtail().arg(dir.path()).arg(&file).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "==> {} <==\n\n==> {} <==\nx\n",
            dir.path().display(),
            file.display()
        )
    );
}

#[test]
fn reads_piped_stdin() {
    rtail()
        .args(["-n", "2"])
        .write_stdin("a\nb\nc\n")
        .assert()
        .success()
        .stdout("b\nc\n");
    rtail()
        .args(["-c", "+3", "-"])
        .write_stdin("abcdef")
        .assert()
        .success()
        .stdout("cdef");
}

#[test]
fn seeks_stdin_redirected_from_a_file() {
    let dir = TempDir::new().unwrap();
    let path = write(&dir, "corpus", &corpus());
    let expected = system_tail(&["-n", "3"], &path);
    rtail()
        .args(["-n", "3"])
        .pipe_stdin(&path)
        .unwrap()
        .assert()
        .success()
        .stdout(expected);
}

#[test]
fn stdin_header_is_named_standard_input() {
    rtail()
        .args(["-v", "-n", "1"])
        .write_stdin("x\n")
        .assert()
        .success()
        .stdout("==> standard input <==\nx\n");
}

#[test]
fn closed_stdout_exits_quietly() {
    let dir = TempDir::new().unwrap();
    let path = write(&dir, "big", &corpus().repeat(4));

    let mut child = Command::new(cargo_bin("rtail"))
        .args(["-n", "+1"])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdout.read_exact(&mut [0; 16]).unwrap();
    drop(stdout);

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_count_is_a_usage_error() {
    rtail().args(["-n", "ten"]).assert().code(1);
}
