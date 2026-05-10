use std::fs;
use std::os::unix::fs::MetadataExt;

use memmap2::Mmap;
use serde_json::Value;

use crate::types::{AgentEntry, State, TaskItem, TaskStatus, TodoItem, ToolEntry, TranscriptData};

pub fn parse_transcript(path: Option<&str>, state: &mut State) -> TranscriptData {
    let Some(path) = path else {
        return TranscriptData::default();
    };

    let Ok(file) = fs::File::open(path) else {
        return TranscriptData::default();
    };

    let Ok(meta) = file.metadata() else {
        return TranscriptData::default();
    };

    let inode = meta.ino();
    let file_size = meta.len();

    if file_size == 0 {
        return TranscriptData::default();
    }

    if inode != state.inode || file_size < state.file_size {
        state.byte_offset = 0;
        state.tools.clear();
        state.agents.clear();
        state.todos.clear();
        state.tasks.clear();
        state.next_seq = 0;
    }

    state.inode = inode;
    state.file_size = file_size;

    let Ok(mmap) = (unsafe { Mmap::map(&file) }) else {
        return TranscriptData::default();
    };

    let start = (state.byte_offset as usize).min(mmap.len());
    let slice = &mmap[start..];

    for line in slice.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }

        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if state.parent_session_id.is_none() {
            if let Some(sid) = entry
                .get("forkedFrom")
                .and_then(|f| f.get("sessionId"))
                .and_then(|v| v.as_str())
            {
                state.parent_session_id = Some(sid.to_string());
            }
        }

        match entry_type {
            "assistant" => {
                parse_assistant_message(&entry, state);
            }
            "user" => {
                parse_tool_results(&entry, state);
            }
            _ => {}
        }
    }

    state.byte_offset = mmap.len() as u64;

    TranscriptData {
        tools: state.tools.clone(),
        agents: state.agents.clone(),
        todos: state.todos.clone(),
        tasks: state.tasks.clone(),
    }
}

fn parse_assistant_message(entry: &Value, state: &mut State) {
    let Some(content) = entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return;
    };

    for block in content {
        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if block_type != "tool_use" {
            continue;
        }

        let Some(id) = block.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = block
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let input = block.get("input");

        let target = extract_target(name, input);

        if name == "Agent" || name == "Task" {
            let subagent_type = input
                .and_then(|i| i.get("subagent_type"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let model = input
                .and_then(|i| i.get("model"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let description = input
                .and_then(|i| i.get("description"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let timestamp = entry.get("timestamp").and_then(|v| v.as_str());
            let start_time = timestamp.and_then(|ts| {
                ts.parse::<chrono::DateTime<chrono::Utc>>()
                    .ok()
                    .map(|dt| dt.timestamp())
            });

            let seq = state.next_seq;
            state.next_seq += 1;
            state.agents.insert(
                id.to_string(),
                AgentEntry {
                    subagent_type,
                    model,
                    description,
                    start_time,
                    completed: false,
                    seq,
                },
            );
        } else if name == "TodoWrite" {
            if let Some(todos) = input.and_then(|i| i.get("todos")).and_then(|v| v.as_array()) {
                state.todos = todos
                    .iter()
                    .filter_map(|t| {
                        let content = t.get("content").and_then(|v| v.as_str())?;
                        let status = t
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending");
                        Some(TodoItem {
                            content: content.to_string(),
                            completed: parse_status(status) == TaskStatus::Completed,
                        })
                    })
                    .collect();
            }
        } else if name == "TaskCreate" {
            // TaskCreate's input has no taskId / status — those are assigned by
            // the runtime and surfaced in the tool_result (e.g. "Task #3
            // created successfully: ..."). Insert under the tool_use id as a
            // placeholder; `parse_tool_results` re-keys it to the numeric id
            // once the result lands. TaskUpdate then keys by that numeric id.
            let status = input
                .and_then(|i| i.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            state.tasks.insert(
                id.to_string(),
                TaskItem {
                    status: parse_status(status),
                },
            );
        } else if name == "TaskUpdate" {
            let task_id = input
                .and_then(|i| i.get("taskId"))
                .and_then(|v| v.as_str());
            let status = input
                .and_then(|i| i.get("status"))
                .and_then(|v| v.as_str());
            if let (Some(task_id), Some(status)) = (task_id, status) {
                if let Some(task) = state.tasks.get_mut(task_id) {
                    task.status = parse_status(status);
                }
            }
        } else {
            let seq = state.next_seq;
            state.next_seq += 1;
            state.tools.insert(
                id.to_string(),
                ToolEntry {
                    name: name.to_string(),
                    target,
                    completed: false,
                    error: false,
                    seq,
                },
            );
        }
    }
}

fn parse_tool_results(entry: &Value, state: &mut State) {
    let Some(content) = entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return;
    };

    for block in content {
        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if block_type != "tool_result" {
            continue;
        }

        let Some(tool_use_id) = block.get("tool_use_id").and_then(|v| v.as_str()) else {
            continue;
        };

        let is_error = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);

        if let Some(tool) = state.tools.get_mut(tool_use_id) {
            tool.completed = true;
            tool.error = is_error;
        }

        if let Some(agent) = state.agents.get_mut(tool_use_id) {
            agent.completed = true;
        }

        // Re-key TaskCreate entries from the tool_use id to the numeric id that
        // TaskUpdate will reference. The numeric id is embedded in the result
        // text: "Task #N created successfully: ...". Without this, every
        // TaskUpdate misses the HashMap and the completion never registers.
        if state.tasks.contains_key(tool_use_id) {
            let result_text = tool_result_text(block);
            if let Some(numeric_id) = parse_task_create_id(&result_text) {
                if let Some(task) = state.tasks.remove(tool_use_id) {
                    state.tasks.insert(numeric_id, task);
                }
            }
        }
    }
}

/// Best-effort extraction of the result body as a string. Tool results can be
/// either a bare string or an array of `{type, text}` content blocks.
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Parse the numeric task id out of "Task #N created successfully: ...".
/// Returns the digits as a `String` (matches what TaskUpdate's `taskId` looks like).
fn parse_task_create_id(content: &str) -> Option<String> {
    let after_hash = content.split_once("Task #")?.1;
    let id: String = after_hash.chars().take_while(|c| c.is_ascii_digit()).collect();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn extract_target(name: &str, input: Option<&Value>) -> Option<String> {
    let input = input?;
    let get_str = |key: &str| input.get(key).and_then(|v| v.as_str());
    match name {
        "Read" | "Write" | "Edit" => get_str("file_path").map(short_path),
        "Glob" | "Grep" => get_str("pattern").map(|s| s.to_string()),
        "Bash" => get_str("command").map(|s| truncate(s, 30)),
        _ => None,
    }
}

fn short_path(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

fn parse_status(status: &str) -> TaskStatus {
    match status {
        "completed" | "complete" | "done" => TaskStatus::Completed,
        "in_progress" | "running" => TaskStatus::InProgress,
        _ => TaskStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_tool_use(id: &str, name: &str, input: Value) -> String {
        serde_json::to_string(&serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-03-22T10:00:00.000Z",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input
                }]
            }
        }))
        .unwrap()
    }

    fn make_tool_result(tool_use_id: &str, is_error: bool) -> String {
        make_tool_result_with_content(tool_use_id, is_error, "ok")
    }

    fn make_tool_result_with_content(tool_use_id: &str, is_error: bool, content: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "type": "user",
            "timestamp": "2026-03-22T10:00:01.000Z",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "is_error": is_error,
                    "content": content
                }]
            }
        }))
        .unwrap()
    }

    fn write_transcript(dir: &tempfile::TempDir, lines: &[String]) -> String {
        let path = dir.path().join("transcript.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn missing_path_returns_default() {
        let mut state = State::default();
        let data = parse_transcript(None, &mut state);
        assert!(data.tools.is_empty());
    }

    #[test]
    fn missing_file_returns_default() {
        let mut state = State::default();
        let data = parse_transcript(Some("/nonexistent.jsonl"), &mut state);
        assert!(data.tools.is_empty());
    }

    #[test]
    fn tool_use_creates_running_entry() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![make_tool_use(
            "t1",
            "Read",
            serde_json::json!({"file_path": "/foo/bar.rs"}),
        )];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert_eq!(data.tools.len(), 1);
        let tool = &data.tools["t1"];
        assert_eq!(tool.name, "Read");
        assert_eq!(tool.target.as_deref(), Some("bar.rs"));
        assert!(!tool.completed);
    }

    #[test]
    fn tool_result_marks_completed() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            make_tool_use("t1", "Read", serde_json::json!({"file_path": "/foo/bar.rs"})),
            make_tool_result("t1", false),
        ];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert!(data.tools["t1"].completed);
        assert!(!data.tools["t1"].error);
    }

    #[test]
    fn tool_result_marks_error() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            make_tool_use("t1", "Bash", serde_json::json!({"command": "false"})),
            make_tool_result("t1", true),
        ];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert!(data.tools["t1"].completed);
        assert!(data.tools["t1"].error);
    }

    #[test]
    fn incremental_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");

        {
            let mut f = fs::File::create(&path).unwrap();
            writeln!(
                f,
                "{}",
                make_tool_use("t1", "Read", serde_json::json!({"file_path": "/a.rs"}))
            )
            .unwrap();
        }

        let mut state = State::default();
        let data = parse_transcript(Some(path.to_str().unwrap()), &mut state);
        assert_eq!(data.tools.len(), 1);
        let offset_after_first = state.byte_offset;
        assert!(offset_after_first > 0);

        {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(
                f,
                "{}",
                make_tool_use("t2", "Write", serde_json::json!({"file_path": "/b.rs"}))
            )
            .unwrap();
        }

        let data = parse_transcript(Some(path.to_str().unwrap()), &mut state);
        assert_eq!(data.tools.len(), 2);
        assert!(state.byte_offset > offset_after_first);
    }

    #[test]
    fn session_reset_on_inode_change() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![make_tool_use(
            "t1",
            "Read",
            serde_json::json!({"file_path": "/a.rs"}),
        )];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        parse_transcript(Some(&path), &mut state);
        assert_eq!(state.tools.len(), 1);

        state.inode = 99999;

        let data = parse_transcript(Some(&path), &mut state);
        assert_eq!(data.tools.len(), 1);
        assert!(data.tools.contains_key("t1"));
    }

    #[test]
    fn session_reset_on_size_shrink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");

        {
            let mut f = fs::File::create(&path).unwrap();
            writeln!(
                f,
                "{}",
                make_tool_use("t1", "Read", serde_json::json!({"file_path": "/a.rs"}))
            )
            .unwrap();
            writeln!(
                f,
                "{}",
                make_tool_use("t2", "Write", serde_json::json!({"file_path": "/b.rs"}))
            )
            .unwrap();
        }

        let mut state = State::default();
        parse_transcript(Some(path.to_str().unwrap()), &mut state);
        assert_eq!(state.tools.len(), 2);

        {
            let mut f = fs::File::create(&path).unwrap();
            writeln!(
                f,
                "{}",
                make_tool_use("t3", "Grep", serde_json::json!({"pattern": "foo"}))
            )
            .unwrap();
        }

        let data = parse_transcript(Some(path.to_str().unwrap()), &mut state);
        assert_eq!(data.tools.len(), 1);
        assert!(data.tools.contains_key("t3"));
    }

    #[test]
    fn malformed_lines_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "not json at all").unwrap();
        writeln!(f, "{{\"broken").unwrap();
        writeln!(
            f,
            "{}",
            make_tool_use("t1", "Read", serde_json::json!({"file_path": "/a.rs"}))
        )
        .unwrap();
        drop(f);

        let mut state = State::default();
        let data = parse_transcript(Some(path.to_str().unwrap()), &mut state);
        assert_eq!(data.tools.len(), 1);
    }

    #[test]
    fn agent_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            make_tool_use(
                "a1",
                "Agent",
                serde_json::json!({
                    "subagent_type": "Explore",
                    "model": "haiku",
                    "description": "find files"
                }),
            ),
            make_tool_result("a1", false),
        ];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert_eq!(data.agents.len(), 1);
        let agent = &data.agents["a1"];
        assert_eq!(agent.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(agent.model.as_deref(), Some("haiku"));
        assert!(agent.completed);
    }

    #[test]
    fn todo_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![make_tool_use(
            "tw1",
            "TodoWrite",
            serde_json::json!({
                "todos": [
                    {"content": "task a", "status": "completed"},
                    {"content": "task b", "status": "pending"},
                    {"content": "task c", "status": "in_progress"}
                ]
            }),
        )];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert_eq!(data.todos.len(), 3);
        assert!(data.todos[0].completed);
        assert!(!data.todos[1].completed);
        assert!(!data.todos[2].completed);
    }

    #[test]
    fn task_create_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            make_tool_use(
                "tc1",
                "TaskCreate",
                serde_json::json!({
                    "subject": "implement feature",
                    "description": "details",
                    "status": "pending"
                }),
            ),
            make_tool_use(
                "tc2",
                "TaskCreate",
                serde_json::json!({
                    "subject": "write tests",
                    "status": "in_progress"
                }),
            ),
        ];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert_eq!(data.tasks.len(), 2);
        assert_eq!(data.tasks["tc1"].status, TaskStatus::Pending);
        assert_eq!(data.tasks["tc2"].status, TaskStatus::InProgress);
    }

    #[test]
    fn task_update_changes_status() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            make_tool_use(
                "tc1",
                "TaskCreate",
                serde_json::json!({
                    "subject": "do thing",
                    "status": "pending"
                }),
            ),
            make_tool_use(
                "tu1",
                "TaskUpdate",
                serde_json::json!({
                    "taskId": "tc1",
                    "status": "completed"
                }),
            ),
        ];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert_eq!(data.tasks["tc1"].status, TaskStatus::Completed);
    }

    #[test]
    fn task_create_rekeyed_from_tool_result() {
        // Real production shape: TaskCreate input has no taskId, but the
        // tool_result text contains "Task #N". TaskUpdate then references that
        // numeric N. Without re-keying, every update misses the HashMap.
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            make_tool_use(
                "toolu_abc",
                "TaskCreate",
                serde_json::json!({"subject": "do thing", "activeForm": "doing"}),
            ),
            make_tool_result_with_content(
                "toolu_abc",
                false,
                "Task #1 created successfully: do thing",
            ),
            make_tool_use(
                "toolu_def",
                "TaskUpdate",
                serde_json::json!({"taskId": "1", "status": "completed"}),
            ),
        ];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert!(!data.tasks.contains_key("toolu_abc"), "should be re-keyed");
        assert_eq!(data.tasks["1"].status, TaskStatus::Completed);
    }

    #[test]
    fn parse_task_create_id_examples() {
        assert_eq!(parse_task_create_id("Task #1 created successfully: foo"), Some("1".into()));
        assert_eq!(parse_task_create_id("Task #42 created successfully: bar"), Some("42".into()));
        assert_eq!(parse_task_create_id("nothing matches here"), None);
        assert_eq!(parse_task_create_id("Task # missing number"), None);
    }

    #[test]
    fn task_update_unknown_id_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![make_tool_use(
            "tu1",
            "TaskUpdate",
            serde_json::json!({
                "taskId": "nonexistent",
                "status": "completed"
            }),
        )];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);
        assert!(data.tasks.is_empty());
    }

    #[test]
    fn status_parsing() {
        assert_eq!(parse_status("pending"), TaskStatus::Pending);
        assert_eq!(parse_status("not_started"), TaskStatus::Pending);
        assert_eq!(parse_status("in_progress"), TaskStatus::InProgress);
        assert_eq!(parse_status("running"), TaskStatus::InProgress);
        assert_eq!(parse_status("completed"), TaskStatus::Completed);
        assert_eq!(parse_status("complete"), TaskStatus::Completed);
        assert_eq!(parse_status("done"), TaskStatus::Completed);
    }

    #[test]
    fn task_create_status_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            make_tool_use(
                "tc1",
                "TaskCreate",
                serde_json::json!({"subject": "a", "status": "not_started"}),
            ),
            make_tool_use(
                "tc2",
                "TaskCreate",
                serde_json::json!({"subject": "b", "status": "running"}),
            ),
            make_tool_use(
                "tc3",
                "TaskCreate",
                serde_json::json!({"subject": "c", "status": "done"}),
            ),
            make_tool_use(
                "tc4",
                "TaskCreate",
                serde_json::json!({"subject": "d", "status": "complete"}),
            ),
        ];
        let path = write_transcript(&dir, &lines);

        let mut state = State::default();
        let data = parse_transcript(Some(&path), &mut state);

        assert_eq!(data.tasks["tc1"].status, TaskStatus::Pending);
        assert_eq!(data.tasks["tc2"].status, TaskStatus::InProgress);
        assert_eq!(data.tasks["tc3"].status, TaskStatus::Completed);
        assert_eq!(data.tasks["tc4"].status, TaskStatus::Completed);
    }

    #[test]
    fn extract_target_variations() {
        assert_eq!(
            extract_target("Read", Some(&serde_json::json!({"file_path": "/foo/bar.rs"}))),
            Some("bar.rs".to_string())
        );
        assert_eq!(
            extract_target("Grep", Some(&serde_json::json!({"pattern": "TODO"}))),
            Some("TODO".to_string())
        );
        assert_eq!(
            extract_target(
                "Bash",
                Some(&serde_json::json!({"command": "cargo test --release --all"}))
            ),
            Some("cargo test --release --all".to_string())
        );
        let long_cmd = "a".repeat(50);
        let result = extract_target("Bash", Some(&serde_json::json!({"command": long_cmd})));
        assert!(result.unwrap().ends_with("..."));
    }
}
