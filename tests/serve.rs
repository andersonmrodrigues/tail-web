//! `--serve` end to end: spawns rtail on an ephemeral port and talks plain
//! HTTP/1.1 to it, so the tests need no HTTP client dependency.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(cargo_bin("rtail"))
            .arg("--serve=127.0.0.1:0")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rtail should start");

        // The first stderr line announces the bound address.
        let mut stderr = BufReader::new(child.stderr.take().expect("stderr is piped"));
        let mut line = String::new();
        stderr
            .read_line(&mut line)
            .expect("stderr should be readable");
        let port = line
            .trim_end()
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in {line:?}"));
        // Keep draining stderr so notices never block the server on a full pipe.
        std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
        Self { child, port }
    }

    fn connect(&self, path: &str, host: &str) -> TcpStream {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).expect("server should accept");
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream
    }

    /// The whole response, for endpoints that are not streams.
    fn get(&self, path: &str) -> String {
        self.get_with_host(path, &format!("127.0.0.1:{}", self.port))
    }

    fn get_with_host(&self, path: &str, host: &str) -> String {
        let mut response = String::new();
        self.connect(path, host)
            .read_to_string(&mut response)
            .expect("response should be readable");
        response
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads an event stream until `needle` shows up.
fn read_until(stream: &mut TcpStream, seen: &mut String, needle: &str) {
    let deadline = Instant::now() + TIMEOUT;
    let mut buf = [0; 4096];
    while !seen.contains(needle) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?} in {seen:?}"
        );
        let n = stream.read(&mut buf).expect("stream should stay readable");
        assert!(n > 0, "stream ended before {needle:?} in {seen:?}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
}

fn append(path: &Path, data: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(data.as_bytes())
        .unwrap();
}

#[test]
fn streams_backlog_then_live_lines() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("app.log");
    fs::write(&log, "old\nkept\n\n<b>html</b>\n").unwrap();
    let server = Server::start(&["-n", "3", log.to_str().unwrap()]);

    let mut stream = server.connect("/events/0", &format!("localhost:{}", server.port));
    let mut seen = String::new();
    read_until(&mut stream, &mut seen, "data: \"<b>html</b>\"");
    assert!(seen.contains("text/event-stream"), "{seen}");
    assert!(seen.contains("event: line\ndata: \"kept\""), "{seen}");
    // An empty line is still an event, sent as an empty JSON string.
    assert!(seen.contains("event: line\ndata: \"\""), "{seen}");
    assert!(
        !seen.contains("\"old\""),
        "-n 3 should drop the first line: {seen}"
    );

    append(&log, "fresh\r\n");
    read_until(&mut stream, &mut seen, "data: \"fresh\"");

    fs::write(&log, "").unwrap();
    read_until(&mut stream, &mut seen, "event: notice");
    assert!(seen.contains("file truncated"), "{seen}");
}

#[test]
fn serves_page_and_file_list() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.log");
    let b = dir.path().join("b.log");
    fs::write(&a, "").unwrap();
    fs::write(&b, "").unwrap();
    let server = Server::start(&[a.to_str().unwrap(), b.to_str().unwrap()]);

    let page = server.get("/");
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("<title>rtail</title>"), "{page}");

    let files = server.get("/api/files");
    assert!(files.contains("application/json"), "{files}");
    let expected = format!(
        r#"[{{"index":0,"name":"{}"}},{{"index":1,"name":"{}"}}]"#,
        a.display(),
        b.display()
    );
    assert!(files.ends_with(&expected), "{files}");

    assert!(server.get("/events/2").starts_with("HTTP/1.1 404"));
}

#[test]
fn rejects_foreign_host_header() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("app.log");
    fs::write(&log, "secret\n").unwrap();
    let server = Server::start(&[log.to_str().unwrap()]);

    let response = server.get_with_host("/events/0", "attacker.example");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(!response.contains("secret"), "{response}");
    assert!(
        server
            .get_with_host("/", &format!("[::1]:{}", server.port))
            .starts_with("HTTP/1.1 200")
    );
}

#[test]
fn shuts_down_when_pid_exits() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("app.log");
    fs::write(&log, "line\n").unwrap();
    let mut writer = Command::new("sleep").arg("1").spawn().unwrap();
    let pid = writer.id().to_string();
    let mut server = Server::start(&["--pid", &pid, log.to_str().unwrap()]);

    // An open stream must not keep the server alive once following stops.
    let mut stream = server.connect("/events/0", &format!("127.0.0.1:{}", server.port));
    let mut seen = String::new();
    read_until(&mut stream, &mut seen, "data: \"line\"");
    writer.wait().unwrap();

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "server did not exit after --pid");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    let mut rest = String::new();
    stream
        .read_to_string(&mut rest)
        .expect("stream should end cleanly");
}

#[test]
fn refuses_to_serve_stdin() {
    let output = Command::new(cargo_bin("rtail"))
        .args(["--serve=127.0.0.1:0", "-"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("standard input"));
}

#[test]
fn refuses_to_serve_fifo() {
    let dir = TempDir::new().unwrap();
    let fifo = dir.path().join("pipe");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    // Opening a FIFO blocks until the other end opens too, so rtail needs a
    // writer before it can look at the operand.
    let writer = {
        let fifo = fifo.clone();
        std::thread::spawn(move || OpenOptions::new().write(true).open(fifo))
    };
    let output = Command::new(cargo_bin("rtail"))
        .args(["--serve=127.0.0.1:0", fifo.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    writer.join().unwrap().expect("writer should open the FIFO");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "--serve cannot follow '{}': not a regular file",
            fifo.display()
        )),
        "{stderr}"
    );
    assert!(stderr.contains("no files remaining"), "{stderr}");
}
