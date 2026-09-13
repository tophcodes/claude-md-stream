//! Queues text for a herdr agent, and takes it back when asked.
//!
//! The queue is local. Claude Code has one of its own, but an interrupt flushes
//! that one rather than discarding it, so text handed over is text that will be
//! delivered. Holding it here instead is what makes cancelling possible at all.
//!
//! Delivery happens whenever someone looks: this command flushes what it can
//! after queueing, and a running `claude-md-stream tail` flushes on its poll.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{anyhow, Context, Result};
use claude_md_stream::{deliver_one, queue};

const USAGE: &str =
    "usage: claude-send [--to <agent> | [--stdin] <path>] [--cancel] [--recall] [--flush]";

enum Action {
    /// Read text and put it at the back of the queue.
    Send { stdin: bool, path: Option<PathBuf> },
    /// Interrupt the running turn and put everything waiting back in the buffer.
    Cancel,
    /// Take the newest waiting message back into the buffer, to edit it.
    Recall,
    /// Hand over the next waiting message if the agent is free. For a queue
    /// with no viewer watching it.
    Flush,
}

struct Args {
    handle: String,
    outbox: PathBuf,
    action: Action,
}

fn state_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is unset")?;
    Ok(PathBuf::from(home).join(".local/state/claude-md-stream"))
}

/// The agent a path belongs to is the directory it sits in. The path is
/// resolved first: an editor passes a buffer name relative to its own working
/// directory, which on its own has no parent to read a handle from.
fn from_path(path: &Path) -> Result<(String, PathBuf)> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("cannot tell which agent {path:?} belongs to"))?;
    let handle = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("cannot tell which agent {path:?} belongs to"))?;
    Ok((handle.to_string(), dir.to_path_buf()))
}

fn parse_args() -> Result<Args> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| argv.iter().any(|a| a == name);

    let named = argv
        .iter()
        .position(|a| a == "--to")
        .map(|i| {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| anyhow!("--to takes an agent name"))
        })
        .transpose()?;

    let path = argv
        .iter()
        .find(|a| !a.starts_with("--") && Some(*a) != named.as_ref())
        .map(PathBuf::from)
        .or_else(|| std::env::var("CLAUDE_SEND_TARGET").ok().map(PathBuf::from));

    let (handle, outbox) = match (&named, &path) {
        (Some(handle), _) => (handle.clone(), state_dir()?.join(handle)),
        (None, Some(path)) => from_path(path)?,
        (None, None) => return Err(anyhow!("{USAGE}")),
    };

    let action = if flag("--cancel") {
        Action::Cancel
    } else if flag("--recall") {
        Action::Recall
    } else if flag("--flush") {
        Action::Flush
    } else {
        Action::Send {
            stdin: flag("--stdin") || named.is_some(),
            path,
        }
    };

    Ok(Args {
        handle,
        outbox,
        action,
    })
}

/// Hands recovered text back to the caller on stdout, ahead of whatever the
/// caller passed in, because what comes back was written before what is sitting
/// in the buffer now. An editor pipes its buffer through this and replaces it
/// with the result, so nothing is written to disk and nothing typed is lost.
///
/// The same text is also appended to `recovered.md`, because a caller that
/// discards stdout would otherwise drop it on the floor.
fn hand_back(outbox: &Path, texts: &[String]) -> Result<()> {
    let mut carried = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_string(&mut carried)?;
    }

    if !texts.is_empty() {
        let _ = std::fs::create_dir_all(outbox);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(outbox.join("recovered.md"))
        {
            let _ = writeln!(file, "{}", texts.join("\n"));
        }
    }

    let mut out = texts.join("\n");
    if !carried.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&carried);
    }
    print!("{out}");
    Ok(())
}

fn read_text(stdin: bool, path: Option<&PathBuf>) -> Result<String> {
    if stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        return Ok(text);
    }
    let path = path.ok_or_else(|| anyhow!("{USAGE}"))?;
    std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))
}

/// Stops the turn the agent is in the middle of. The queue it keeps is not
/// touched by this, which is why ours holds the waiting text instead.
fn interrupt(handle: &str) -> Result<()> {
    let out = Command::new("herdr")
        .args(["pane", "send-keys", handle, "esc"])
        .output()
        .context("running `herdr pane send-keys`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "herdr refused the interrupt: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn run() -> Result<u8> {
    let args = parse_args()?;

    match args.action {
        Action::Cancel => {
            interrupt(&args.handle)?;
            let waiting = queue::drain(&args.outbox)?;
            eprintln!(
                "interrupted {}, {} message(s) handed back",
                args.handle,
                waiting.len()
            );
            hand_back(&args.outbox, &waiting)?;
        }
        Action::Recall => {
            let waiting: Vec<String> = queue::pop(&args.outbox)?.into_iter().collect();
            if waiting.is_empty() {
                eprintln!("nothing waiting to recall");
            }
            hand_back(&args.outbox, &waiting)?;
        }
        Action::Flush => match deliver_one(&args.outbox, &args.handle)? {
            Some(sent) => println!("sent {} bytes to {}", sent.len(), args.handle),
            None => eprintln!("nothing delivered: queue empty or {} is busy", args.handle),
        },
        Action::Send { stdin, path } => {
            let text = read_text(stdin, path.as_ref())?;
            if text.trim().is_empty() {
                eprintln!("nothing to send");
                return Ok(0);
            }
            queue::push(&args.outbox, &text)?;
            match deliver_one(&args.outbox, &args.handle)? {
                Some(sent) => println!("sent {} bytes to {}", sent.len(), args.handle),
                None => println!("queued for {}, waiting for it to be free", args.handle),
            }
        }
    }

    Ok(0)
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
