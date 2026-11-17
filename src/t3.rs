//! Follows T3 Code's live thread list and reports threads whose work just ended.
//!
//! The desktop app's sidebar gets its thread list from the `orchestration.subscribeShell`
//! stream on `/ws`. We subscribe to the same stream with a read-only token, so we sleep
//! until T3 pushes a change instead of polling.

use serde::de::IgnoredAny;
use serde::Deserialize;
use std::collections::HashMap;
use std::io;
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

/// A thread must stay done this long before it counts. A queued follow-up message or a
/// delegate's result starts the next turn within this window.
const GRACE: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(5);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const SUBSCRIBE: &str = r#"{"_tag":"Request","id":"1","tag":"orchestration.subscribeShell","payload":{"omitIdleSubagents":true,"omitOlderSettled":true,"requestCompletionMarker":true},"headers":[]}"#;
const ACK: &str = r#"{"_tag":"Ack","requestId":"1"}"#;
const PONG: &str = r#"{"_tag":"Pong"}"#;

/// What the watcher reports to the tray.
pub enum Event<'a> {
    Offline,
    Watching { working: usize },
    Finished { link: &'a str },
}

#[derive(Deserialize)]
#[serde(tag = "_tag")]
enum Envelope {
    Chunk { values: Vec<Item> },
    Ping {},
    Exit {},
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "kind")]
enum Item {
    #[serde(rename = "snapshot")]
    Snapshot { snapshot: Snapshot },
    #[serde(rename = "synchronized")]
    Synchronized {},
    #[serde(rename = "thread.updated")]
    Updated { location: String, thread: Thread },
    #[serde(rename = "thread.removed")]
    Removed {
        #[serde(rename = "threadId")]
        thread_id: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    threads: Vec<Thread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    id: String,
    #[serde(default)]
    status: String,
    activity_run_status: Option<String>,
    #[serde(default)]
    pending_background_tasks: Vec<BackgroundTask>,
    pending_runtime_request: Option<RuntimeRequest>,
    lineage: Option<Lineage>,
    delegated_from: Option<IgnoredAny>,
    archived_at: Option<IgnoredAny>,
    deleted_at: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct BackgroundTask {
    kind: String,
    awaited: Option<bool>,
}

#[derive(Deserialize)]
struct RuntimeRequest {
    blocking: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lineage {
    relationship_to_parent: Option<String>,
    side_chat: Option<bool>,
}

impl Thread {
    /// Threads the user started. Subagents, side chats and delegated tasks belong to
    /// another thread, which reports for them.
    fn is_watched(&self) -> bool {
        let lineage = self.lineage.as_ref();
        lineage.and_then(|l| l.relationship_to_parent.as_deref()) != Some("subagent")
            && lineage.and_then(|l| l.side_chat) != Some(true)
            && self.delegated_from.is_none()
            && self.archived_at.is_none()
            && self.deleted_at.is_none()
    }

    /// Mirrors T3's own rule (`listedThreadStatus` in OrchestratorMcpService.ts): a thread
    /// is live while a run is active, or while it waits on a subagent, a delegated task or
    /// work the agent marked as awaited. A blocking question for the user ends the wait.
    fn is_working(&self) -> bool {
        if self.pending_runtime_request.as_ref().is_some_and(|r| r.blocking != Some(false)) {
            return false;
        }
        let holds = self.pending_background_tasks.iter().any(|t| {
            t.awaited == Some(true) || matches!(t.kind.as_str(), "subagent" | "background_task")
        });
        let status = match self.activity_run_status.as_deref() {
            Some(active) => active,
            None if self.status == "failed" => "failed",
            None if holds => "waiting",
            None => &self.status,
        };
        matches!(status, "queued" | "preparing" | "starting" | "running" | "waiting")
    }
}

enum Failure {
    Offline,
    Unauthorized,
}

pub struct Watcher {
    home: PathBuf,
    token: Option<String>,
    /// Watched threads and whether each one is working. Kept across reconnects, so a thread
    /// that finished while the socket was down still counts.
    working: HashMap<String, bool>,
    /// Threads that stopped working, with the time they stopped.
    finished: Vec<(String, Instant)>,
}

impl Watcher {
    pub fn new() -> Self {
        let home = std::env::var_os("T3CODE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".t3")))
            .unwrap_or_default();
        Self { home, token: None, working: HashMap::new(), finished: Vec::new() }
    }

    /// Runs forever, reconnecting whenever T3 Code stops or restarts.
    pub fn run(mut self, mut report: impl FnMut(Event)) -> ! {
        loop {
            if let Err(Failure::Unauthorized) = self.session(&mut report) {
                self.token = None;
            }
            self.finished.clear();
            report(Event::Offline);
            std::thread::sleep(RETRY);
        }
    }

    fn session(&mut self, report: &mut impl FnMut(Event)) -> Result<(), Failure> {
        let userdata = self.home.join("userdata");
        let runtime = std::fs::read(userdata.join("server-runtime.json")).map_err(|_| Failure::Offline)?;
        let origin: Origin = serde_json::from_slice(&runtime).map_err(|_| Failure::Offline)?;
        let host = origin.origin.strip_prefix("http://").ok_or(Failure::Offline)?.to_owned();
        let environment = std::fs::read_to_string(userdata.join("environment-id")).map_err(|_| Failure::Offline)?;
        let link_prefix = format!("t3code://threads/{}/", environment.trim());

        let stream = TcpStream::connect(&host).map_err(|_| Failure::Offline)?;
        if self.token.is_none() {
            self.token = Some(self.issue_token().ok_or(Failure::Offline)?);
        }
        let mut request = format!("ws://{host}/ws?orchestrationProtocol=2")
            .into_client_request()
            .map_err(|_| Failure::Offline)?;
        let bearer = format!("Bearer {}", self.token.as_deref().unwrap_or_default());
        request.headers_mut().insert(
            "Authorization",
            bearer.parse().map_err(|_| Failure::Unauthorized)?,
        );
        let (mut socket, _) = tungstenite::client(request, stream).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) if r.status() == 401 => {
                Failure::Unauthorized
            }
            _ => Failure::Offline,
        })?;
        socket.send(Message::text(SUBSCRIBE)).map_err(|_| Failure::Offline)?;

        // Threads seen in the snapshot that opens each subscription, until it is complete.
        let mut snapshot: Option<HashMap<String, bool>> = Some(HashMap::new());
        let mut last_count = usize::MAX;
        loop {
            let message = match read(&mut socket, self.finished.first().map(|f| f.1 + GRACE)) {
                Ok(Some(message)) => message,
                Ok(None) => {
                    self.report_finished(&link_prefix, report);
                    continue;
                }
                Err(_) => return Err(Failure::Offline),
            };
            let Message::Text(text) = message else { continue };
            // Acknowledge every chunk, even one we cannot read, or the server stops sending.
            if text.starts_with(r#"{"_tag":"Chunk""#) {
                socket.send(Message::text(ACK)).map_err(|_| Failure::Offline)?;
            }
            match serde_json::from_str::<Envelope>(&text) {
                Ok(Envelope::Chunk { values }) => {
                    for item in values {
                        self.apply(item, &mut snapshot);
                    }
                }
                Ok(Envelope::Ping {}) => socket.send(Message::text(PONG)).map_err(|_| Failure::Offline)?,
                Ok(Envelope::Exit {}) => return Err(Failure::Offline),
                Ok(Envelope::Other) | Err(_) => continue,
            }
            if snapshot.is_none() {
                let count = self.working.values().filter(|w| **w).count();
                if count != last_count {
                    last_count = count;
                    report(Event::Watching { working: count });
                }
            }
        }
    }

    fn apply(&mut self, item: Item, snapshot: &mut Option<HashMap<String, bool>>) {
        match item {
            Item::Snapshot { snapshot: s } => {
                for thread in s.threads.into_iter().filter(Thread::is_watched) {
                    let working = thread.is_working();
                    match snapshot {
                        Some(fresh) => {
                            fresh.insert(thread.id, working);
                        }
                        None => self.update(thread.id, working),
                    }
                }
            }
            Item::Synchronized {} => {
                if let Some(fresh) = snapshot.take() {
                    let before = std::mem::replace(&mut self.working, fresh);
                    let now = Instant::now();
                    for (id, was_working) in before {
                        if was_working && self.working.get(&id) == Some(&false) {
                            self.finished.push((id, now));
                        }
                    }
                }
            }
            Item::Updated { location, thread } => {
                if location == "active" && thread.is_watched() {
                    let working = thread.is_working();
                    match snapshot {
                        Some(fresh) => {
                            fresh.insert(thread.id, working);
                        }
                        None => self.update(thread.id, working),
                    }
                } else {
                    self.working.remove(&thread.id);
                    if let Some(fresh) = snapshot {
                        fresh.remove(&thread.id);
                    }
                }
            }
            Item::Removed { thread_id } => {
                self.working.remove(&thread_id);
                if let Some(fresh) = snapshot {
                    fresh.remove(&thread_id);
                }
            }
            Item::Other => {}
        }
    }

    fn update(&mut self, id: String, working: bool) {
        if self.working.insert(id.clone(), working) == Some(true) && !working {
            self.finished.push((id, Instant::now()));
        }
    }

    /// Reports threads that stayed done for the whole grace period.
    fn report_finished(&mut self, link_prefix: &str, report: &mut impl FnMut(Event)) {
        let now = Instant::now();
        let ready = self.finished.partition_point(|(_, at)| *at + GRACE <= now);
        for (id, _) in self.finished.drain(..ready) {
            if self.working.get(&id) == Some(&false) {
                report(Event::Finished { link: &format!("{link_prefix}{id}") });
            }
        }
    }

    /// Asks the t3 CLI for a read-only token that expires after a day; the server then
    /// rejects it and we ask again. Revokes the tokens of earlier runs first, so at most
    /// one `settled` session exists.
    fn issue_token(&self) -> Option<String> {
        if let Some(sessions) = self.cli("auth session list --json") {
            let sessions: Vec<Session> = serde_json::from_slice(&sessions).unwrap_or_default();
            for session in sessions.iter().filter(|s| s.client.label.as_deref() == Some(LABEL)) {
                self.cli(&format!("auth session revoke {}", session.session_id));
            }
        }
        let issued = self.cli(&format!("auth session issue --scope orchestration:read --ttl 1d --label {LABEL} --json"))?;
        serde_json::from_slice::<Issued>(&issued).ok().map(|i| i.token)
    }

    /// Runs the t3 CLI without a console window and returns its output if it succeeded.
    fn cli(&self, args: &str) -> Option<Vec<u8>> {
        let launcher = self.home.join("bin").join("t3.cmd");
        let output = Command::new("cmd")
            .raw_arg(format!("/d /s /c \"\"{}\" {args}\"", launcher.display()))
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }
}

const LABEL: &str = "settled";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    session_id: String,
    client: SessionClient,
}

#[derive(Deserialize)]
struct SessionClient {
    label: Option<String>,
}

#[derive(Deserialize)]
struct Issued {
    token: String,
}

#[derive(Deserialize)]
struct Origin {
    origin: String,
}

/// Reads the next message. Blocks without a deadline; with one, returns `Ok(None)` once
/// the deadline passes.
fn read(socket: &mut WebSocket<TcpStream>, deadline: Option<Instant>) -> io::Result<Option<Message>> {
    let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)));
    socket.get_mut().set_read_timeout(timeout)?;
    match socket.read() {
        Ok(message) => Ok(Some(message)),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
        {
            Ok(None)
        }
        Err(e) => Err(io::Error::other(e)),
    }
}
