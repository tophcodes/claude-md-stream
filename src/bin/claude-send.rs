//! Sends text to a herdr agent, and records what it sent.
//!
//! Sending is one shot: the text goes to `herdr agent prompt`, which types it
//! into the agent's terminal. Nothing watches the buffer file, which is only
//! where an editor happens to keep the text until it is sent.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{anyhow, Context, Result};

const USAGE: &str = "usage: claude-send --to <agent> | [--stdin] <path-to-buffer>";

/// Where the text comes from and which agent it is for.
enum Source {
    /// The agent is named outright, the text arrives on stdin. For a caller
    /// with no file at all: a text field, a script, another program.
    Named(String),
    /// `~/.local/state/claude-md-stream/<agent>/input.md` names its own target.
    /// The text is read from that file, or from stdin when the caller pipes it
    /// because its save has not landed yet.
    Buffer { path: PathBuf, stdin: bool },
}

fn parse_args() -> Result<Source> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Some(i) = args.iter().position(|a| a == "--to") {
        let handle = args
            .get(i + 1)
            .ok_or_else(|| anyhow!("--to takes an agent name"))?;
        return Ok(Source::Named(handle.clone()));
    }

    let stdin = args.iter().any(|a| a == "--stdin");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .or_else(|| std::env::var("CLAUDE_SEND_TARGET").ok().map(PathBuf::from))
        .ok_or_else(|| anyhow!("{USAGE}"))?;
    Ok(Source::Buffer { path, stdin })
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(text)
}

/// The agent this text is for, and the directory its log belongs in. A path is
/// resolved first: an editor passes the buffer name relative to its own working
/// directory, which on its own has no parent to read a handle from.
fn target(source: &Source) -> Result<(String, PathBuf)> {
    match source {
        Source::Named(handle) => Ok((handle.clone(), state_dir()?.join(handle))),
        Source::Buffer { path, .. } => {
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            let dir = path
                .parent()
                .ok_or_else(|| anyhow!("cannot tell which agent {path:?} belongs to"))?;
            let handle = dir
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow!("cannot tell which agent {path:?} belongs to"))?;
            Ok((handle.to_string(), dir.to_path_buf()))
        }
    }
}

fn state_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is unset")?;
    Ok(PathBuf::from(home).join(".local/state/claude-md-stream"))
}

/// Appends the sent text to the outbox log. A viewer tails that log to show the
/// prompt when it left, rather than when the agent finally reads it. Failing to
/// record is not worth failing a delivered message over.
fn record(dir: &Path, text: &str) {
    let _ = std::fs::create_dir_all(dir);
    let entry = serde_json::json!({ "at": claude_md_stream::now_iso(), "text": text });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(claude_md_stream::sent_log(dir))
    {
        use std::io::Write;
        let _ = writeln!(file, "{entry}");
    }
}

fn run() -> Result<u8> {
    let source = parse_args()?;
    let (handle, dir) = target(&source)?;

    let text = match &source {
        Source::Named(_) => read_stdin()?,
        Source::Buffer { stdin: true, .. } => read_stdin()?,
        Source::Buffer { path, .. } => {
            std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?
        }
    };

    if text.trim().is_empty() {
        eprintln!("nothing to send");
        return Ok(0);
    }

    let out = Command::new("herdr")
        .args(["agent", "prompt", &handle, &text])
        .output()
        .context("running `herdr agent prompt`")?;

    if out.status.success() {
        record(&dir, &text);
        println!("sent {} bytes to {handle}", text.len());
        return Ok(0);
    }

    // The buffer is deliberately left alone: a rejected prompt is text the user
    // still has to send somewhere.
    let reason = String::from_utf8_lossy(&out.stderr);
    let reason = reason.trim();
    if reason.contains("agent_blocked") {
        eprintln!("agent {handle} is blocked, buffer kept");
        return Ok(3);
    }
    eprintln!("herdr refused the prompt: {reason}");
    Ok(1)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}
