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

`STATE_VERSION = 5`. On version mismatch, state is discarded and rebuilt from
scratch on the next transcript parse. Bincode 1 is positional (not
self-describing), so any field addition requires a version bump — a mismatch
lands in `State::default()` silently.

**State fields** (non-obvious ones):

| Field | Type | Purpose |
|-------|------|---------|
| `last_total_input_tokens` | `Option<u64>` | Baseline for turn detection; `None` means "no prior data, don't trust stdin cache fields" |
| `last_cache_hit` | `Option<i64>` | Unix timestamp of last confirmed cache hit; `None` means "unknown/cold" |
| `last_cache_miss` | `Option<i64>` | Unix timestamp of last cold miss; cleared 30s after a warm hit |
| `last_cost_usd` | `Option<f64>` | Prior poll's cumulative session cost; used to detect invisible API calls |
| `last_poll_time` | `Option<i64>` | Unix timestamp of the prior cstat invocation; used as `cost_stamp_time` |
| `parent_session_id` | `Option<String>` | Extracted from `forkedFrom.sessionId` in transcript; enables branch inheritance |

A separate global file (`cstat-rate-limits.bin`) persists the last-known rate
limit values. This lets a brand-new session show usage immediately without
waiting for the first `rate_limits` key to appear in stdin.

---

## Cache TTL detection

There is no hook script. Cache state is inferred from the `context_window`
field in stdin on every invocation. The state machine lives in
`main.rs::update_cache_state`.

### Fresh-state guard

When `last_total_input_tokens` is `None` (fresh state, or after a version
discard), the function **initializes the baseline and returns immediately**
without inspecting `current_usage`. Stale `cache_read` values in stdin from a
long-ago API call must not produce a false warm stamp. The next real turn will
have a genuine token delta and be processed normally.

### Cost-delta path (invisible turns)

`/recap` and other "invisible" API calls increase the session's cumulative
cost but don't change `total_input_tokens` or populate `current_usage`. To
detect these, `update_cache_state` checks cost **before** the token section:
any cost increase is evidence of a recent API call that extended the cache TTL
server-side, so `last_cache_hit` is stamped (extended, not created).

The stamp uses `cost_stamp_time` (the prior poll's timestamp, from
`last_poll_time`) rather than `now`. This avoids overestimating TTL when cstat
was suspended — e.g. during `/btw`, all queued invocations fire at Esc. Using
`now` would stamp Esc-time and make the cache appear fresher than it is.
`last_poll_time` gives a slight underestimate bounded by one poll interval +
API latency.

### Token-drop detection (/compact)

If `total_input_tokens` drops from a known baseline (`curr_tokens <
prev_tokens`), the context was rebuilt (e.g. `/compact`). All cache stamps are
cleared, including `last_cost_usd`. Clearing cost prevents the cost-delta path
from producing a false warm stamp on the next real turn (the compact's own API
call increases cost but the old cache prefix is gone). The first real
post-compact turn will correctly stamp via the token path.

### Branch inheritance

When `parent_session_id` is set and `last_cache_hit` is `None`, cstat loads
the parent session's state and copies `last_cache_hit` and
`last_total_input_tokens`. This serves two purposes:

1. **TTL continuity**: the cache countdown continues from the parent's
   remaining TTL rather than resetting to full TTL on the first tick.
2. **Spurious-restamp prevention**: inheriting `last_total_input_tokens`
   causes the equal-tokens short-circuit to fire on the first tick (which
   re-emits the parent's `context_window` verbatim), preventing a spurious
   `last_cache_hit = Some(now)` stamp.

**Caveat**: this assumes the fork uses the same AI provider as the parent
(shared prompt cache prefix). Cross-provider forks will briefly show the
parent's cache state until the fork's first API response corrects it (~10s
window).

### Turn-detection summary

| Condition | Action |
|-----------|--------|
| `last_total_input_tokens` is `None` | Initialize baseline, return (no stamp) |
| `curr_tokens < prev_tokens` | Clear all stamps + cost baseline (/compact) |
| `curr_tokens == prev_tokens` | Short-circuit, return (no new turn) |
| `curr_tokens > prev_tokens`, `cache_read > 0` | Warm hit: stamp `last_cache_hit = now` |
| `curr_tokens > prev_tokens`, `cache_creation > 0` | Cold miss: stamp both `last_cache_hit` and `last_cache_miss = now` |
| `curr_tokens > prev_tokens`, both 0 | No cache event (post-/compact, N/A): leave stamps alone |

The cost-delta path runs **before** the token section. It extends
`last_cache_hit` on any cost increase, but only if a prior stamp exists (it
does not create one from scratch). On a genuine token change, the token path
overwrites the cost-delta stamp with the correct value.

**TTL is 280s** (not 300) to account for ~20s lag between API response receipt
and statusline invocation.

**Rendered glyph**:
- Within 30s of a cold miss: `$` (same column width as `⧖` — prevents layout
  shift mid-statusline)
- Otherwise: `⧖ m:ss` (time since last warm hit), or `⧖ cold` if no hit stamp

Color tiers: green > 180s remaining, yellow > 60s, red ≤ 60s.

### Known pitfall: stale `last_cache_hit` in old state files

Prior to the fresh-state guard, `update_cache_state` used `unwrap_or(0)` on
`last_total_input_tokens`, conflating `None` (no baseline) with `Some(0)`.
Stale `cache_read > 0` in stdin produced a false warm stamp on every session
resume. State files written by those old binaries may carry a false
`last_cache_hit` that survives until the next real API call overwrites it, or
until the file is deleted (`rm /Volumes/ramdisk/cstat-*.bin`).

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
