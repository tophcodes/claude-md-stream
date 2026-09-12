use claude_md_stream::{fence_len, parse_line, render, Anchor, Event, RenderOpts, Thread, Unit};

fn opts() -> RenderOpts {
    RenderOpts {
        max_result_lines: 40,
    }
}

fn render_one(line: &str) -> String {
    parse_line(line, &Thread::Main)
        .unwrap()
        .iter()
        .map(|e| render(e, &opts()))
        .collect()
}

#[test]
fn fence_grows_past_any_run_in_the_body() {
    assert_eq!(fence_len("plain"), 3);
    assert_eq!(fence_len("```"), 4);
    assert_eq!(fence_len("a\n`````\nb"), 6);
}

#[test]
fn a_body_full_of_backticks_cannot_close_its_own_fence() {
    let body = "outer\n`````\nstill inside\n";
    let event = Event {
        anchor: Anchor {
            at: "t".into(),
            uuid: "u".into(),
            thread: Thread::Main,
        },
        unit: Unit::Thinking { text: body.into() },
    };
    let out = render(&event, &opts());
    let fence = "`".repeat(6);
    assert_eq!(out.matches(&fence).count(), 2, "exactly one fence pair");
    assert!(out.contains("still inside"));
}

#[test]
fn an_unknown_line_type_yields_nothing() {
    let line = r#"{"type":"invented-later","uuid":"u","timestamp":"t"}"#;
    assert!(parse_line(line, &Thread::Main).unwrap().is_empty());
}

#[test]
fn an_unknown_content_block_stays_visible_as_meta() {
    let line = r#"{"type":"assistant","uuid":"u","timestamp":"t",
      "message":{"content":[{"type":"hologram","data":1}]}}"#;
    let out = render_one(line);
    assert!(out.contains("claude:meta kind=unknown-block"));
    assert!(out.contains("type: hologram"));
}

#[test]
fn a_prompt_is_blockquoted_line_by_line() {
    let line = r#"{"type":"user","uuid":"u","timestamp":"t",
      "message":{"content":"one\n\ntwo"}}"#;
    let out = render_one(line);
    assert!(out.contains("> one"));
    assert!(out.contains("\n>\n"));
    assert!(out.contains("> two"));
}

#[test]
fn injected_context_is_not_a_prompt() {
    let line = r#"{"type":"user","isMeta":true,"uuid":"u","timestamp":"t",
      "message":{"content":"<system-reminder>noise</system-reminder>"}}"#;
    assert!(parse_line(line, &Thread::Main).unwrap().is_empty());
}

#[test]
fn a_failed_tool_result_says_so_in_the_info_string() {
    let line = r#"{"type":"user","uuid":"u","timestamp":"t","message":{"content":[
      {"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}]}}"#;
    let out = render_one(line);
    assert!(out.contains("claude:result for=t1 status=error lines=1"));
}

#[test]
fn a_long_result_is_truncated_but_reports_its_length() {
    let body: String = (0..100).map(|i| format!("line {i}\\n")).collect();
    let line = format!(
        r#"{{"type":"user","uuid":"u","timestamp":"t","message":{{"content":[
        {{"type":"tool_result","tool_use_id":"t1","content":"{body}"}}]}}}}"#
    );
    let out = render_one(&line);
    assert!(out.contains("lines=100"));
    assert!(out.contains("... 60 more lines"));
}

#[test]
fn the_thread_rides_on_every_anchor() {
    let line = r#"{"type":"assistant","uuid":"u","timestamp":"t",
      "message":{"content":[{"type":"text","text":"hi"}]}}"#;
    let thread = Thread::Sidechain("agent-7".into());
    let out: String = parse_line(line, &thread)
        .unwrap()
        .iter()
        .map(|e| render(e, &opts()))
        .collect();
    assert!(out.contains("thread=agent-7"));
}
