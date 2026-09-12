//! Sends an outbox buffer to the herdr agent named by its directory.
//!
//! The text comes from the file, or from stdin with `--stdin`. An editor whose
//! save is asynchronous races a reader of the file, so piping the buffer in is
//! the reliable path; the argument is then only there to name the agent.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{anyhow, Context, Result};

const USAGE: &str = "usage: claude-send [--stdin] <path-to-buffer>";

/// `~/.local/state/claude-md-stream/<handle>/input.md` names its own target.
/// The path is resolved first: an editor passes the buffer name relative to its
/// own working directory, which on its own has no parent to read a handle from.
fn handle_of(path: &Path) -> Result<String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("cannot tell which agent {path:?} belongs to"))
}

fn buffer_path(args: &[String]) -> Result<PathBuf> {
    if let Some(arg) = args.iter().find(|a| *a != "--stdin") {
        return Ok(PathBuf::from(arg));
    }
    std::env::var("CLAUDE_SEND_TARGET")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("{USAGE}"))
}

fn run() -> Result<u8> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = buffer_path(&args)?;
    let handle = handle_of(&path)?;

    let text = if args.iter().any(|a| a == "--stdin") {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        std::fs::read_to_string(&path).with_context(|| format!("reading {path:?}"))?
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
