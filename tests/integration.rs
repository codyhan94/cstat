// TODO: wire up `tests/fixtures/statusline_sample.json` (and add cold-cache /
// warm-cache / cold-miss variants) so the integration tests run against the
// real Claude Code stdin shape rather than the hand-crafted JSON literals
// below. The fixtures live there now; wiring is deferred.

use std::path::{Path, PathBuf};
use std::process::Command;

fn state_dir() -> &'static str {
    if Path::new("/Volumes/ramdisk").is_dir() {
        "/Volumes/ramdisk"
    } else {
        "/tmp"
    }
}

fn cleanup_state(session_id: &str) {
    let path = PathBuf::from(format!("{}/cstat-{}.bin", state_dir(), session_id));
    let _ = std::fs::remove_file(path);
}

fn run_with_stdin(input: &str) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_cstat");
    Command::new(bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("HOME", "/nonexistent")
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(input.as_bytes()).ok();
            }
            child.wait_with_output()
        })
        .unwrap()
}

#[test]
fn empty_stdin_exits_0() {
    let out = run_with_stdin("");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cstat"));
    assert!(stdout.contains("no data"));
}

#[test]
fn invalid_json_stdin_exits_0() {
    let out = run_with_stdin("not json at all");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cstat"));
    assert!(stdout.contains("no data"));
}

#[test]
fn minimal_json_exits_0() {
    let out = run_with_stdin("{}");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cstat"));
    assert!(stdout.contains("no data"));
}

#[test]
fn partial_json_exits_0() {
    let input = r#"{"model": {"display_name": "Opus"}, "cwd": "/tmp/test"}"#;
    let out = run_with_stdin(input);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Opus"));
    assert!(stdout.contains("test"));
}

#[test]
fn missing_transcript_exits_0() {
    let input = r#"{"model": {"display_name": "X"}, "cwd": "/tmp/p", "transcript_path": "/nonexistent/transcript.jsonl"}"#;
    let out = run_with_stdin(input);
    assert!(out.status.success());
}

#[test]
fn stdout_never_contains_error_messages() {
    let out = run_with_stdin("");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("error"));
    assert!(!stdout.contains("panic"));
    assert!(!stdout.contains("Error"));
}

#[test]
fn stdout_ends_with_newline() {
    let out = run_with_stdin("");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.ends_with('\n'));
}

#[test]
fn single_line_output() {
    let input = r#"{"model": {"display_name": "Opus"}, "cwd": "/tmp/proj"}"#;
    let out = run_with_stdin(input);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("Opus"));
    assert!(lines[0].contains("proj"));
}

fn unique_sid(label: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("test-{label}-{}-{now}", std::process::id())
}

#[test]
fn cold_cache_label_present() {
    // No cache token activity yet → "⧖ cold" should appear.
    let sid = unique_sid("cold");
    let input = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}"}}"#
    );
    let out = run_with_stdin(&input);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("⧖ cold") || stdout.contains("cold"));
    cleanup_state(&sid);
}

#[test]
fn cache_warm_after_warm_hit() {
    // Warm hit (cache_read > 0): hourglass glyph, no cost indicator.
    // Needs a priming invocation first: fresh state has no token baseline,
    // so cache fields in stdin are ignored (they may be stale from a prior
    // session). The second invocation with different tokens triggers a real stamp.
    let sid = unique_sid("warm");
    let prime = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}","context_window":{{"total_input_tokens":5000}}}}"#
    );
    let _ = run_with_stdin(&prime);
    let input = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}","context_window":{{"total_input_tokens":10000,"current_usage":{{"cache_read_input_tokens":5000,"cache_creation_input_tokens":1000}}}}}}"#
    );
    let out = run_with_stdin(&input);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("⧖"), "expected ⧖ in output, got: {stdout}");
    assert!(!stdout.contains(" $ "), "should not show cost glyph on plain warm hit");
    assert!(!stdout.contains("cold"), "should be warm, not cold");
    cleanup_state(&sid);
}

#[test]
fn cache_cold_miss_swaps_glyph() {
    // Cold miss within the last 30s → hourglass swaps to $ (same width, no shift).
    // Needs a priming invocation first to establish a token baseline.
    let sid = unique_sid("miss");
    let prime = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}","context_window":{{"total_input_tokens":5000}}}}"#
    );
    let _ = run_with_stdin(&prime);
    let input = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}","context_window":{{"total_input_tokens":10000,"current_usage":{{"cache_read_input_tokens":0,"cache_creation_input_tokens":5000}}}}}}"#
    );
    let out = run_with_stdin(&input);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains('$'), "expected $ glyph after cold miss, got: {stdout}");
    assert!(!stdout.contains("⧖"), "hourglass should be swapped out during the cold-miss window");
    cleanup_state(&sid);
}

#[test]
fn cache_stamp_persists_across_invocations() {
    // First call: warm hit, stamp written. Second call: same total_input_tokens (no new turn),
    // current_usage is empty → state machine short-circuits, but stamp survives → still ⧖.
    // Needs a priming invocation first to establish a token baseline.
    let sid = unique_sid("persist");
    let prime = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}","context_window":{{"total_input_tokens":5000}}}}"#
    );
    let _ = run_with_stdin(&prime);

    let input1 = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}","context_window":{{"total_input_tokens":10000,"current_usage":{{"cache_read_input_tokens":5000,"cache_creation_input_tokens":0}}}}}}"#
    );
    let out1 = run_with_stdin(&input1);
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(stdout1.contains("⧖"), "expected warm cache timer after warm hit, got: {stdout1}");
    assert!(!stdout1.contains("cold"), "expected warm, not cold, got: {stdout1}");

    // Second call: same tokens, no current_usage → short-circuits, but stamp persists.
    let input2 = format!(
        r#"{{"model":{{"display_name":"Opus"}},"cwd":"/tmp/proj","session_id":"{sid}","context_window":{{"total_input_tokens":10000}}}}"#
    );
    let out2 = run_with_stdin(&input2);
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("⧖"), "warm stamp should persist, got: {stdout2}");
    assert!(!stdout2.contains("cold"), "warm stamp should persist, got: {stdout2}");
    cleanup_state(&sid);
}
