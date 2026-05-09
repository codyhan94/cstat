# cstat architecture reference

This document covers the non-obvious design decisions in cstat. Intended as a
quick-start for future contributors (or future-you after a long break).

---

## State persistence

State is keyed by **`session_id`** (from stdin `session_id` field), not by
transcript path hash (the old approach). This makes the key stable across
restarts and unambiguous — two sessions with different IDs can't collide even
if they share a working directory.

State files live at `/Volumes/ramdisk/cstat-{session_id}.bin` with a `/tmp`
fallback when the ramdisk isn't mounted. The ramdisk is ephemeral across
reboots, so stale state doesn't accumulate indefinitely.

`session_id` is sanitized (`[A-Za-z0-9-]` only) before use in the path to
prevent path traversal.

`STATE_VERSION = 3`. On version mismatch, state is discarded and rebuilt from
scratch on the next transcript parse.

A separate global file (`cstat-rate-limits.bin`) persists the last-known rate
limit values. This lets a brand-new session show usage immediately without
waiting for the first `rate_limits` key to appear in stdin.

---

## Cache TTL detection

There is no hook script. Cache state is inferred from the `context_window`
field in stdin on every invocation.

**Turn detection**: a new turn is detected when `total_input_tokens` changes
from the value stored in state. This is the signal to inspect
`current_usage`.

**On a new turn**, branch on the cache token shape:

| `cache_read` | `cache_creation` | Meaning              | Action                              |
|-------------|-----------------|----------------------|-------------------------------------|
| > 0         | any             | Warm hit             | Stamp `last_cache_hit = now`        |
| 0           | > 0             | Cold miss            | Stamp both `last_cache_hit` and `last_cache_miss = now` |
| 0           | 0               | Post-/compact or N/A | Leave stamps alone                  |

**TTL is 280s** (not 300) to account for ~20s lag between API response receipt
and statusline invocation.

**Rendered glyph**:
- Within 30s of a cold miss: `$` (same column width as `⧖` — prevents layout
  shift mid-statusline)
- Otherwise: `⧖ m:ss` (time since last warm hit), or `⧖ cold` if no hit stamp

Color tiers: green > 180s remaining, yellow > 60s, red ≤ 60s.

---

## Rendered format

```
project │ ⎇ branch │ Model │ Ctx: N% │ ⧖ m:ss │ Usage: N% ▓▓░░┃░░░░░ → HH:MM │ activity │ tasks
```

All sections are separated by dim ` │ `. Sections that have no data are
omitted entirely (no blank separators).

**Usage bar**: 10 cells, each `▓` (filled) or `░` (empty). A `┃` pace marker
overwrites one cell at the current elapsed-time position. Pace marker colors
run green → yellow → orange → red → magenta → muted purple (ANSI 256 color
38;5;96 for the runaway tier — bright pink was too eye-catching mid-statusline).

**Activity tail**: capped to the most recent 1 completed tool group. More than
that pushes the line past ~120 chars in busy sessions.

**Weekly usage**: parsed and cached in state but not rendered by default (matches
old bash script behavior where `SHOW_WEEKLY=0`).

---

## Transcript parsing: tasks

Claude Code's task system assigns numeric ids at runtime, not at creation time.
The `TaskCreate` tool input contains `{subject, description, activeForm}` — no
id. The runtime surfaces the assigned id in the **tool_result** text:
`"Task #N created successfully: ..."`.

`TaskUpdate` references tasks by that numeric id (e.g. `taskId: "1"`).

The parser handles this by:
1. On `TaskCreate`, inserting the task under the `tool_use_id` placeholder key.
2. On the matching tool_result, extracting the numeric id from the result text
   and re-keying the HashMap entry from `toolu_xxx` → `"N"`.
3. `TaskUpdate` then finds the entry normally.

Without the re-keying step, every update silently misses and the task count
stays permanently at 0 completed.

---

## Test philosophy

Unit tests cover **pure helpers only**: color tier boundaries, bar cell math,
pace marker placement, `group_consecutive_names`, `parse_task_create_id`. These
are cheap and refactor-stable.

Integration tests run the compiled binary end-to-end via `Command` and assert
on structural contracts ("contains `Ctx:`", "contains `⧖`") rather than exact
strings. This tolerates cosmetic changes without breaking.

Tests that asserted on exact ANSI escape sequences or full rendered lines were
removed — they required updating on every color or format tweak and provided
little safety beyond "did it compile."

`tests/fixtures/statusline_sample.json` is a copy of the real Claude Code stdin
payload. The integration tests don't use it yet (TODO: wire it up as a
baseline fixture with warm-cache / cold-miss variants).
