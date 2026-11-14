//! Steps for `features/security/request_checks.feature`: the real `ladderback
//! serve` process, spoken to in raw HTTP so each scenario controls every
//! header byte, `Host` included, which an HTTP client library would set for
//! itself.

use std::assert_matches;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use cucumber::{given, then, when};
use testkit::children::Owned;

use super::{Agent, LADDERBACK, blocking};

/// How long a freshly started daemon may take to listen or give up.
const START_TIMEOUT: Duration = Duration::from_secs(60);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const LISTENING: &str = "listening on http://";

/// What the request-check and sign-in scenarios keep between steps.
#[derive(Debug, Default)]
pub struct Web {
    pub(crate) server: Option<Server>,
    pub(crate) request: Option<Request>,
    pub(crate) response: Option<Response>,
    start: Option<Outcome>,
    /// The log of the daemon started last.
    log: Option<PathBuf>,
    /// Whether requests come from the UI's page, which sends an `Origin`.
    page: bool,
    /// The folders `ladderback set-password` keeps the admin password in,
    /// which every daemon the scenario starts then shares. Without it, each
    /// daemon gets folders of its own.
    pub(crate) home: Option<PathBuf>,
}

/// A running daemon, ended when the scenario ends or the run stops at its
/// limit.
#[derive(Debug)]
pub(crate) struct Server {
    #[allow(
        dead_code,
        reason = "owned so that the daemon is ended with the scenario"
    )]
    child: Owned,
    pub(crate) addr: SocketAddr,
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Listening(SocketAddr),
    Exited(ExitStatus, String),
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<String>,
    /// Whether the request names the daemon's own address as its `Host`.
    pub(crate) default_host: bool,
    /// What follows the headers, sent with a `Content-Length` of its size.
    pub(crate) body: String,
}

#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    headers: Vec<(String, String)>,
    pub(crate) body: String,
}

static LOGS: AtomicUsize = AtomicUsize::new(0);

/// Starts `ladderback serve` with `args`, and waits until it listens or exits.
/// The wait is on a blocking thread: see [`blocking`]. The daemon uses the
/// folders under `home`, or folders of its own; `rust_log`, when given, is
/// the `RUST_LOG` it logs at.
async fn start(
    scratch: &Path,
    home: Option<&Path>,
    args: &[&str],
    rust_log: Option<&'static str>,
) -> (Owned, Outcome, PathBuf) {
    let scratch = scratch.to_owned();
    let home = home.map(Path::to_owned);
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        spawn_daemon(&scratch, home.as_deref(), &args, rust_log)
    })
    .await
}

fn spawn_daemon(
    scratch: &Path,
    home: Option<&Path>,
    args: &[&str],
    rust_log: Option<&str>,
) -> (Owned, Outcome, PathBuf) {
    let started = LOGS.fetch_add(1, Ordering::Relaxed);
    let log_path = scratch.join(format!("serve-{started}.log"));
    let log = std::fs::File::create(&log_path).expect("a log file for the daemon");
    let mut command = Command::new(LADDERBACK);
    // Folders of its own, not the platform's, and not those of another daemon
    // this scenario started, unless it shares the admin password's.
    let own = scratch.join(format!("daemon-{started}"));
    testkit::daemon::isolate(&mut command, home.unwrap_or(&own));
    command
        .arg("serve")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // The banner is read in English, whatever language the machine or a
        // pseudolocale asks for: `C` makes the program ignore `LANGUAGE`,
        // which is removed too.
        .env("LC_ALL", "C")
        .env_remove("LANGUAGE");
    if let Some(rust_log) = rust_log {
        command.env("RUST_LOG", rust_log);
    }
    let child = Owned::spawn(&mut command).expect("ladderback starts");
    let began = Instant::now();
    loop {
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        // The log's last line may be half written: wait for its newline.
        if let Some(rest) = text.split(LISTENING).nth(1)
            && rest.contains('\n')
            && let Some(addr) = rest.lines().next().and_then(|l| l.trim().parse().ok())
        {
            return (child, Outcome::Listening(addr), log_path);
        }
        if let Some(status) = child.try_wait().expect("the daemon's status") {
            // Read again: it may have written its last words and exited
            // between the read above and this wait.
            let text = std::fs::read_to_string(&log_path).unwrap_or(text);
            return (child, Outcome::Exited(status, text), log_path);
        }
        assert!(
            began.elapsed() < START_TIMEOUT,
            "ladderback neither listened nor exited within {START_TIMEOUT:?}: {text}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Request {
    fn text(&self, port: u16) -> String {
        let mut text = format!("{} {} HTTP/1.1\r\n", self.method, self.path);
        if self.default_host {
            text.push_str(&format!("Host: localhost:{port}\r\n"));
        }
        for header in &self.headers {
            text.push_str(&header.replace("{port}", &port.to_string()));
            text.push_str("\r\n");
        }
        if !self.has_header("connection") {
            text.push_str("Connection: close\r\n");
        }
        text.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        text.push_str(&self.body);
        text
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|h| header_is(h, name))
    }
}

fn header_is(line: &str, name: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(n, _)| n.trim().eq_ignore_ascii_case(name))
}

/// Sends `request` over a new connection and reads one response.
pub(crate) fn exchange(addr: SocketAddr, request: &Request) -> Response {
    let mut stream = testkit::net::connect(addr).expect("a connection to the daemon");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("a read timeout");
    // A daemon that refuses a body over its limit answers at once and closes,
    // so a large body may find the connection gone before it is all written.
    // The answer is already on its way, and a write that failed for any other
    // reason shows as no answer, just below.
    let _ = stream.write_all(request.text(addr.port()).as_bytes());
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("a status line");
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("a status line, got {line:?}"));
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).expect("a header line");
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').expect("a header with a colon");
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    assert!(
        !headers.iter().any(|(n, _)| n == "transfer-encoding"),
        "the steps read bodies with a Content-Length only: {headers:?}"
    );
    let mut body = Vec::new();
    // A 101 Switching Protocols response has no body: the connection is the
    // WebSocket now, so reading to its end would wait for the timeout.
    if request.method != "HEAD" && status != 101 {
        match length {
            Some(length) => {
                body.resize(length, 0);
                reader.read_exact(&mut body).expect("the response body");
            }
            None => {
                reader.read_to_end(&mut body).expect("the response body");
            }
        }
    }
    Response {
        status,
        headers,
        body: String::from_utf8_lossy_owned(body),
    }
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Web {
    pub(crate) fn server(&self) -> &Server {
        self.server.as_ref().expect("Ladderback is serving")
    }

    pub(crate) fn request(&mut self) -> &mut Request {
        self.request.as_mut().expect("a request is being built")
    }

    pub(crate) fn response(&self) -> &Response {
        self.response.as_ref().expect("the request was sent")
    }
}

#[given(expr = "Ladderback is serving on loopback, configured for the host name {string}")]
async fn serving(agent: &mut Agent, name: String) {
    let (child, outcome, _) = start(
        agent.scratch(),
        None,
        &["--listen", "127.0.0.1:0", "--allowed-host", &name],
        None,
    )
    .await;
    match outcome {
        Outcome::Listening(addr) => agent.web.server = Some(Server { child, addr }),
        Outcome::Exited(status, log) => panic!("ladderback exited with {status}: {log}"),
    }
}

/// Starts `ladderback serve` on every address with sign-in off and `name` as
/// its one host name: on the network, as a home server is. Requests reach it
/// at its loopback address, the same socket, with the `Host` each scenario
/// sets, so no name has to resolve to it.
#[given(
    expr = "Ladderback is serving on every address with sign-in off, configured for the host name {string}"
)]
async fn serving_on_the_network(agent: &mut Agent, name: String) {
    let (child, outcome, log) = start(
        agent.scratch(),
        None,
        &["--listen", "0.0.0.0:0", "--allowed-host", &name],
        None,
    )
    .await;
    match outcome {
        Outcome::Listening(addr) => {
            assert!(addr.ip().is_unspecified(), "listening on {addr}");
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()));
            agent.web.server = Some(Server { child, addr });
        }
        Outcome::Exited(status, log) => panic!("ladderback exited with {status}: {log}"),
    }
    agent.web.log = Some(log);
}

#[given("the request comes from the UI's page")]
fn from_the_page(agent: &mut Agent) {
    agent.web.page = true;
}

#[given(expr = "a {string} request for {string}")]
pub(crate) fn request_for(agent: &mut Agent, method: String, path: String) {
    // A page's request carries its origin, which is the daemon's own address.
    let headers = if agent.web.page {
        vec!["Origin: http://localhost:{port}".to_owned()]
    } else {
        Vec::new()
    };
    agent.web.request = Some(Request {
        method,
        path,
        headers,
        default_host: true,
        body: String::new(),
    });
}

#[given(expr = "the request has the header {string}")]
fn add_header(agent: &mut Agent, header: String) {
    let request = agent.web.request();
    if header_is(&header, "host") {
        request.default_host = false;
    }
    request.headers.push(header);
}

#[given(expr = "the request has a JSON body of {int} bytes")]
fn json_body(agent: &mut Agent, size: usize) {
    // A body that is valid JSON of the shape the operations route takes, padded
    // to the size: a route that read it would take it.
    const FRAME: &str = r#"{"plan":""}"#;
    let padding = size
        .checked_sub(FRAME.len())
        .expect("a JSON body holds at least its braces and key");
    agent.web.request().body = format!(r#"{{"plan":"{}"}}"#, "a".repeat(padding));
}

#[given(expr = "the request has a body of {int} byte(s)")]
fn plain_body(agent: &mut Agent, size: usize) {
    agent.web.request().body = "x".repeat(size);
}

#[given("the request has no Host header")]
fn no_host(agent: &mut Agent) {
    agent.web.request().default_host = false;
}

#[given("the request asks to upgrade to a WebSocket")]
fn websocket(agent: &mut Agent) {
    agent.web.request().headers.extend(
        [
            "Connection: Upgrade",
            "Upgrade: websocket",
            "Sec-WebSocket-Version: 13",
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
        ]
        .map(str::to_owned),
    );
}

#[when("the request is sent")]
pub(crate) fn send(agent: &mut Agent) {
    let addr = agent.web.server().addr;
    let request = agent.web.request.as_ref().expect("a request is built");
    agent.web.response = Some(exchange(addr, request));
}

/// A refusal is JSON with the error code and nothing else.
fn refusal_code(response: &Response) -> Option<&str> {
    response
        .body
        .strip_prefix(r#"{"code":""#)
        .and_then(|rest| rest.strip_suffix(r#""}"#))
}

#[then("the request isn't refused by a request check")]
fn not_refused(agent: &mut Agent) {
    let response = agent.web.response();
    assert_eq!(
        refusal_code(response),
        None,
        "refused with status {}",
        response.status
    );
    // What the UI's own routes answer: the page, no such file, no such method,
    // or (when this build has no UI) none. Anything else, a 400 from a request
    // the server couldn't read, say, is not a request that was served.
    assert_matches!(
        response.status,
        200 | 404 | 405 | 503,
        "status {} with body {:?}",
        response.status,
        response.body
    );
}

#[then(expr = "the request is refused with status {int} and the error code {string}")]
fn refused(agent: &mut Agent, status: u16, code: String) {
    let response = agent.web.response();
    assert_eq!(
        (response.status, refusal_code(response)),
        (status, Some(code.as_str())),
        "body {:?}",
        response.body
    );
    assert_eq!(response.header("content-type"), Some("application/json"));
}

#[then("the WebSocket handshake is accepted")]
fn handshake_accepted(agent: &mut Agent) {
    let response = agent.web.response();
    assert_eq!(response.status, 101, "body {:?}", response.body);
    assert_eq!(response.header("upgrade"), Some("websocket"));
    // The key the upgrade step sends is the RFC 6455 sample key, whose
    // accept is the sample accept.
    assert_eq!(
        response.header("sec-websocket-accept"),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
    );
}

#[then("the request isn't refused for its size")]
fn not_refused_for_size(agent: &mut Agent) {
    let response = agent.web.response();
    assert_ne!(response.status, 413, "body {:?}", response.body);
    assert_ne!(refusal_code(response), Some("body-too-large"));
}

#[then(
    expr = "the request is refused with status {int} and the error code {string}, naming the limit of {int} bytes"
)]
fn refused_naming_limit(agent: &mut Agent, status: u16, code: String, limit: u64) {
    let response = agent.web.response();
    let body: serde_json::Value = serde_json::from_str(&response.body).expect("a refusal is JSON");
    assert_eq!(response.status, status, "body {:?}", response.body);
    assert_eq!(
        body,
        serde_json::json!({"code": code, "args": {"limit": {"type": "bytes", "value": limit}}})
    );
    assert_eq!(response.header("content-type"), Some("application/json"));
}

#[then(expr = "the response has the header {string} with the value {string}")]
fn has_header_value(agent: &mut Agent, name: String, value: String) {
    assert_eq!(agent.web.response().header(&name), Some(value.as_str()));
}

#[then(expr = "the response has a {string} header")]
fn has_header(agent: &mut Agent, name: String) {
    assert!(
        agent.web.response().header(&name).is_some(),
        "no {name} in {:?}",
        agent.web.response().headers
    );
}

#[then(expr = "the response's {string} has the directive {string}")]
fn has_directive(agent: &mut Agent, name: String, directive: String) {
    let policy = agent
        .web
        .response()
        .header(&name)
        .unwrap_or_else(|| panic!("no {name} header"));
    // A directive may carry more sources after the ones named, such as the
    // hash of the page's own script.
    assert!(
        policy
            .split(';')
            .map(str::trim)
            .any(|have| have == directive
                || have
                    .strip_prefix(&directive)
                    .is_some_and(|rest| rest.starts_with(' '))),
        "{name} is {policy:?}"
    );
}

#[then(expr = "the response has no {string} header")]
fn no_header(agent: &mut Agent, name: String) {
    assert_eq!(agent.web.response().header(&name), None);
}

#[when(expr = "Ladderback is started with the arguments {string}")]
async fn started_with(agent: &mut Agent, arguments: String) {
    start_with(agent, &arguments, None).await;
}

#[when(expr = "Ladderback is started with the log turned off and the arguments {string}")]
async fn started_with_the_log_off(agent: &mut Agent, arguments: String) {
    start_with(agent, &arguments, Some("off")).await;
}

async fn start_with(agent: &mut Agent, arguments: &str, rust_log: Option<&'static str>) {
    let args: Vec<&str> = arguments.split_whitespace().collect();
    let home = agent.web.home.clone();
    let (child, outcome, log) = start(agent.scratch(), home.as_deref(), &args, rust_log).await;
    if let Outcome::Listening(addr) = &outcome {
        // Stopped when the scenario ends, as the daemon from the Background.
        agent.web.server = Some(Server { child, addr: *addr });
    }
    agent.web.start = Some(outcome);
    agent.web.log = Some(log);
}

/// Starts `ladderback serve` on a free loopback port with the scenario's
/// shared folders, and keeps it as the daemon requests go to.
pub(crate) async fn serve_on_loopback(agent: &mut Agent) {
    let home = agent.web.home.clone();
    let (child, outcome, log) = start(
        agent.scratch(),
        home.as_deref(),
        &["--listen", "127.0.0.1:0"],
        None,
    )
    .await;
    match outcome {
        Outcome::Listening(addr) => agent.web.server = Some(Server { child, addr }),
        Outcome::Exited(status, text) => panic!("ladderback exited with {status}: {text}"),
    }
    agent.web.log = Some(log);
}

#[then("Ladderback is listening")]
fn listening(agent: &mut Agent) {
    match agent.web.start.as_ref().expect("Ladderback was started") {
        Outcome::Listening(_) => {}
        Outcome::Exited(status, log) => panic!("ladderback exited with {status}: {log}"),
    }
}

#[then(expr = "its log warns {string}")]
fn log_warns(agent: &mut Agent, words: String) {
    let path = agent.web.log.as_ref().expect("Ladderback was started");
    let log = std::fs::read_to_string(path).expect("the daemon's log");
    assert!(
        log.lines()
            .any(|line| line.contains("WARN") && line.contains(&words)),
        "no warning saying {words:?} in: {log}"
    );
}

/// A line the daemon says itself, after its name, rather than a log record:
/// no `RUST_LOG` can hide it (§10.1).
#[then(expr = "Ladderback warns {string}")]
fn warns(agent: &mut Agent, words: String) {
    let path = agent.web.log.as_ref().expect("Ladderback was started");
    let log = std::fs::read_to_string(path).expect("the daemon's log");
    assert!(
        log.lines()
            .any(|line| line.starts_with("ladderback: ") && line.contains(&words)),
        "Ladderback said nothing saying {words:?} in: {log}"
    );
}

/// Nothing the daemon says itself holds these words.
#[then(expr = "Ladderback doesn't warn {string}")]
fn doesnt_warn(agent: &mut Agent, words: String) {
    let path = agent.web.log.as_ref().expect("Ladderback was started");
    let log = std::fs::read_to_string(path).expect("the daemon's log");
    assert!(
        !log.lines()
            .any(|line| line.starts_with("ladderback: ") && line.contains(&words)),
        "Ladderback said {words:?} in: {log}"
    );
}

/// The warning names the address the daemon took, its port included rather
/// than the 0 it was asked for, and comes before the line that says where it
/// listens, which scripts and people read first.
#[then(
    expr = "Ladderback warns {string} and the address it listens on, before it says where it listens"
)]
fn warns_at_the_listened_address(agent: &mut Agent, words: String) {
    let Some(Outcome::Listening(addr)) = agent.web.start.as_ref() else {
        panic!("Ladderback is not listening");
    };
    let path = agent.web.log.as_ref().expect("Ladderback was started");
    let log = std::fs::read_to_string(path).expect("the daemon's log");
    let said = format!("{words} {addr}");
    let lines: Vec<&str> = log.lines().collect();
    let warned = lines
        .iter()
        .position(|line| line.starts_with("ladderback: ") && line.contains(&said));
    let listening = lines.iter().position(|line| line.contains(LISTENING));
    assert!(
        matches!((warned, listening), (Some(warned), Some(listening)) if warned < listening),
        "Ladderback did not say {said:?} before where it listens in: {log}"
    );
}

/// The daemon's exit status and log, when it didn't start listening.
pub(crate) fn exited(agent: &Agent) -> (ExitStatus, &str) {
    match agent.web.start.as_ref().expect("Ladderback was started") {
        Outcome::Listening(addr) => panic!("Ladderback is listening on {addr}"),
        Outcome::Exited(status, log) => (*status, log),
    }
}

#[then("Ladderback refuses to start")]
fn refuses(agent: &mut Agent) {
    let (status, log) = exited(agent);
    // Exit code 1 is a refusal; a panic (101) or a signal isn't.
    assert_eq!(status.code(), Some(1), "log: {log}");
}

#[then(expr = "Ladderback refuses to start, naming {string}")]
fn refuses_naming(agent: &mut Agent, words: String) {
    let (status, log) = exited(agent);
    assert_eq!(status.code(), Some(1), "log: {log}");
    assert!(
        log.lines()
            .any(|line| line.starts_with("ladderback: ") && line.contains(&words)),
        "no refusal naming {words:?} in: {log}"
    );
}

#[then(expr = "Ladderback rejects the argument {string}")]
fn rejects(agent: &mut Agent, value: String) {
    let (status, log) = exited(agent);
    // Exit code 2 is how the argument parser rejects a value.
    assert_eq!(status.code(), Some(2), "log: {log}");
    // What a person reads is Ladderback's own message, from the catalog (§8.4),
    // not the argument parser's English.
    let message = log
        .lines()
        .find(|line| line.starts_with("ladderback: ") && line.contains(&value));
    assert!(
        message.is_some(),
        "no message of Ladderback's own naming {value:?} in: {log}"
    );
}
