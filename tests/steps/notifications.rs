//! Steps for the `@e2e` scenarios of `features/notifications/rules.feature`
//! and `features/execution/restart.feature` that a webhook is told of: it
//! listens on loopback, its URL is in a file in the scenario's scratch
//! folder, and the daemon's config.toml has a rule that sends to it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cucumber::{given, then};
use domain::notifications::{
    Channel, ChannelKind, ChannelName, EventKind, Notifications, Rule, RuleName,
};
use domain::secrets::{FileRead, SecretRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::Agent;

/// The longest the daemon may take to tell the webhook, after the run was
/// accepted or the daemon started again: the backup of three small files
/// and one delivery on loopback.
const TOLD_WITHIN: Duration = Duration::from_secs(30);

/// The most of one request the webhook reads.
const MAX_REQUEST: usize = 64 * 1024;

/// The notifications the daemon's config.toml holds, and the bodies the
/// scenario's webhook was posted.
#[derive(Default)]
pub struct NotificationsE2e {
    pub notifications: Option<Notifications>,
    bodies: Arc<Mutex<Vec<String>>>,
}

/// The body of one POST, once its headers and as much of it as its
/// `Content-Length` says have arrived.
async fn read_body(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || bytes.len() > MAX_REQUEST {
            return None;
        }
        bytes.extend_from_slice(chunk.get(..read)?);
        let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(bytes.get(..end)?).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|length| length.trim().parse().ok())
            .unwrap_or(0);
        if bytes.len() >= end + 4 + length {
            return String::from_utf8(bytes.get(end + 4..end + 4 + length)?.to_vec()).ok();
        }
    }
}

#[given(expr = "a webhook listening on loopback that the rule {string} sends {string} to")]
async fn a_webhook_on_loopback(agent: &mut Agent, name: String, event: String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let bodies = agent.notifications.bodies.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let bodies = bodies.clone();
            tokio::spawn(async move {
                if let Some(body) = read_body(&mut stream).await {
                    bodies
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(body);
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    // The URL is a secret: the config names the file it is in.
    let file = agent.scratch().join("webhook-url");
    std::fs::write(&file, format!("http://127.0.0.1:{port}/demo-hook\n")).expect("the URL file");
    let url = SecretRef::File {
        path: file.to_str().expect("a UTF-8 path").to_owned(),
        read: FileRead::FirstLine,
    };
    let channel = ChannelName::parse("hook").expect("a channel name");
    let rule = Rule::new(
        BTreeSet::from([EventKind::parse(&event).expect("an event")]),
        channel.clone(),
        BTreeSet::new(),
        None,
    )
    .expect("a rule");
    agent.notifications.notifications = Some(
        Notifications::new(
            BTreeMap::from([(
                channel,
                Channel::new(ChannelKind::Webhook, url, None).expect("a channel"),
            )]),
            BTreeMap::from([(RuleName::parse(&name).expect("a rule name"), rule)]),
        )
        .expect("the rule sends to the channel"),
    );
}

#[then(expr = "the webhook is told {string} for the plan {string}")]
async fn the_webhook_is_told(agent: &mut Agent, event: String, plan: String) {
    told_once(agent, &event, &plan).await;
}

#[then(expr = "the webhook is told {string} for the plan {string} with the code {string}")]
async fn the_webhook_is_told_the_code(
    agent: &mut Agent,
    event: String,
    plan: String,
    code: String,
) {
    let body = told_once(agent, &event, &plan).await;
    assert_eq!(body["code"], code.as_str(), "{body}");
}

/// The one notification the webhook is told, once it has come, checked for
/// its `event`, its `plan` and a message.
async fn told_once(agent: &Agent, event: &str, plan: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + TOLD_WITHIN;
    loop {
        let bodies = agent
            .notifications
            .bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut told: Vec<serde_json::Value> = bodies
            .iter()
            .filter_map(|body| serde_json::from_str(body).ok())
            .collect();
        if let Some(body) = told.pop() {
            assert!(told.is_empty(), "one notification: {bodies:?}");
            assert_eq!(body["event"], event, "{body}");
            assert_eq!(body["plan"], plan, "{body}");
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "a message: {body}"
            );
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the webhook was told nothing within {TOLD_WITHIN:?}: {bodies:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
