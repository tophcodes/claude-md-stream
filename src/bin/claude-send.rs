//! Sends an outbox buffer to the herdr agent named by its directory.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{anyhow, Context, Result};

const USAGE: &str = "usage: claude-send <path-to-buffer>";

/// `~/.local/state/claude-md-stream/<handle>/input.md` names its own target.
fn handle_of(path: &Path) -> Result<String> {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("cannot tell which agent {path:?} belongs to"))
}

fn buffer_path() -> Result<PathBuf> {
    if let Some(arg) = std::env::args().nth(1) {
        return Ok(PathBuf::from(arg));
    }
    std::env::var("CLAUDE_SEND_TARGET")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("{USAGE}"))
}

fn run() -> Result<u8> {
    let path = buffer_path()?;
    let handle = handle_of(&path)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path:?}"))?;

    if text.trim().is_empty() {
        eprintln!("nothing to send");
        return Ok(0);
    }

    let out = Command::new("herdr")
        .args(["agent", "prompt", &handle, &text])
        .output()
        .context("running `herdr agent prompt`")?;

    if out.status.success() {
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
