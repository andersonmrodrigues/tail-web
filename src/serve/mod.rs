//! `--serve`: follows the files as `-f` does, but streams each line to
//! browsers over Server-Sent Events instead of printing it.
//!
//! The follow loop keeps running on its own thread, exactly as in terminal
//! mode, and publishes lines into one [`Hub`] per file. A hub keeps the most
//! recent lines so a browser that connects late still sees context, and fans
//! new ones out through a broadcast channel.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::{Stream, StreamExt};

use crate::describe;
use crate::follow::{self, Followed, Sink};

const INDEX: &str = include_str!("index.html");
/// Lines replayed to a browser when it connects.
const BACKLOG: usize = 5000;
/// Live lines a slow browser may fall behind by before it is told it lagged.
const CHANNEL: usize = 4096;
/// A line longer than this is sent in pieces, so a file without newlines
/// cannot make the server buffer it whole.
const MAX_LINE: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
enum Message {
    Line(Arc<str>),
    Notice(Arc<str>),
}

impl Message {
    fn into_event(self) -> Event {
        match self {
            Message::Line(text) => json_event("line", &*text),
            Message::Notice(text) => json_event("notice", &*text),
        }
    }
}

/// Browsers drop SSE events whose data is empty, so every payload is sent as
/// JSON: an empty log line arrives as `""` instead of vanishing.
fn json_event(kind: &str, data: impl Serialize) -> Event {
    Event::default()
        .event(kind)
        .json_data(data)
        .expect("strings and integers always serialize to JSON")
}

/// Recent lines of one file plus the channel that carries new ones.
struct Hub {
    name: String,
    state: Mutex<HubState>,
}

struct HubState {
    backlog: VecDeque<Message>,
    /// `None` once following has stopped, which ends every open stream.
    sender: Option<broadcast::Sender<Message>>,
}

impl Hub {
    fn new(name: String) -> Self {
        let (sender, _) = broadcast::channel(CHANNEL);
        Self {
            name,
            state: Mutex::new(HubState {
                backlog: VecDeque::with_capacity(BACKLOG),
                sender: Some(sender),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        // The critical sections cannot panic halfway, so a poisoned lock
        // still guards consistent data.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, message: Message) {
        let mut state = self.lock();
        if state.backlog.len() == BACKLOG {
            state.backlog.pop_front();
        }
        state.backlog.push_back(message.clone());
        if let Some(sender) = &state.sender {
            // An error only means no browser is connected right now.
            let _ = sender.send(message);
        }
    }

    /// Copies the backlog and subscribes under one lock, so a line published
    /// concurrently lands in exactly one of the two.
    fn subscribe(&self) -> (Vec<Message>, broadcast::Receiver<Message>) {
        let state = self.lock();
        let receiver = match &state.sender {
            Some(sender) => sender.subscribe(),
            // Following is over: the receiver of a dropped channel ends at once.
            None => broadcast::channel(1).1,
        };
        (state.backlog.iter().cloned().collect(), receiver)
    }

    fn close(&self) {
        self.lock().sender = None;
    }
}

/// Turns the byte chunks the follow loop reads into whole lines. A line that
/// has no newline yet is held until it is completed.
struct HubSink {
    hubs: Arc<[Hub]>,
    partial: Vec<Vec<u8>>,
}

impl HubSink {
    fn new(hubs: Arc<[Hub]>) -> Self {
        let partial = vec![Vec::new(); hubs.len()];
        Self { hubs, partial }
    }

    fn push(&mut self, index: usize, data: &[u8]) {
        let (Some(hub), Some(partial)) = (self.hubs.get(index), self.partial.get_mut(index)) else {
            return;
        };
        let mut rest = data;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            partial.extend_from_slice(&rest[..newline]);
            hub.publish(Message::Line(line_text(partial)));
            partial.clear();
            rest = &rest[newline + 1..];
        }
        partial.extend_from_slice(rest);
        if partial.len() >= MAX_LINE {
            hub.publish(Message::Line(line_text(partial)));
            partial.clear();
        }
    }

    /// Publishes a held partial line, used before a notice so that a file
    /// that was truncated or replaced does not glue old and new text together.
    fn finish_partial(&mut self, index: usize) {
        if let (Some(hub), Some(partial)) = (self.hubs.get(index), self.partial.get_mut(index))
            && !partial.is_empty()
        {
            hub.publish(Message::Line(line_text(partial)));
            partial.clear();
        }
    }
}

fn line_text(bytes: &[u8]) -> Arc<str> {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into()
}

impl Sink for HubSink {
    fn write(&mut self, index: usize, _name: &str, data: &[u8]) -> io::Result<()> {
        self.push(index, data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn notice(&mut self, index: usize, message: &str) {
        eprintln!("rtail: {message}");
        self.finish_partial(index);
        if let Some(hub) = self.hubs.get(index) {
            hub.publish(Message::Notice(message.into()));
        }
    }
}

#[derive(Clone)]
struct AppState {
    hubs: Arc<[Hub]>,
    /// `Host` values accepted when bound to loopback; `None` accepts any.
    allowed_hosts: Option<Arc<[String]>>,
}

/// Serves `files` until following stops (every file gone, or `--pid` exited).
/// `backlogs[i]` is the initial tail of `files[i]`. Returns whether every file
/// was followed successfully, like [`follow::run`].
pub fn run(
    addr: SocketAddr,
    files: Vec<Followed>,
    backlogs: Vec<Vec<u8>>,
    opts: follow::Options,
) -> io::Result<bool> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = match runtime.block_on(TcpListener::bind(addr)) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("rtail: cannot listen on {addr}: {}", describe(&e));
            return Ok(false);
        }
    };
    let local = listener.local_addr()?;

    let hubs: Arc<[Hub]> = files.iter().map(|f| Hub::new(f.name.clone())).collect();
    let mut sink = HubSink::new(Arc::clone(&hubs));
    for (index, backlog) in backlogs.iter().enumerate() {
        sink.push(index, backlog);
    }

    let allowed_hosts = if local.ip().is_loopback() {
        Some(loopback_hosts(local).into())
    } else {
        eprintln!(
            "rtail: warning: {local} is reachable from other machines; anyone who can connect can read the followed files"
        );
        None
    };
    let host = if local.ip().is_unspecified() {
        format!("localhost:{}", local.port())
    } else {
        local.to_string()
    };
    eprintln!("rtail: serving {} file(s) on http://{host}/", hubs.len());

    let (done, stopped) = oneshot::channel();
    let follow_hubs = Arc::clone(&hubs);
    let follower = thread::spawn(move || {
        let result = follow::run(files, &opts, &mut sink);
        for hub in follow_hubs.iter() {
            hub.close();
        }
        let _ = done.send(());
        result
    });

    let app = router(AppState {
        hubs,
        allowed_hosts,
    });
    runtime.block_on(async {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    })?;

    follower
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The `Host` values a browser sends for this server when it is bound to
/// loopback. Browsers leave out the port when it is the default, 80.
fn loopback_hosts(local: SocketAddr) -> Vec<String> {
    let port = local.port();
    let bound = match local.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    let mut hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "[::1]".to_owned(),
        bound,
    ];
    hosts.sort();
    hosts.dedup();
    let mut allowed: Vec<String> = hosts.iter().map(|host| format!("{host}:{port}")).collect();
    if port == 80 {
        allowed.extend(hosts);
    }
    allowed
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/files", get(list_files))
        .route("/events/{index}", get(events))
        .layer(middleware::from_fn_with_state(state.clone(), check_host))
        .with_state(state)
}

/// Rejects requests whose `Host` is not this server. Without it, a web page on
/// any site could rebind its own domain to 127.0.0.1 and read the logs.
async fn check_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(allowed) = &state.allowed_hosts {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok());
        if !host.is_some_and(|host| allowed.iter().any(|a| a.eq_ignore_ascii_case(host))) {
            return (StatusCode::FORBIDDEN, "unexpected Host header\n").into_response();
        }
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

#[derive(Serialize)]
struct FileInfo<'a> {
    index: usize,
    name: &'a str,
}

async fn list_files(State(state): State<AppState>) -> Response {
    let files: Vec<FileInfo> = state
        .hubs
        .iter()
        .enumerate()
        .map(|(index, hub)| FileInfo {
            index,
            name: &hub.name,
        })
        .collect();
    axum::Json(files).into_response()
}

async fn events(
    State(state): State<AppState>,
    Path(index): Path<usize>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let hub = state.hubs.get(index).ok_or(StatusCode::NOT_FOUND)?;
    let (backlog, receiver) = hub.subscribe();
    Ok(Sse::new(stream(backlog, receiver)).keep_alive(KeepAlive::default()))
}

fn stream(
    backlog: Vec<Message>,
    receiver: broadcast::Receiver<Message>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let live = BroadcastStream::new(receiver).map(|item| match item {
        Ok(message) => message.into_event(),
        Err(BroadcastStreamRecvError::Lagged(missed)) => json_event("lagged", missed),
    });
    tokio_stream::iter(backlog)
        .map(Message::into_event)
        .chain(live)
        .map(Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hubs(n: usize) -> Arc<[Hub]> {
        (0..n).map(|i| Hub::new(format!("f{i}"))).collect()
    }

    fn lines(hub: &Hub) -> Vec<Message> {
        hub.subscribe().0
    }

    fn line(text: &str) -> Message {
        Message::Line(text.into())
    }

    #[test]
    fn splits_chunks_into_lines() {
        let hubs = hubs(2);
        let mut sink = HubSink::new(Arc::clone(&hubs));
        sink.push(0, b"one\ntw");
        sink.push(0, b"o\r\n\nthree");
        sink.push(1, b"other\n");
        assert_eq!(lines(&hubs[0]), [line("one"), line("two"), line("")]);
        assert_eq!(lines(&hubs[1]), [line("other")]);

        // A notice completes the held line first.
        sink.notice(0, "f0: file truncated");
        assert_eq!(
            lines(&hubs[0])[3..],
            [line("three"), Message::Notice("f0: file truncated".into())]
        );
    }

    #[test]
    fn splits_lines_without_newline_at_the_limit() {
        let hubs = hubs(1);
        let mut sink = HubSink::new(Arc::clone(&hubs));
        sink.push(0, &vec![b'x'; MAX_LINE - 1]);
        assert!(lines(&hubs[0]).is_empty());
        sink.push(0, b"x");
        sink.push(0, b"tail\n");
        let published = lines(&hubs[0]);
        assert_eq!(published.len(), 2);
        assert_eq!(published[0], line(&"x".repeat(MAX_LINE)));
        assert_eq!(published[1], line("tail"));
    }

    #[test]
    fn allows_the_hosts_a_browser_sends_on_loopback() {
        let hosts = loopback_hosts("127.0.0.2:8080".parse().unwrap());
        for host in [
            "localhost:8080",
            "127.0.0.1:8080",
            "[::1]:8080",
            "127.0.0.2:8080",
        ] {
            assert!(
                hosts.iter().any(|h| h == host),
                "{host} missing from {hosts:?}"
            );
        }
        assert!(!hosts.iter().any(|h| h == "localhost"));

        let hosts = loopback_hosts("[::1]:80".parse().unwrap());
        assert!(hosts.iter().any(|h| h == "localhost"));
        assert!(hosts.iter().any(|h| h == "[::1]:80"));
        assert_eq!(hosts.iter().filter(|h| *h == "[::1]:80").count(), 1);
    }

    #[test]
    fn replaces_invalid_utf8() {
        let hubs = hubs(1);
        HubSink::new(Arc::clone(&hubs)).push(0, b"bad \xff byte\n");
        assert_eq!(lines(&hubs[0]), [line("bad \u{fffd} byte")]);
    }

    #[test]
    fn backlog_keeps_the_most_recent_lines() {
        let hub = Hub::new("f".into());
        for i in 0..BACKLOG + 3 {
            hub.publish(line(&i.to_string()));
        }
        let backlog = lines(&hub);
        assert_eq!(backlog.len(), BACKLOG);
        assert_eq!(backlog[0], line("3"));
    }

    #[test]
    fn subscriber_sees_live_lines_until_closed() {
        let hub = Hub::new("f".into());
        hub.publish(line("old"));
        let (backlog, mut receiver) = hub.subscribe();
        assert_eq!(backlog, [line("old")]);

        hub.publish(line("new"));
        hub.close();
        assert_eq!(receiver.try_recv(), Ok(line("new")));
        assert_eq!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        );

        let (backlog, mut receiver) = hub.subscribe();
        assert_eq!(backlog.len(), 2);
        assert_eq!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        );
    }
}
