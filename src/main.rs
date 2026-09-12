use std::collections::HashSet;
use std::io::Write;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use claude_md_stream::{
    frontmatter, parse_line, parse_sent, poll, read_sidecar, render, resolve, sent_log, Follower,
    MetaKind, RenderOpts, Session, Tailed, Target, Thread, Unit,
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

/// UTC in the shape the transcript uses, so `at=` means one thing throughout.
/// Civil date from a day count, after Howard Hinnant's algorithm.
fn now_iso() -> String {
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

fn meta(session: &Session, kind: MetaKind, fields: Vec<(String, String)>, opts: &RenderOpts) {
    let event = claude_md_stream::Event {
        anchor: claude_md_stream::Anchor {
            at: claude_md_stream::now_iso(),
            uuid: session.id.clone(),
            thread: Thread::Main,
        },
        unit: Unit::Meta { kind, fields },
    };
    print!("{}", render(&event, opts));
}

/// Where `claude-send` writes what it handed to an agent. Convention rather
/// than a flag: the sender derives the same path from the same handle.
fn outbox(handle: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(sent_log(
        &std::path::PathBuf::from(home)
            .join(".local/state/claude-md-stream")
            .join(handle),
    ))
}

/// Prints what has been sent since the last look.
fn drain_sent(tail: &mut Option<Tailed>, opts: &RenderOpts) -> Result<()> {
    let Some(tail) = tail else { return Ok(()) };
    for line in tail.drain()? {
        if let Some(event) = parse_sent(&line)? {
            print!("{}", render(&event, opts));
        }
    }
    Ok(())
}

/// How often the agent list is asked what the pane is doing. The stream itself
/// carries no such signal, so this is wall clock rather than line-driven.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// Emits a status unit when the observed state changed. `observed` is how long
/// herdr reported the previous state, which is not the agent's own accounting
/// of how long it worked.
fn report(
    session: &Session,
    polled: &claude_md_stream::Poll,
    state: &mut Option<String>,
    since: &mut std::time::Instant,
    opts: &RenderOpts,
) {
    if polled.state == *state {
        return;
    }
    if let Some(now) = &polled.state {
        let mut fields = vec![("state".into(), now.clone())];
        if state.is_some() {
            fields.push(("observed".into(), format!("{}s", since.elapsed().as_secs())));
        }
        meta(session, MetaKind::Status, fields, opts);
    }
    *state = polled.state.clone();
    *since = std::time::Instant::now();
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
        let mut state: Option<String> = None;
        let mut since = std::time::Instant::now();
        let mut polled_at = std::time::Instant::now();
        // Starts at the end of the log: what was sent before this viewer
        // existed has long since reached the transcript.
        let mut sent = outbox(&args.target).map(Tailed::following);
        let switched = loop {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok((thread, line)) => {
                    drain_sent(&mut sent, &opts)?;
                    if polled_at.elapsed() >= POLL_EVERY {
                        let p = poll(&target, &session)?;
                        report(&session, &p, &mut state, &mut since, &opts);
                        polled_at = std::time::Instant::now();
                        if let Some(next) = p.switched {
                            break Some(next);
                        }
                    }
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
                    drain_sent(&mut sent, &opts)?;
                    let p = poll(&target, &session)?;
                    report(&session, &p, &mut state, &mut since, &opts);
                    polled_at = std::time::Instant::now();
                    if let Some(next) = p.switched {
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
