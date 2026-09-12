use claude_md_stream::{parse_line, render, RenderOpts, Thread};

/// Guards the emitted format against silent drift. Regenerate deliberately:
/// `cargo test -- --ignored regenerate_golden`.
fn rendered() -> String {
    let fixture = include_str!("fixtures/session.jsonl");
    let opts = RenderOpts {
        max_result_lines: 40,
    };
    fixture
        .lines()
        .flat_map(|l| parse_line(l, &Thread::Main).unwrap())
        .map(|e| render(&e, &opts))
        .collect()
}

#[test]
fn the_fixture_renders_to_the_golden_file() {
    let golden = include_str!("fixtures/session.md");
    assert_eq!(rendered(), golden);
}

#[test]
#[ignore = "writes the golden file"]
fn regenerate_golden() {
    std::fs::write(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session.md"),
        rendered(),
    )
    .unwrap();
}
