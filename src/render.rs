use std::collections::HashMap;

use crate::git::GitInfo;
use crate::types::{AgentEntry, CacheStatus, Config, StdinData, TaskItem, TaskStatus, TodoItem, ToolEntry, TranscriptData};
use crate::types::UsageInfo;

/// Cache TTL in seconds. Set to 280 (not 300) because we stamp on statusline fire,
/// which lags the API request timestamp by ~20s; using 300 would briefly show "warm"
/// after Anthropic's cache had already expired.
const CACHE_TTL_SECS: i64 = 280;

fn context_percentage(data: &StdinData) -> Option<u8> {
    data.context_window.as_ref()?.used_percentage
}

fn color_for_percentage(pct: u8, config: &Config) -> &'static str {
    let warning = config.context_warning.unwrap_or(70);
    let critical = config.context_critical.unwrap_or(85);
    if pct > critical {
        "\x1b[31m"
    } else if pct >= warning {
        "\x1b[33m"
    } else {
        "\x1b[32m"
    }
}

const RESET: &str = "\x1b[0m";

const BRIGHT: &str = "\x1b[1;37m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
#[allow(dead_code)]
const BLUE: &str = "\x1b[34m";
#[allow(dead_code)]
const MAGENTA: &str = "\x1b[35m";
#[allow(dead_code)]
const USAGE_HIGH: f64 = 80.0;

/// 5-hour rate-limit window in seconds.
const FIVE_HOUR_SECS: i64 = 5 * 3600;

/// 10-level dark-green→deep-red gradient, mirrors the old bash statusline's `LEVEL_N`.
const USAGE_GRADIENT: [&str; 10] = [
    "\x1b[38;5;22m",  // dark green
    "\x1b[38;5;28m",  // soft green
    "\x1b[38;5;34m",  // medium green
    "\x1b[38;5;100m", // green-yellow dark
    "\x1b[38;5;142m", // olive
    "\x1b[38;5;178m", // muted yellow
    "\x1b[38;5;172m", // muted yellow-orange
    "\x1b[38;5;166m", // darker orange
    "\x1b[38;5;160m", // dark red
    "\x1b[38;5;124m", // deep red
];

/// 6-tier pace marker colors (projected end-of-window utilization).
const PACE_COMFORTABLE: &str = "\x1b[38;5;34m"; // green
const PACE_ON_TRACK: &str = "\x1b[38;5;37m"; // teal
const PACE_WARMING: &str = "\x1b[38;5;178m"; // yellow
const PACE_PRESSING: &str = "\x1b[38;5;208m"; // orange
const PACE_CRITICAL: &str = "\x1b[38;5;160m"; // red
const PACE_RUNAWAY: &str = "\x1b[38;5;96m"; // muted purple (was 135 — too eye-catching)

/// Pick a usage-gradient color for a percentage 0..=100.
fn usage_color(pct: u8) -> &'static str {
    let idx = ((pct as usize).saturating_sub(1)) / 10;
    USAGE_GRADIENT[idx.min(9)]
}

/// Pick a pace color from projected percentage. Only meaningful once enough time has elapsed.
fn pace_color(projected_pct: u32) -> &'static str {
    if projected_pct < 50 {
        PACE_COMFORTABLE
    } else if projected_pct < 75 {
        PACE_ON_TRACK
    } else if projected_pct < 90 {
        PACE_WARMING
    } else if projected_pct < 100 {
        PACE_PRESSING
    } else if projected_pct < 120 {
        PACE_CRITICAL
    } else {
        PACE_RUNAWAY
    }
}

fn colorize(text: String, color: &str, enabled: bool) -> String {
    if enabled {
        format!("{color}{text}{RESET}")
    } else {
        text
    }
}

/// The raw separator wrapped in DIM when colors are enabled. Both the outer
/// section join and the activity line's internal join use this for consistency.
fn rendered_separator(config: &Config) -> String {
    let raw = config.separator();
    if config.colors() {
        format!("{DIM}{raw}{RESET}")
    } else {
        raw.to_string()
    }
}

#[allow(dead_code)]
fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        return "<1m".to_string();
    }
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    let remaining_hours = hours % 24;
    let remaining_minutes = minutes % 60;
    if days > 0 {
        format!("{days}d {remaining_hours}h")
    } else if hours > 0 {
        format!("{hours}h {remaining_minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn format_agent_duration(seconds: i64) -> String {
    if seconds < 0 {
        return "0s".to_string();
    }
    let minutes = seconds / 60;
    let secs = seconds % 60;
    if minutes == 0 {
        format!("{secs}s")
    } else {
        format!("{minutes}m {secs}s")
    }
}

/// Build a 10-cell usage bar with an optional pace marker inserted in place of one cell.
/// Returns just the bar string (no leading space, no color codes — caller wraps).
/// 10 cells matches the old bash statusline; at 5 cells the marker swallowing a cell
/// distorts the visible fill too much (1 cell = 20% of the bar).
fn build_usage_bar(pct: u8, reset_secs: Option<i64>) -> String {
    const CELLS: usize = 10;
    let filled = ((pct as usize * CELLS + 50) / 100).min(CELLS);
    let mut cells: Vec<&str> = (0..CELLS)
        .map(|i| if i < filled { "▓" } else { "░" })
        .collect();

    // Pace marker: replace one cell at the elapsed-time position with ┃.
    // Only insert when we know the window's remaining time and we're inside it.
    if let Some(remaining) = reset_secs {
        if remaining > 0 && remaining < FIVE_HOUR_SECS {
            let elapsed = FIVE_HOUR_SECS - remaining;
            let pos = ((elapsed as usize * CELLS + (FIVE_HOUR_SECS as usize / 2)) / FIVE_HOUR_SECS as usize).min(CELLS - 1);
            cells[pos] = "┃";
        }
    }
    cells.concat()
}

/// Format a Unix timestamp as 24h `HH:MM` in local time, rounded to the nearest minute.
fn format_reset_clock(reset_secs: i64) -> String {
    use chrono::{Local, TimeZone};
    let now = chrono::Utc::now().timestamp();
    let mut epoch = now + reset_secs;
    let secs_part = epoch % 60;
    if secs_part >= 30 {
        epoch += 60 - secs_part;
    } else {
        epoch -= secs_part;
    }
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Combined usage line in old-script style: `Usage: N% ▓▓┃░░ → 12:20`.
/// Only the 5-hour window is rendered; weekly is intentionally dropped from the line.
fn render_usage(usage: Option<&UsageInfo>, config: &Config) -> Option<String> {
    let info = usage?;
    let pct_f = info.usage_5h?;
    let pct = pct_f.round().clamp(0.0, 100.0) as u8;
    let colors = config.colors();

    let bar = build_usage_bar(pct, info.reset_5h);
    let reset_part = info
        .reset_5h
        .filter(|&s| s > 0)
        .map(|s| format!(" → {}", format_reset_clock(s)))
        .unwrap_or_default();

    let body = format!("Usage: {pct}% {bar}{reset_part}");
    if !colors {
        return Some(body);
    }

    let base = usage_color(pct);
    // If we drew a pace marker, color it independently from the bar tint.
    let label_text = if let Some(remaining) = info.reset_5h.filter(|&s| s > 0 && s < FIVE_HOUR_SECS) {
        let elapsed = FIVE_HOUR_SECS - remaining;
        // Need >=540s elapsed (matches old script) before projecting.
        let projected: u32 = if elapsed >= 540 {
            ((pct as u64 * FIVE_HOUR_SECS as u64) / elapsed as u64) as u32
        } else {
            pct as u32
        };
        let pc = pace_color(projected);
        // Replace ┃ in the bar with a colored version that snaps back to the base tint.
        let colored_bar = bar.replace('┃', &format!("{pc}┃{RESET}{base}"));
        format!("{base}Usage: {pct}% {colored_bar}{reset_part}{RESET}")
    } else {
        format!("{base}{body}{RESET}")
    };
    Some(label_text)
}

fn render_tasks(todos: &[TodoItem], tasks: &HashMap<String, TaskItem>, config: &Config) -> Option<String> {
    let todo_total = todos.len();
    let todo_completed = todos.iter().filter(|t| t.completed).count();

    let task_total = tasks.len();
    let task_completed = tasks.values().filter(|t| t.status == TaskStatus::Completed).count();

    let total = todo_total + task_total;
    let completed = todo_completed + task_completed;

    if total == 0 {
        return None;
    }

    let label = format!("tasks {completed}/{total}");
    let color = if completed == total { GREEN } else { DIM };
    Some(colorize(label, color, config.colors()))
}

enum TimelineKind {
    Tool,
    Agent,
}

struct TimelineItem<'a> {
    kind: TimelineKind,
    name: &'a str,
    target: Option<&'a str>,
    seq: u64,
    completed: bool,
    start_time: Option<i64>,
    model: Option<&'a str>,
}

/// Collapse consecutive same-named items into `(name, count)` pairs.
/// Pure run-length encoding, scoped to runs (not global) — `[A, A, B, A]` stays
/// `[(A, 2), (B, 1), (A, 1)]`.
fn group_consecutive_names<'a, I: IntoIterator<Item = &'a str>>(names: I) -> Vec<(&'a str, usize)> {
    let mut groups: Vec<(&'a str, usize)> = Vec::new();
    for name in names {
        if let Some(last) = groups.last_mut() {
            if last.0 == name {
                last.1 += 1;
                continue;
            }
        }
        groups.push((name, 1));
    }
    groups
}

fn render_activity_line(tools: &HashMap<String, ToolEntry>, agents: &HashMap<String, AgentEntry>, config: &Config) -> Option<String> {
    let mut items: Vec<TimelineItem> = Vec::new();

    for t in tools.values() {
        items.push(TimelineItem {
            kind: TimelineKind::Tool,
            name: &t.name,
            target: t.target.as_deref(),
            seq: t.seq,
            completed: t.completed,
            start_time: None,
            model: None,
        });
    }

    for a in agents.values() {
        items.push(TimelineItem {
            kind: TimelineKind::Agent,
            name: a.subagent_type.as_deref().unwrap_or("agent"),
            target: a.description.as_deref(),
            seq: a.seq,
            completed: a.completed,
            start_time: a.start_time,
            model: a.model.as_deref(),
        });
    }

    if items.is_empty() {
        return None;
    }

    items.sort_by_key(|i| i.seq);

    let sep = rendered_separator(config);
    let colors = config.colors();
    let mut parts: Vec<String> = Vec::new();

    let (completed, running): (Vec<_>, Vec<_>) = items.iter().partition(|i| i.completed);

    let groups = group_consecutive_names(completed.iter().map(|i| i.name));

    // Cap completed-history tail to the most recent group only.
    for &(name, count) in groups.iter().rev().take(1).rev() {
        let label = if count == 1 {
            name.to_string()
        } else {
            format!("{name} x{count}")
        };
        parts.push(colorize(label, DIM, colors));
    }

    let now = chrono::Utc::now().timestamp();
    for item in &running {
        match item.kind {
            TimelineKind::Tool => {
                let label = match item.target {
                    Some(t) => format!("{} {t}", item.name),
                    None => item.name.to_string(),
                };
                parts.push(colorize(label, BRIGHT, colors));
            }
            TimelineKind::Agent => {
                let model_part = item.model.map(|m| format!("[{m}]")).unwrap_or_default();
                let dur = item.start_time.map(|t| format_agent_duration(now - t)).unwrap_or_default();
                let label = format!("{}{model_part} {dur}", item.name).trim().to_string();
                parts.push(colorize(label, YELLOW, colors));
            }
        }
    }

    if parts.is_empty() {
        return None;
    }

    Some(parts.join(sep.as_str()))
}

/// Render the cache TTL label using monospace glyphs (no emoji width drift):
/// - No prior hit, or remaining ≤ 0 → "⧖ cold" (dim)
/// - 0 < remaining ≤ 60s            → "⧖ m:ss" (red)
/// - 60 < remaining ≤ 180s          → "⧖ m:ss" (yellow)
/// - remaining > 180s               → "⧖ m:ss" (green)
/// If a cold miss happened in the last 30s and the cache is warm, the leading
/// glyph swaps from ⧖ to $ to flag the cost. Same color as the warm tier — line
/// width never changes.
fn render_cache_label(cache: &CacheStatus, config: &Config) -> String {
    let now = chrono::Utc::now().timestamp();
    let colors = config.colors();
    let cold = || colorize("⧖ cold".to_string(), DIM, colors);

    let last_hit = match cache.last_cache_hit {
        Some(t) => t,
        None => return cold(),
    };
    let remaining = CACHE_TTL_SECS - (now - last_hit);
    if remaining <= 0 {
        return cold();
    }

    let mins = remaining / 60;
    let secs = remaining % 60;
    let color = if remaining > 180 {
        GREEN
    } else if remaining > 60 {
        YELLOW
    } else {
        RED
    };

    // Swap glyph (not append) on a recent cold miss — keeps width constant at 6 chars.
    let recent_miss = cache
        .last_cache_miss
        .is_some_and(|t| now - t <= 30);
    let glyph = if recent_miss { '$' } else { '⧖' };
    let label = format!("{glyph} {mins}:{secs:02}");
    colorize(label, color, colors)
}

pub fn render(
    data: &StdinData,
    config: &Config,
    transcript: &TranscriptData,
    git: Option<&GitInfo>,
    usage: Option<&UsageInfo>,
    cache: &CacheStatus,
) -> String {
    let sep = rendered_separator(config);
    let colors = config.colors();

    let mut parts: Vec<String> = Vec::new();

    // 1. Project (cwd basename, with configurable path_levels)
    let project_name = data
        .cwd
        .as_deref()
        .map(|p| {
            let mut bits: Vec<&str> = p.rsplit('/').take(config.path_levels() as usize).collect();
            bits.reverse();
            bits.join("/")
        })
        .unwrap_or_else(|| "no data".into());
    parts.push(project_name);

    // 2. Git branch — green ⎇ glyph, asterisk suffix when dirty.
    if let Some(gi) = git {
        let dirty = if gi.dirty { "*" } else { "" };
        parts.push(colorize(format!("⎇ {}{dirty}", gi.branch), GREEN, colors));
    }

    // 3. Model
    let model_name = data
        .model
        .as_ref()
        .and_then(|m| m.display_name.as_deref())
        .unwrap_or("cstat");
    parts.push(model_name.to_string());

    // 4. Context %
    if let Some(pct) = context_percentage(data) {
        let color = color_for_percentage(pct, config);
        parts.push(colorize(format!("Ctx: {pct}%"), color, colors));
    }

    // 5. Cache TTL (always shown — ⧖ cold when no hit / expired)
    parts.push(render_cache_label(cache, config));

    // 6. Usage (hourly only — combined with bar + clock-time reset)
    if let Some(u) = render_usage(usage, config) {
        parts.push(u);
    }

    // 7. Activity (running tools, agents, recent completions)
    if let Some(a) = render_activity_line(&transcript.tools, &transcript.agents, config) {
        parts.push(a);
    }

    // 8. Tasks
    if let Some(t) = render_tasks(&transcript.todos, &transcript.tasks, config) {
        parts.push(t);
    }

    parts.join(sep.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentEntry, ContextWindow, Model, StdinData, TaskItem, TaskStatus, TodoItem, ToolEntry, TranscriptData};

    fn make_data(pct: Option<u8>) -> StdinData {
        StdinData {
            model: Some(Model {
                display_name: Some("Opus".into()),
            }),
            cwd: Some("/home/user/my-project".into()),
            context_window: Some(ContextWindow {
                used_percentage: pct,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn render_model_and_project() {
        let data = StdinData {
            model: Some(Model {
                display_name: Some("Opus".into()),
            }),
            cwd: Some("/home/user/my-project".into()),
            ..Default::default()
        };
        let out = render(&data, &Config::default(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("Opus"));
        assert!(out.contains("my-project"));
    }

    #[test]
    fn render_empty_stdin() {
        let data = StdinData::default();
        let out = render(&data, &Config::default(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("cstat"));
        assert!(out.contains("no data"));
    }

    #[test]
    fn render_missing_model_name() {
        let data = StdinData {
            model: Some(Model { display_name: None }),
            cwd: Some("/tmp/foo".into()),
            ..Default::default()
        };
        let out = render(&data, &Config::default(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(out.contains("cstat"));
        assert!(out.contains("foo"));
    }

    // Color-tier logic: tested directly against `color_for_percentage`, not via
    // rendered ANSI strings. See `color_for_percentage_*` tests below.
    //
    // Section omission and label format are covered by `context_missing_window`,
    // `context_missing_tokens`, and the integration tests.

    #[test]
    fn context_missing_window() {
        let data = StdinData {
            model: Some(Model {
                display_name: Some("Opus".into()),
            }),
            cwd: Some("/home/user/my-project".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(!out.contains("Ctx:"));
    }

    #[test]
    fn context_missing_tokens() {
        let data = make_data(None);
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(!out.contains("Ctx:"));
    }

    #[test]
    fn path_levels_2() {
        let data = StdinData {
            model: Some(Model {
                display_name: Some("Opus".into()),
            }),
            cwd: Some("/home/user/my-project".into()),
            ..Default::default()
        };
        let cfg = Config {
            path_levels: Some(2),
            ..Default::default()
        };
        let out = render(&data, &cfg, &TranscriptData::default(), None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.starts_with("user/my-project"));
    }

    #[test]
    fn path_levels_3() {
        let data = StdinData {
            model: Some(Model {
                display_name: Some("Opus".into()),
            }),
            cwd: Some("/home/user/my-project".into()),
            ..Default::default()
        };
        let cfg = Config {
            path_levels: Some(3),
            ..Default::default()
        };
        let out = render(&data, &cfg, &TranscriptData::default(), None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.starts_with("home/user/my-project"));
    }

    #[test]
    fn custom_separator() {
        let data = make_data(Some(10));
        let cfg = Config {
            colors: Some(false),
            separator: Some(" | ".into()),
            ..Default::default()
        };
        let out = render(&data, &cfg, &TranscriptData::default(), None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("my-project | "));
        assert!(line2.contains(" | Ctx: 10%"));
    }

    fn tool(name: &str, target: Option<&str>, completed: bool) -> ToolEntry {
        tool_seq(name, target, completed, 0)
    }

    fn tool_seq(name: &str, target: Option<&str>, completed: bool, seq: u64) -> ToolEntry {
        ToolEntry {
            name: name.to_string(),
            target: target.map(|s| s.to_string()),
            completed,
            error: false,
            seq,
        }
    }

    fn no_colors_cfg() -> Config {
        Config {
            colors: Some(false),
            ..Default::default()
        }
    }

    #[test]
    fn activity_shown_on_line2() {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Edit", Some("auth.ts"), false));
        let transcript = TranscriptData {
            tools,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("Edit auth.ts"));
    }

    #[test]
    fn activity_running_tool_without_target() {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Glob", None, false));
        let transcript = TranscriptData {
            tools,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("Glob"));
    }

    #[test]
    fn activity_shows_only_last_completed_group() {
        // Cap is 1 — only the most recent consecutive run should appear.
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool_seq("Read", Some("a.rs"), true, 0));
        tools.insert("t2".into(), tool_seq("Read", Some("b.rs"), true, 1));
        tools.insert("t3".into(), tool_seq("Grep", Some("TODO"), true, 2));
        tools.insert("t4".into(), tool_seq("Edit", Some("c.rs"), true, 3));
        let activity = render_activity_line(&tools, &HashMap::new(), &no_colors_cfg()).unwrap();
        assert!(activity.contains("Edit"), "expected most-recent group, got: {activity}");
        assert!(!activity.contains("Read"), "Read should be capped out, got: {activity}");
        assert!(!activity.contains("Grep"), "Grep should be capped out, got: {activity}");
    }

    #[test]
    fn activity_running_plus_completed() {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Read", Some("a.rs"), true));
        tools.insert("t2".into(), tool("Read", Some("b.rs"), true));
        tools.insert("t3".into(), tool("Edit", Some("main.rs"), false));
        let transcript = TranscriptData {
            tools,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("Edit main.rs"));
        assert!(line2.contains("Read x2"));
    }

    #[test]
    fn activity_with_colors() {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Edit", Some("auth.ts"), false));
        tools.insert("t2".into(), tool("Read", Some("a.rs"), true));
        let transcript = TranscriptData {
            tools,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &Config::default(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains(BRIGHT));
        assert!(line2.contains(DIM));
        assert!(line2.contains(RESET));
    }

    #[test]
    fn activity_single_completed_no_count() {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Grep", Some("TODO"), true));
        let transcript = TranscriptData {
            tools,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("Grep"));
        assert!(!line2.contains("x1"));
    }

    #[test]
    fn activity_running_agent_with_model() {
        let mut agents = HashMap::new();
        agents.insert(
            "a1".into(),
            AgentEntry {
                subagent_type: Some("explore".into()),
                model: Some("haiku".into()),
                description: Some("find files".into()),
                start_time: Some(chrono::Utc::now().timestamp() - 135),
                completed: false,
                seq: 0,
            },
        );
        let transcript = TranscriptData {
            agents,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("explore[haiku] 2m 15s"));
    }

    #[test]
    fn activity_running_agent_without_model() {
        let mut agents = HashMap::new();
        agents.insert(
            "a1".into(),
            AgentEntry {
                subagent_type: Some("general-purpose".into()),
                model: None,
                description: None,
                start_time: Some(chrono::Utc::now().timestamp() - 45),
                completed: false,
                seq: 0,
            },
        );
        let transcript = TranscriptData {
            agents,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("general-purpose 45s"));
    }

    #[test]
    fn activity_completed_agent_in_history() {
        let mut agents = HashMap::new();
        agents.insert(
            "a1".into(),
            AgentEntry {
                subagent_type: Some("explore".into()),
                model: Some("haiku".into()),
                description: None,
                start_time: Some(chrono::Utc::now().timestamp() - 60),
                completed: true,
                seq: 0,
            },
        );
        let transcript = TranscriptData {
            agents,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        assert!(out.contains("explore"));
    }

    #[test]
    fn activity_agent_yellow_with_colors() {
        let mut agents = HashMap::new();
        agents.insert(
            "a1".into(),
            AgentEntry {
                subagent_type: Some("explore".into()),
                model: None,
                description: None,
                start_time: Some(chrono::Utc::now().timestamp() - 10),
                completed: false,
                seq: 0,
            },
        );
        let transcript = TranscriptData {
            agents,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &Config::default(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains(YELLOW));
    }

    #[test]
    fn activity_uses_config_separator() {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Read", None, true));
        tools.insert("t2".into(), tool("Grep", None, true));
        let transcript = TranscriptData {
            tools,
            ..Default::default()
        };
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let cfg = Config {
            colors: Some(false),
            separator: Some(" | ".into()),
            ..Default::default()
        };
        let out = render(&data, &cfg, &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains(" | "));
    }

    #[test]
    fn git_branch_shown() {
        let data = StdinData {
            model: Some(Model { display_name: Some("Opus".into()) }),
            cwd: Some("/tmp/proj".into()),
            ..Default::default()
        };
        let git = GitInfo { branch: "main".into(), dirty: false };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), Some(&git), None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("main"));
        assert!(!line2.contains("git:"));
    }

    #[test]
    fn git_dirty_indicator() {
        let data = StdinData {
            model: Some(Model { display_name: Some("Opus".into()) }),
            cwd: Some("/tmp/proj".into()),
            ..Default::default()
        };
        let git = GitInfo { branch: "feat/x".into(), dirty: true };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), Some(&git), None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("feat/x*"));
    }

    #[test]
    fn git_with_colors() {
        let data = StdinData {
            model: Some(Model { display_name: Some("Opus".into()) }),
            cwd: Some("/tmp/proj".into()),
            ..Default::default()
        };
        let git = GitInfo { branch: "main".into(), dirty: false };
        let out = render(&data, &Config::default(), &TranscriptData::default(), Some(&git), None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains(DIM));
        assert!(line2.contains("main"));
        assert!(!line2.contains("git:"));
    }

    #[test]
    fn git_omitted_when_none() {
        let data = StdinData {
            model: Some(Model { display_name: Some("Opus".into()) }),
            cwd: Some("/tmp/proj".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(!out.contains("main"));
    }

    #[test]
    fn git_with_context() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "dev".into(), dirty: false };
        let cfg = Config { colors: Some(false), ..Default::default() };
        let out = render(&data, &cfg, &TranscriptData::default(), Some(&git), None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("my-project"));
        assert!(line2.contains("dev"));
        assert!(line2.contains("Ctx: 45%"));
    }

    #[test]
    fn format_duration_under_minute() {
        assert_eq!(format_duration(0), "<1m");
        assert_eq!(format_duration(30), "<1m");
        assert_eq!(format_duration(59), "<1m");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(3599), "59m");
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(5400), "1h 30m");
        assert_eq!(format_duration(7200), "2h 0m");
    }

    #[test]
    fn tasks_shown_from_todos() {
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let transcript = TranscriptData {
            todos: vec![
                TodoItem { content: "a".into(), completed: true },
                TodoItem { content: "b".into(), completed: false },
                TodoItem { content: "c".into(), completed: true },
            ],
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("tasks 2/3"));
    }

    #[test]
    fn tasks_shown_from_task_items() {
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let mut tasks = HashMap::new();
        tasks.insert("t1".into(), TaskItem { status: TaskStatus::Completed });
        tasks.insert("t2".into(), TaskItem { status: TaskStatus::Pending });
        tasks.insert("t3".into(), TaskItem { status: TaskStatus::InProgress });
        let transcript = TranscriptData {
            tasks,
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("tasks 1/3"));
    }

    #[test]
    fn tasks_combined_todos_and_task_items() {
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let mut tasks = HashMap::new();
        tasks.insert("t1".into(), TaskItem { status: TaskStatus::Completed });
        tasks.insert("t2".into(), TaskItem { status: TaskStatus::Pending });
        let transcript = TranscriptData {
            todos: vec![
                TodoItem { content: "a".into(), completed: true },
                TodoItem { content: "b".into(), completed: false },
            ],
            tasks,
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("tasks 2/4"));
    }

    #[test]
    fn tasks_hidden_when_empty() {
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(!out.contains("tasks"));
    }

    #[test]
    fn tasks_green_when_all_completed() {
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let transcript = TranscriptData {
            todos: vec![
                TodoItem { content: "a".into(), completed: true },
                TodoItem { content: "b".into(), completed: true },
            ],
            ..Default::default()
        };
        let out = render(&data, &Config::default(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains(GREEN));
        assert!(line2.contains("tasks 2/2"));
    }

    #[test]
    fn tasks_dim_when_not_all_completed() {
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let transcript = TranscriptData {
            todos: vec![
                TodoItem { content: "a".into(), completed: true },
                TodoItem { content: "b".into(), completed: false },
            ],
            ..Default::default()
        };
        let out = render(&data, &Config::default(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains(DIM));
        assert!(line2.contains("tasks 1/2"));
    }

    #[test]
    fn tasks_alongside_tools() {
        let data = StdinData {
            cwd: Some("/tmp/p".into()),
            ..Default::default()
        };
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Read", Some("a.rs"), true));
        let transcript = TranscriptData {
            tools,
            todos: vec![
                TodoItem { content: "a".into(), completed: true },
                TodoItem { content: "b".into(), completed: false },
            ],
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("Read"));
        assert!(line2.contains("tasks 1/2"));
    }

    // Usage rendering details (color tier, bar shape, clock format) are tested
    // directly against `usage_color`, `pace_color`, and `build_usage_bar`.
    // Visual regressions are caught by the integration tests.

    #[test]
    fn usage_omitted_when_none() {
        let data = StdinData {
            model: Some(Model { display_name: Some("Opus".into()) }),
            cwd: Some("/tmp/proj".into()),
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(!out.contains("Usage:"));
        assert!(out.contains("Opus"));
        assert!(out.contains("proj"));
    }

    #[test]
    fn usage_renders_when_present() {
        // One end-to-end test that the section actually appears on the line.
        let data = make_data(Some(45));
        let usage = UsageInfo {
            usage_5h: Some(25.0),
            usage_7d: Some(60.0),
            reset_5h: Some(5400),
            reset_7d: None,
        };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, Some(&usage), &CacheStatus::default());
        assert!(out.contains("Usage: 25%"));
    }

    #[test]
    fn single_line_layout_order() {
        // Section order contract: project … context … cache … usage.
        let data = make_data(Some(45));
        let usage = UsageInfo {
            usage_5h: Some(25.0),
            usage_7d: None,
            reset_5h: Some(5400),
            reset_7d: None,
        };
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, Some(&usage), &CacheStatus::default());
        assert_eq!(out.lines().count(), 1);
        let ctx = out.find("Ctx: 45%").unwrap();
        let cache = out.find('⧖').unwrap();
        let usage_pos = out.find("Usage:").unwrap();
        assert!(ctx < cache, "Ctx should precede cache label");
        assert!(cache < usage_pos, "cache label should precede Usage");
    }

    fn full_transcript() -> TranscriptData {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Read", Some("a.rs"), true));
        tools.insert("t2".into(), tool("Edit", Some("b.rs"), false));
        let mut agents = HashMap::new();
        agents.insert(
            "a1".into(),
            AgentEntry {
                subagent_type: Some("explore".into()),
                model: Some("haiku".into()),
                description: None,
                start_time: Some(chrono::Utc::now().timestamp() - 30),
                completed: false,
                seq: 2,
            },
        );
        let mut tasks = HashMap::new();
        tasks.insert("tk1".into(), TaskItem { status: TaskStatus::Completed });
        tasks.insert("tk2".into(), TaskItem { status: TaskStatus::Pending });
        TranscriptData {
            tools,
            agents,
            todos: vec![
                TodoItem { content: "x".into(), completed: true },
                TodoItem { content: "y".into(), completed: false },
            ],
            tasks,
        }
    }

    fn full_usage() -> UsageInfo {
        UsageInfo {
            usage_5h: Some(25.0),
            usage_7d: Some(60.0),
            reset_5h: Some(3600),
            reset_7d: Some(259200),
        }
    }

    #[test]
    fn all_data_present_no_colors() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "main".into(), dirty: true };
        let usage = full_usage();
        let transcript = full_transcript();
        let out = render(&data, &no_colors_cfg(), &transcript, Some(&git), Some(&usage), &CacheStatus::default());
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("Opus"));
        assert!(out.contains("Usage: 25%"));
        assert!(out.contains("my-project"));
        assert!(out.contains("main*"));
        assert!(out.contains("Ctx: 45%"));
        assert!(out.contains("Edit b.rs"));
        assert!(out.contains("Read"));
        assert!(out.contains("explore[haiku]"));
        assert!(out.contains("tasks 2/4"));
    }

    #[test]
    fn all_data_present_with_colors() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "main".into(), dirty: false };
        let usage = full_usage();
        let transcript = full_transcript();
        let out = render(&data, &Config::default(), &transcript, Some(&git), Some(&usage), &CacheStatus::default());
        // Branch is green, separator + completed-history + tasks dim, agent yellow.
        assert!(out.contains(GREEN));
        assert!(out.contains(DIM));
        assert!(out.contains(BRIGHT));
        assert!(out.contains(YELLOW));
        assert!(out.contains(RESET));
    }

    #[test]
    fn all_data_present_custom_separator() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "main".into(), dirty: false };
        let usage = full_usage();
        let transcript = full_transcript();
        let cfg = Config {
            colors: Some(false),
            separator: Some(" | ".into()),
            ..Default::default()
        };
        let out = render(&data, &cfg, &transcript, Some(&git), Some(&usage), &CacheStatus::default());
        assert!(out.contains(" | Usage: 25%"));
        assert!(out.contains(" | Ctx: 45%"));
    }

    #[test]
    fn missing_git_only() {
        let data = make_data(Some(45));
        let usage = full_usage();
        let transcript = full_transcript();
        let out = render(&data, &no_colors_cfg(), &transcript, None, Some(&usage), &CacheStatus::default());
        assert!(!out.contains("⎇"));
        assert!(out.contains("Ctx: 45%"));
        assert!(out.contains("Usage: 25%"));
    }

    #[test]
    fn missing_context_only() {
        let data = StdinData {
            model: Some(Model { display_name: Some("Opus".into()) }),
            cwd: Some("/home/user/my-project".into()),
            ..Default::default()
        };
        let git = GitInfo { branch: "main".into(), dirty: false };
        let usage = full_usage();
        let transcript = full_transcript();
        let out = render(&data, &no_colors_cfg(), &transcript, Some(&git), Some(&usage), &CacheStatus::default());
        assert!(!out.contains("Ctx:"));
        assert!(out.contains("main"));
        assert!(out.contains("Usage: 25%"));
    }

    #[test]
    fn missing_usage_only() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "main".into(), dirty: false };
        let transcript = full_transcript();
        let out = render(&data, &no_colors_cfg(), &transcript, Some(&git), None, &CacheStatus::default());
        assert!(!out.contains("Usage:"));
        assert!(out.contains("Ctx: 45%"));
        assert!(out.contains("main"));
    }

    #[test]
    fn missing_activity_only() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "main".into(), dirty: false };
        let usage = full_usage();
        let transcript = TranscriptData {
            todos: vec![
                TodoItem { content: "x".into(), completed: true },
                TodoItem { content: "y".into(), completed: false },
            ],
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, Some(&git), Some(&usage), &CacheStatus::default());
        let line2 = out.as_str();
        assert!(line2.contains("tasks 1/2"));
    }

    #[test]
    fn missing_tasks_only() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "main".into(), dirty: false };
        let usage = full_usage();
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool("Read", Some("a.rs"), true));
        let transcript = TranscriptData {
            tools,
            ..Default::default()
        };
        let out = render(&data, &no_colors_cfg(), &transcript, Some(&git), Some(&usage), &CacheStatus::default());
        let line2 = out.as_str();
        assert!(!line2.contains("tasks"));
    }

    #[test]
    fn all_blocks_missing() {
        let data = StdinData::default();
        let out = render(&data, &Config::default(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("cstat"));
        assert!(out.contains("no data"));
    }

    #[test]
    fn all_blocks_missing_no_colors() {
        let data = StdinData::default();
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn no_ansi_codes_when_colors_off() {
        let data = make_data(Some(45));
        let git = GitInfo { branch: "main".into(), dirty: true };
        let usage = full_usage();
        let transcript = full_transcript();
        let out = render(&data, &no_colors_cfg(), &transcript, Some(&git), Some(&usage), &CacheStatus::default());
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn output_never_empty() {
        let data = StdinData::default();
        let out = render(&data, &Config::default(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert!(!out.is_empty());
    }

    #[test]
    fn output_no_trailing_newline() {
        let data = make_data(Some(45));
        let transcript = full_transcript();
        let out = render(&data, &no_colors_cfg(), &transcript, None, None, &CacheStatus::default());
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn first_line_never_empty() {
        let data = StdinData::default();
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        let first = out.lines().next().unwrap();
        assert!(!first.is_empty());
    }

    #[test]
    fn always_one_line() {
        let data = StdinData::default();
        let out = render(&data, &no_colors_cfg(), &TranscriptData::default(), None, None, &CacheStatus::default());
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn format_agent_duration_zero() {
        assert_eq!(format_agent_duration(0), "0s");
    }

    #[test]
    fn format_agent_duration_negative() {
        assert_eq!(format_agent_duration(-5), "0s");
    }

    #[test]
    fn format_agent_duration_seconds() {
        assert_eq!(format_agent_duration(45), "45s");
    }

    #[test]
    fn format_agent_duration_minutes_and_seconds() {
        assert_eq!(format_agent_duration(135), "2m 15s");
    }

    #[test]
    fn context_percentage_none_when_no_context_window() {
        let data = StdinData::default();
        assert!(context_percentage(&data).is_none());
    }

    #[test]
    fn context_percentage_returns_value() {
        let data = make_data(Some(42));
        assert_eq!(context_percentage(&data), Some(42));
    }

    #[test]
    fn render_usage_empty_when_none() {
        assert!(render_usage(None, &Config::default()).is_none());
    }

    // --- Pure-helper tests: color tiers and bar shape -------------------------

    #[test]
    fn color_for_percentage_boundaries() {
        let cfg = Config::default(); // warning=70, critical=85
        assert_eq!(color_for_percentage(0, &cfg), GREEN);
        assert_eq!(color_for_percentage(69, &cfg), GREEN);
        assert_eq!(color_for_percentage(70, &cfg), YELLOW); // ≥ warning
        assert_eq!(color_for_percentage(85, &cfg), YELLOW); // ≤ critical
        assert_eq!(color_for_percentage(86, &cfg), RED); // > critical
        assert_eq!(color_for_percentage(100, &cfg), RED);
    }

    #[test]
    fn color_for_percentage_custom_thresholds() {
        let cfg = Config {
            context_warning: Some(50),
            context_critical: Some(60),
            ..Default::default()
        };
        assert_eq!(color_for_percentage(49, &cfg), GREEN);
        assert_eq!(color_for_percentage(50, &cfg), YELLOW);
        assert_eq!(color_for_percentage(60, &cfg), YELLOW);
        assert_eq!(color_for_percentage(61, &cfg), RED);
    }

    #[test]
    fn usage_color_gradient_buckets() {
        // 1..=10 → bucket 0 (darkest green); 91..=100 → bucket 9 (deep red).
        assert_eq!(usage_color(0), USAGE_GRADIENT[0]);
        assert_eq!(usage_color(1), USAGE_GRADIENT[0]);
        assert_eq!(usage_color(10), USAGE_GRADIENT[0]);
        assert_eq!(usage_color(11), USAGE_GRADIENT[1]);
        assert_eq!(usage_color(90), USAGE_GRADIENT[8]);
        assert_eq!(usage_color(91), USAGE_GRADIENT[9]);
        assert_eq!(usage_color(100), USAGE_GRADIENT[9]);
    }

    #[test]
    fn pace_color_tiers() {
        assert_eq!(pace_color(0), PACE_COMFORTABLE);
        assert_eq!(pace_color(49), PACE_COMFORTABLE);
        assert_eq!(pace_color(50), PACE_ON_TRACK);
        assert_eq!(pace_color(74), PACE_ON_TRACK);
        assert_eq!(pace_color(75), PACE_WARMING);
        assert_eq!(pace_color(89), PACE_WARMING);
        assert_eq!(pace_color(90), PACE_PRESSING);
        assert_eq!(pace_color(99), PACE_PRESSING);
        assert_eq!(pace_color(100), PACE_CRITICAL);
        assert_eq!(pace_color(119), PACE_CRITICAL);
        assert_eq!(pace_color(120), PACE_RUNAWAY);
        assert_eq!(pace_color(500), PACE_RUNAWAY);
    }

    #[test]
    fn build_usage_bar_filled_blocks() {
        // No reset → no pace marker. 10 cells, each cell = 10%; half-cell threshold = 5%.
        assert_eq!(build_usage_bar(0, None), "░░░░░░░░░░");
        assert_eq!(build_usage_bar(4, None), "░░░░░░░░░░"); // below half-cell
        assert_eq!(build_usage_bar(5, None), "▓░░░░░░░░░"); // at half-cell, rounds up
        assert_eq!(build_usage_bar(50, None), "▓▓▓▓▓░░░░░"); // (50*10+50)/100 = 5
        assert_eq!(build_usage_bar(95, None), "▓▓▓▓▓▓▓▓▓▓"); // (95*10+50)/100 = 10
        assert_eq!(build_usage_bar(100, None), "▓▓▓▓▓▓▓▓▓▓");
    }

    #[test]
    fn build_usage_bar_pace_marker_position() {
        // Halfway through the 5h window → marker should sit in the middle of 10 cells.
        let bar = build_usage_bar(40, Some(FIVE_HOUR_SECS / 2));
        assert!(bar.contains('┃'), "expected pace marker, got: {bar}");
        assert_eq!(bar.chars().count(), 10);
    }

    #[test]
    fn build_usage_bar_no_marker_when_window_unknown() {
        // Without a reset hint we can't position the marker.
        assert!(!build_usage_bar(40, None).contains('┃'));
    }

    #[test]
    fn build_usage_bar_no_marker_when_window_already_reset() {
        // remaining ≤ 0 means we're past the reset; don't draw a marker.
        assert!(!build_usage_bar(40, Some(0)).contains('┃'));
        assert!(!build_usage_bar(40, Some(-100)).contains('┃'));
    }

    #[test]
    fn render_tasks_none_when_empty() {
        assert!(render_tasks(&[], &HashMap::new(), &Config::default()).is_none());
    }

    #[test]
    fn activity_line_none_when_empty() {
        assert!(render_activity_line(&HashMap::new(), &HashMap::new(), &Config::default()).is_none());
    }

    #[test]
    fn activity_line_with_only_completed_agents_shows_history() {
        let mut agents = HashMap::new();
        agents.insert(
            "a1".into(),
            AgentEntry {
                subagent_type: Some("explore".into()),
                model: None,
                description: None,
                start_time: Some(chrono::Utc::now().timestamp() - 30),
                completed: true,
                seq: 0,
            },
        );
        let result = render_activity_line(&HashMap::new(), &agents, &Config::default());
        assert!(result.is_some());
        assert!(result.unwrap().contains("explore"));
    }

    #[test]
    fn group_consecutive_names_basic() {
        // Run-length encoding, scoped to runs (not global).
        let g = group_consecutive_names(["Read", "Read", "Grep", "Read"]);
        assert_eq!(g, vec![("Read", 2), ("Grep", 1), ("Read", 1)]);
    }

    #[test]
    fn group_consecutive_names_empty() {
        let g: Vec<(&str, usize)> = group_consecutive_names(std::iter::empty());
        assert!(g.is_empty());
    }

    #[test]
    fn group_consecutive_names_singleton() {
        assert_eq!(group_consecutive_names(["Edit"]), vec![("Edit", 1)]);
    }

    #[test]
    fn activity_agents_in_timeline() {
        let mut tools = HashMap::new();
        tools.insert("t1".into(), tool_seq("Read", Some("a.rs"), true, 0));
        let mut agents = HashMap::new();
        agents.insert(
            "a1".into(),
            AgentEntry {
                subagent_type: Some("explore".into()),
                model: None,
                description: None,
                start_time: Some(chrono::Utc::now().timestamp() - 10),
                completed: false,
                seq: 1,
            },
        );
        tools.insert("t2".into(), tool_seq("Edit", Some("b.rs"), false, 2));
        let result = render_activity_line(&tools, &agents, &no_colors_cfg());
        let s = result.unwrap();
        let read_pos = s.find("Read").unwrap();
        let explore_pos = s.find("explore").unwrap();
        let edit_pos = s.find("Edit").unwrap();
        assert!(read_pos < explore_pos);
        assert!(explore_pos < edit_pos);
    }
}
