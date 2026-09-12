use std::collections::HashSet;
use std::io::Write;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use claude_md_stream::{
    frontmatter, parse_line, read_sidecar, render, resolve, resolve_switch, Follower, MetaKind,
    RenderOpts, Session, Target, Thread, Unit,
};

const USAGE: &str = "usage: claude-md-stream tail <agent|session-id> \
[--no-follow] [--no-sidechains] [--max-result-lines N]";

struct Args {
    target: String,
    follow: bool,
    sidechains: bool,
    max_result_lines: usize,
}

fn parse_args() -> Result<Args> {
    let mut argv = std::env::args().skip(1);
    if argv.next().as_deref() != Some("tail") {
        return Err(anyhow!("{USAGE}"));
    }
    let mut args = Args {
        target: String::new(),
        follow: true,
        sidechains: true,
        max_result_lines: 40,
    };
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--no-follow" => args.follow = false,
            "--no-sidechains" => args.sidechains = false,
            "--max-result-lines" => {
                args.max_result_lines = argv
                    .next()
                    .ok_or_else(|| anyhow!("--max-result-lines takes a number"))?
                    .parse()?
            }
            other if other.starts_with('-') => return Err(anyhow!("unknown flag {other}")),
            other => args.target = other.to_string(),
        }
    }
    if args.target.is_empty() {
        return Err(anyhow!("{USAGE}"));
    }
    Ok(args)
}

/// The model a session runs, taken from the first assistant message that names
/// one. Absent for a session that has not answered yet.
fn model_of(session: &Session) -> Option<String> {
    let text = std::fs::read_to_string(&session.transcript).ok()?;
    text.lines().take(400).find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        v["message"]["model"].as_str().map(str::to_string)
    })
}

/// Emits one line's units. Kept here rather than in the library so the library
/// never decides what an output stream looks like.
fn emit(session: &Session, thread: &Thread, line: &str, opts: &RenderOpts) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for mut event in parse_line(line, thread)? {
        if let Unit::ToolResult {
            body,
            sidecar,
            lines,
            ..
        } = &mut event.unit
        {
            if sidecar.is_some() {
                *body = read_sidecar(session, sidecar.as_deref(), body);
                *lines = body.lines().count();
            }
        }
        write!(out, "{}", render(&event, opts))?;
    }
    out.flush()?;
    Ok(())
}

fn meta(session: &Session, kind: MetaKind, fields: Vec<(String, String)>, opts: &RenderOpts) {
    let event = claude_md_stream::Event {
        anchor: claude_md_stream::Anchor {
            at: String::new(),
            uuid: session.id.clone(),
            thread: Thread::Main,
        },
        unit: Unit::Meta { kind, fields },
    };
    print!("{}", render(&event, opts));
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let target = Target::parse(&args.target);
    let opts = RenderOpts {
        max_result_lines: args.max_result_lines,
    };

    let mut session = resolve(&target)?;
    print!("{}", frontmatter(&session, model_of(&session).as_deref()));

    loop {
        let (tx, rx) = channel();
        let mut follower = Follower::new(&session, args.sidechains, args.follow)?;
        let reader = thread::spawn(move || {
            while let Ok(Some(item)) = follower.next_line() {
                if tx.send(item).is_err() {
                    return;
                }
            }
        });

        // The reader blocks on its own file; the timeout is what lets a session
        // that moved to another id be noticed at all.
        let mut seen: HashSet<String> = HashSet::new();
        let switched = loop {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok((thread, line)) => {
                    // A subagent announces itself by its first line: nothing in
                    // the main thread mentions it until it has finished.
                    if let Thread::Sidechain(id) = &thread {
                        if seen.insert(id.clone()) {
                            meta(
                                &session,
                                MetaKind::SidechainStart,
                                vec![("id".into(), id.clone())],
                                &opts,
                            );
                        }
                    }
                    emit(&session, &thread, &line, &opts)?
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(next) = resolve_switch(&target, &session)? {
                        break Some(next);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break None,
            }
        };

        drop(rx);
        let _ = reader.join();

        match switched {
            Some(next) => {
                session = next;
                meta(
                    &session,
                    MetaKind::SessionSwitch,
                    vec![("session".into(), session.id.clone())],
                    &opts,
                );
            }
            None => return Ok(()),
        }
    }
}
