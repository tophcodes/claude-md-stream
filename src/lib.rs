//! A Claude Code session transcript, rendered as streaming Markdown.
//!
//! The three stages are separate so a frontend can take any of them: resolve a
//! target to the files it writes, turn a transcript line into units, render a
//! unit as Markdown.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;

/// UTC in the shape the transcript uses, so `at=` means one thing throughout.
/// Civil date from a day count, after Howard Hinnant's algorithm.
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The outbox log `claude-send` appends to, beside the buffer it sent.
pub fn sent_log(outbox_dir: &Path) -> PathBuf {
    outbox_dir.join("sent.jsonl")
}

/// The files one Claude Code session writes.
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    /// `<project>/<id>.jsonl`
    pub transcript: PathBuf,
    /// `<project>/<id>/`, holding `subagents/` and `tool-results/`.
    pub dir: PathBuf,
}

/// What the user named on the command line: a herdr agent, or a session id.
pub enum Target {
    Agent(String),
    SessionId(String),
}

impl Target {
    /// A name that looks like a UUID is a session, anything else an agent.
    pub fn parse(raw: &str) -> Self {
        let uuidish = raw.len() >= 8
            && raw
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-')
            && raw.chars().any(|c| c.is_ascii_digit());
        if uuidish {
            Target::SessionId(raw.to_string())
        } else {
            Target::Agent(raw.to_string())
        }
    }
}

fn projects_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is unset")?;
    Ok(PathBuf::from(home).join(".claude/projects"))
}

/// Finds `<id>.jsonl` under any project directory. Avoids reproducing Claude
/// Code's rule for turning a working directory into a directory name.
fn find_transcript(id: &str) -> Result<PathBuf> {
    let root = projects_dir()?;
    for project in fs::read_dir(&root).with_context(|| format!("reading {root:?}"))? {
        let candidate = project?.path().join(format!("{id}.jsonl"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!("no transcript for session {id}"))
}

fn herdr_agents() -> Result<Vec<Value>> {
    let out = Command::new("herdr")
        .args(["agent", "list"])
        .output()
        .context("running `herdr agent list`")?;
    if !out.status.success() {
        return Err(anyhow!("herdr agent list failed"));
    }
    let parsed: Value = serde_json::from_slice(&out.stdout)?;
    Ok(parsed["result"]["agents"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// The agent's own identifiers, any of which the user may have typed.
fn agent_matches(agent: &Value, name: &str) -> bool {
    ["name", "pane_id", "workspace_id", "tab_id", "terminal_id"]
        .iter()
        .any(|k| agent[k].as_str() == Some(name))
}

fn session_of(agent: &Value) -> Option<(String, PathBuf)> {
    let id = agent["agent_session"]["value"].as_str()?.to_string();
    let cwd = PathBuf::from(agent["cwd"].as_str()?);
    Some((id, cwd))
}

fn session_from_parts(id: String, cwd: PathBuf) -> Result<Session> {
    let transcript = find_transcript(&id)?;
    let dir = transcript
        .parent()
        .ok_or_else(|| anyhow!("transcript has no parent"))?
        .join(&id);
    Ok(Session {
        id,
        cwd,
        transcript,
        dir,
    })
}

/// Resolves a target through `herdr agent list`, or directly for a session id.
pub fn resolve(target: &Target) -> Result<Session> {
    match target {
        Target::SessionId(id) => {
            let transcript = find_transcript(id)?;
            let cwd = first_cwd(&transcript).unwrap_or_default();
            session_from_parts(id.clone(), cwd)
        }
        Target::Agent(name) => {
            let agent = herdr_agents()?
                .into_iter()
                .find(|a| agent_matches(a, name))
                .ok_or_else(|| anyhow!("no herdr agent and no session named '{name}'"))?;
            let (id, cwd) = session_of(&agent)
                .ok_or_else(|| anyhow!("agent '{name}' reports no session"))?;
            session_from_parts(id, cwd)
        }
    }
}

/// The `cwd` a transcript records, for a session named without an agent.
fn first_cwd(transcript: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(transcript).ok()?;
    for line in text.lines().take(50) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some(cwd) = v["cwd"].as_str() {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

/// What one look at the agent list says about a followed pane. Both answers
/// come from the same call so following costs one subprocess, not two.
pub struct Poll {
    /// Set when the pane now holds a different session.
    pub switched: Option<Session>,
    /// `working`, `idle`, `blocked`, as herdr reports it. `None` for a session
    /// followed by id, which has no pane to ask about.
    pub state: Option<String>,
}

/// Re-reads the agent list to see whether the pane moved on, and what the agent
/// is doing. The state is the only thing in this tool that does not come from
/// the transcript, and it never enters a unit's content.
pub fn poll(target: &Target, current: &Session) -> Result<Poll> {
    let mut out = Poll {
        switched: None,
        state: None,
    };
    let Target::Agent(name) = target else {
        return Ok(out);
    };
    let Some(agent) = herdr_agents()?.into_iter().find(|a| agent_matches(a, name)) else {
        return Ok(out);
    };
    out.state = agent["agent_status"].as_str().map(str::to_string);
    if let Some((id, cwd)) = session_of(&agent) {
        if id != current.id {
            out.switched = Some(session_from_parts(id, cwd)?);
        }
    }
    Ok(out)
}

/// Which conversation a unit belongs to. Several subagents run at once, so
/// this rides on every unit rather than being bracketed around a run of them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Thread {
    Main,
    Sidechain(String),
}

impl Thread {
    fn label(&self) -> &str {
        match self {
            Thread::Main => "main",
            Thread::Sidechain(id) => id,
        }
    }
}

/// Ordering and identity for a unit, emitted as an HTML comment so it stays
/// invisible in a plain Markdown renderer. A frontend that separates the
/// threads reconciles them from `thread` and from the tool ids.
pub struct Anchor {
    pub at: String,
    pub uuid: String,
    pub thread: Thread,
}

pub enum Status {
    Ok,
    Error,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Error => "error",
        }
    }
}

pub enum MetaKind {
    SessionStart,
    SessionSwitch,
    Compact,
    /// A subagent appeared. Carries the tool id of the `Task` call that started
    /// it, so a frontend can place the thread against its caller.
    SidechainStart,
    SidechainEnd,
    UnknownBlock,
    /// The agent changed between working and waiting. Out of band: this comes
    /// from herdr, not from the transcript.
    Status,
}

impl MetaKind {
    fn label(&self) -> &'static str {
        match self {
            MetaKind::SessionStart => "session-start",
            MetaKind::SessionSwitch => "session-switch",
            MetaKind::Compact => "compact",
            MetaKind::SidechainStart => "sidechain-start",
            MetaKind::SidechainEnd => "sidechain-end",
            MetaKind::UnknownBlock => "unknown-block",
            MetaKind::Status => "status",
        }
    }
}

/// One renderable thing. Anything the transcript carries that is not one of
/// these is dropped before a unit exists.
pub enum Unit {
    Prompt {
        text: String,
    },
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        for_id: String,
        status: Status,
        lines: usize,
        body: String,
        /// Path the envelope offered for the full output, when the inline body
        /// is a truncation. Read through [`read_sidecar`].
        sidecar: Option<String>,
    },
    Meta {
        kind: MetaKind,
        fields: Vec<(String, String)>,
    },
    /// Text handed to the agent, recorded when it left rather than when the
    /// agent got round to it. A queued prompt reaches the transcript only once
    /// the turn that reads it begins, which can be minutes later.
    Sent {
        text: String,
    },
}

pub struct Event {
    pub anchor: Anchor,
    pub unit: Unit,
}

/// Flattens a content value, which is either a bare string or a block array.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Parses one JSONL line from `thread`. A line holds several content blocks,
/// so it yields several events. An unrecognised line type yields none.
pub fn parse_line(line: &str, thread: &Thread) -> Result<Vec<Event>> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Ok(vec![]),
    };

    let kind = v["type"].as_str().unwrap_or_default();
    if !matches!(kind, "user" | "assistant") {
        return Ok(vec![]);
    }
    // Injected context, not something the user or the model said.
    if v["isMeta"].as_bool().unwrap_or(false) {
        return Ok(vec![]);
    }

    let at = v["timestamp"].as_str().unwrap_or_default().to_string();
    let uuid = v["uuid"].as_str().unwrap_or_default().to_string();
    let sidecar = v["toolUseResult"]["persistedOutputPath"]
        .as_str()
        .or_else(|| v["toolUseResult"]["outputFile"].as_str())
        .map(str::to_string);

    let anchor = |unit| Event {
        anchor: Anchor {
            at: at.clone(),
            uuid: uuid.clone(),
            thread: thread.clone(),
        },
        unit,
    };

    let content = &v["message"]["content"];
    let mut events = Vec::new();

    if let Value::String(s) = content {
        if kind == "user" {
            events.push(anchor(Unit::Prompt { text: s.clone() }));
        } else {
            events.push(anchor(Unit::Text { text: s.clone() }));
        }
        return Ok(events);
    }

    let Some(blocks) = content.as_array() else {
        return Ok(events);
    };

    for block in blocks {
        let unit = match block["type"].as_str().unwrap_or_default() {
            "text" => {
                let text = block["text"].as_str().unwrap_or_default().to_string();
                if kind == "user" {
                    Unit::Prompt { text }
                } else {
                    Unit::Text { text }
                }
            }
            "thinking" => Unit::Thinking {
                text: block["thinking"].as_str().unwrap_or_default().to_string(),
            },
            "tool_use" => Unit::ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: block["name"].as_str().unwrap_or_default().to_string(),
                input: block["input"].clone(),
            },
            "tool_result" => {
                let body = text_of(&block["content"]);
                let status = if block["is_error"].as_bool().unwrap_or(false) {
                    Status::Error
                } else {
                    Status::Ok
                };
                Unit::ToolResult {
                    for_id: block["tool_use_id"].as_str().unwrap_or_default().to_string(),
                    status,
                    lines: body.lines().count(),
                    body,
                    sidecar: sidecar.clone(),
                }
            }
            other => Unit::Meta {
                kind: MetaKind::UnknownBlock,
                fields: vec![("type".into(), other.to_string())],
            },
        };
        // Redacted thinking and empty text blocks carry nothing to show.
        if let Unit::Text { text } | Unit::Thinking { text } | Unit::Prompt { text } = &unit {
            if text.trim().is_empty() {
                continue;
            }
        }
        events.push(anchor(unit));
    }

    Ok(events)
}

/// Reads the sidecar a truncated tool result points at, falling back to the
/// inline body when the file is gone. The envelope hint is the only field read
/// outside the allowlist.
pub fn read_sidecar(session: &Session, hint: Option<&str>, inline: &str) -> String {
    let Some(hint) = hint else {
        return inline.to_string();
    };
    let path = Path::new(hint);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        session.dir.join(path)
    };
    fs::read_to_string(path).unwrap_or_else(|_| inline.to_string())
}

/// One line of the outbox log. Yields nothing for a line that is not one.
pub fn parse_sent(line: &str) -> Result<Option<Event>> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    let Some(text) = v["text"].as_str() else {
        return Ok(None);
    };
    Ok(Some(Event {
        anchor: Anchor {
            at: v["at"].as_str().unwrap_or_default().to_string(),
            uuid: String::new(),
            thread: Thread::Main,
        },
        unit: Unit::Sent {
            text: text.to_string(),
        },
    }))
}

pub struct RenderOpts {
    pub max_result_lines: usize,
}

/// The document header. Emitted once per output stream.
pub fn frontmatter(session: &Session, model: Option<&str>) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("session: {}\n", session.id));
    out.push_str(&format!("cwd: {}\n", session.cwd.display()));
    if let Some(model) = model {
        out.push_str(&format!("model: {model}\n"));
    }
    out.push_str("---\n\n");
    out
}

/// Backticks needed to fence `body` so that nothing in it can close the fence.
pub fn fence_len(body: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    (longest + 1).max(3)
}

fn fenced(info: &str, body: &str) -> String {
    let fence = "`".repeat(fence_len(body));
    let body = body.trim_end_matches('\n');
    format!("{fence}{info}\n{body}\n{fence}\n\n")
}

fn blockquote(text: &str) -> String {
    let quoted: Vec<String> = text
        .lines()
        .map(|l| if l.is_empty() { ">".into() } else { format!("> {l}") })
        .collect();
    format!("{}\n\n", quoted.join("\n"))
}

fn truncate(body: &str, max: usize) -> String {
    let total = body.lines().count();
    if total <= max {
        return body.to_string();
    }
    let kept: Vec<&str> = body.lines().take(max).collect();
    format!("{}\n... {} more lines\n", kept.join("\n"), total - max)
}

/// One unit as Markdown, anchor comment included, trailing blank line included.
pub fn render(event: &Event, opts: &RenderOpts) -> String {
    let kind = match &event.unit {
        Unit::Prompt { .. } => "prompt",
        Unit::Text { .. } => "text",
        Unit::Thinking { .. } => "thinking",
        Unit::ToolCall { .. } => "tool",
        Unit::ToolResult { .. } => "result",
        Unit::Meta { .. } => "meta",
        Unit::Sent { .. } => "sent",
    };
    let head = format!(
        "<!-- claude at={} uuid={} thread={} kind={} -->\n",
        event.anchor.at,
        event.anchor.uuid,
        event.anchor.thread.label(),
        kind
    );

    let body = match &event.unit {
        Unit::Prompt { text } => blockquote(text),
        Unit::Text { text } => format!("{}\n\n", text.trim_end_matches('\n')),
        Unit::Thinking { text } => fenced("claude:thinking", text),
        Unit::ToolCall { id, name, input } => {
            let json = serde_json::to_string_pretty(input).unwrap_or_default();
            fenced(&format!("claude:tool id={id} name={name}"), &json)
        }
        Unit::ToolResult {
            for_id,
            status,
            lines,
            body,
            ..
        } => fenced(
            &format!(
                "claude:result for={for_id} status={} lines={lines}",
                status.label()
            ),
            &truncate(body, opts.max_result_lines),
        ),
        Unit::Sent { text } => fenced("claude:sent", text),
        Unit::Meta { kind, fields } => {
            let body: String = fields
                .iter()
                .map(|(k, v)| format!("{k}: {v}\n"))
                .collect();
            fenced(&format!("claude:meta kind={}", kind.label()), &body)
        }
    };

    format!("{head}{body}")
}

/// One file being tailed, and where reading stopped.
pub struct Tailed {
    thread: Thread,
    path: PathBuf,
    offset: u64,
}

impl Tailed {
    /// Starts at the end, so an existing outbox log is not replayed.
    pub fn following(path: PathBuf) -> Self {
        let offset = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Tailed {
            thread: Thread::Main,
            path,
            offset,
        }
    }

    /// Complete lines appended since the last call. A partial trailing line is
    /// left for the next one.
    pub fn drain(&mut self) -> Result<Vec<String>> {
        let mut file = match fs::File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Ok(vec![]),
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            // Truncated underneath us; start over rather than emit garbage.
            self.offset = 0;
        }
        if len == self.offset {
            return Ok(vec![]);
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = String::new();
        file.take(len - self.offset).read_to_string(&mut buf)?;
        let Some(last) = buf.rfind('\n') else {
            return Ok(vec![]);
        };
        self.offset += (last + 1) as u64;
        Ok(buf[..=last]
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect())
    }
}

/// Follows the main transcript and, unless disabled, every subagent file that
/// appears beside it. Blocks until a line is available; without `follow` it
/// drains what exists and then ends.
pub struct Follower {
    follow: bool,
    sidechains: bool,
    subagents: PathBuf,
    files: Vec<Tailed>,
    queue: VecDeque<(Thread, String)>,
    rx: Option<Receiver<notify::Result<notify::Event>>>,
    _watcher: Option<RecommendedWatcher>,
}

impl Follower {
    pub fn new(session: &Session, sidechains: bool, follow: bool) -> Result<Self> {
        let files = vec![Tailed {
            thread: Thread::Main,
            path: session.transcript.clone(),
            offset: 0,
        }];

        let (rx, watcher) = if follow {
            let (tx, rx) = channel();
            let mut watcher = notify::recommended_watcher(tx)?;
            if let Some(parent) = session.transcript.parent() {
                watcher.watch(parent, RecursiveMode::Recursive).ok();
            }
            (Some(rx), Some(watcher))
        } else {
            (None, None)
        };

        Ok(Follower {
            follow,
            sidechains,
            subagents: session.dir.join("subagents"),
            files,
            queue: VecDeque::new(),
            rx,
            _watcher: watcher,
        })
    }

    /// Picks up subagent files that appeared since the last scan.
    fn adopt_subagents(&mut self) {
        if !self.sidechains {
            return;
        }
        let Ok(entries) = fs::read_dir(&self.subagents) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if self.files.iter().any(|f| f.path == path) {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("subagent")
                .to_string();
            self.files.push(Tailed {
                thread: Thread::Sidechain(id),
                path,
                offset: 0,
            });
        }
    }

    fn fill(&mut self) -> Result<()> {
        self.adopt_subagents();
        for file in &mut self.files {
            for line in file.drain()? {
                self.queue.push_back((file.thread.clone(), line));
            }
        }
        Ok(())
    }

    /// `None` once the session is gone and no more lines can arrive.
    pub fn next_line(&mut self) -> Result<Option<(Thread, String)>> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Ok(Some(item));
            }
            self.fill()?;
            if !self.queue.is_empty() {
                continue;
            }
            if !self.follow {
                return Ok(None);
            }
            // A notification only says something moved; the offsets decide what
            // is new. The timeout also covers files no watch reached yet.
            if let Some(rx) = &self.rx {
                let _ = rx.recv_timeout(Duration::from_millis(200));
            }
        }
    }
}
