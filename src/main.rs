mod config;
mod git;
mod render;
mod state;
mod stdin;
mod transcript;
mod types;

use types::{CacheStatus, CachedRateLimits, ContextWindow, State, UsageInfo};

fn reset_secs(resets_at: Option<i64>, now: i64) -> Option<i64> {
    resets_at.map(|t| t - now).filter(|&r| r > 0)
}

fn usage_from_stdin(data: &types::StdinData) -> Option<(UsageInfo, CachedRateLimits)> {
    let rl = data.rate_limits.as_ref()?;
    let usage_5h = rl.five_hour.as_ref().and_then(|w| w.used_percentage);
    let usage_7d = rl.seven_day.as_ref().and_then(|w| w.used_percentage);
    if usage_5h.is_none() && usage_7d.is_none() {
        return None;
    }
    let resets_at_5h = rl.five_hour.as_ref().and_then(|w| w.resets_at);
    let resets_at_7d = rl.seven_day.as_ref().and_then(|w| w.resets_at);
    let now = chrono::Utc::now().timestamp();
    let usage = UsageInfo {
        usage_5h,
        usage_7d,
        reset_5h: reset_secs(resets_at_5h, now),
        reset_7d: reset_secs(resets_at_7d, now),
    };
    let cache = CachedRateLimits { usage_5h, usage_7d, resets_at_5h, resets_at_7d };
    Some((usage, cache))
}

fn usage_from_cache(cached: &CachedRateLimits) -> Option<UsageInfo> {
    if cached.usage_5h.is_none() && cached.usage_7d.is_none() {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    Some(UsageInfo {
        usage_5h: cached.usage_5h,
        usage_7d: cached.usage_7d,
        reset_5h: reset_secs(cached.resets_at_5h, now),
        reset_7d: reset_secs(cached.resets_at_7d, now),
    })
}

/// Detect a new API round-trip by comparing cumulative `total_input_tokens`,
/// then update cache stamps based on `current_usage` cache token counts.
///
/// Three cases per the design doc:
/// - `cache_read > 0`            → warm hit (TTL extended)
/// - `cache_read == 0, creation > 0` → cold miss (paid full price; flag for 💸)
/// - both 0                       → no cache event (e.g. post-/compact); leave state alone
///
/// On a warm hit, the cold-miss flag is cleared if older than 30s — this keeps the 💸
/// visible through a burst of tool-call warm hits within the same response.
///
/// Cost delta is checked independently up front: `/recap` and other "invisible" turns
/// produce a cumulative cost increase but no change to `total_input_tokens` or
/// `current_usage`, which would otherwise short-circuit below. Any cost increase is
/// evidence of a recent API call that extended the cache TTL server-side, so we stamp
/// `last_cache_hit`. We use `cost_stamp_time` (the previous poll's timestamp) rather
/// than `now` to avoid overestimating TTL when cstat was suspended (e.g. during /btw):
/// the API call happened before cstat resumed, so stamping `now` would make the cache
/// appear fresher than it is. Using the prior poll time gives a slight underestimate
/// (bounded by one poll interval + API latency) instead of a potentially large overestimate.
fn update_cache_state(st: &mut State, ctx: &ContextWindow, cost_usd: Option<f64>, now: i64, cost_stamp_time: i64) {
    if let Some(curr_cost) = cost_usd {
        if let Some(prev_cost) = st.last_cost_usd {
            if curr_cost > prev_cost {
                st.last_cache_hit = Some(cost_stamp_time);
            }
        }
        st.last_cost_usd = Some(curr_cost);
    }

    let Some(curr_tokens) = ctx.total_input_tokens else {
        return;
    };

    // No prior baseline — fresh state or after a version discard. Cache fields in
    // stdin may be stale from a long-ago API call. Initialize the count and return
    // without stamping; the next real turn will produce a meaningful delta.
    let Some(prev_tokens) = st.last_total_input_tokens else {
        st.last_total_input_tokens = Some(curr_tokens);
        return;
    };

    // Token count dropped from a known baseline — context was rebuilt (e.g. /compact).
    // Clear all cache stamps: the old prefix is gone and whatever the cost-delta path
    // may have just stamped is wrong. Also clear last_cost_usd so the first real turn
    // after compact doesn't get a false warm stamp from the cost-delta path.
    if curr_tokens < prev_tokens {
        st.last_cache_hit = None;
        st.last_cache_miss = None;
        st.last_cost_usd = None;
        st.last_total_input_tokens = Some(curr_tokens);
        return;
    }

    if curr_tokens == prev_tokens {
        return;
    }
    st.last_total_input_tokens = Some(curr_tokens);

    let Some(usage) = ctx.current_usage.as_ref() else {
        return;
    };
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let cache_creation = usage.cache_creation_input_tokens.unwrap_or(0);

    if cache_read > 0 {
        st.last_cache_hit = Some(now);
        if let Some(miss_ts) = st.last_cache_miss {
            if now - miss_ts > 30 {
                st.last_cache_miss = None;
            }
        }
    } else if cache_creation > 0 {
        st.last_cache_hit = Some(now);
        st.last_cache_miss = Some(now);
    }
}

fn main() {
    let data = stdin::read_stdin();
    let config = config::load_config();
    let mut st = state::load_state(data.session_id.as_deref());
    let transcript_data = transcript::parse_transcript(data.transcript_path.as_deref(), &mut st);
    let git = git::read_git_info(data.cwd.as_deref(), st.git_index_mtime);
    let git_info = git.as_ref().map(|(info, _)| info);
    if let Some((_, mtime)) = &git {
        st.git_index_mtime = Some(*mtime);
    }
    // For branched sessions: inherit parent's cache timestamps before update_cache_state
    // runs, so the countdown continues from the parent's remaining TTL rather than
    // resetting to full TTL. Inheriting last_total_input_tokens causes the equal-tokens
    // short-circuit to fire on the first tick (which re-emits the parent's context_window
    // verbatim), preventing a spurious last_cache_hit = Some(now) stamp.
    if st.last_cache_hit.is_none() {
        if let Some(pid) = st.parent_session_id.as_deref() {
            let parent = state::load_state(Some(pid));
            if let Some(t) = parent.last_cache_hit {
                st.last_cache_hit = Some(t);
                st.last_total_input_tokens = parent.last_total_input_tokens;
            }
        }
    }
    {
        let cost_usd = data.cost.as_ref().and_then(|c| c.total_cost_usd);
        let tokens = data.context_window.as_ref().and_then(|c| c.total_input_tokens);
        let cost_changed = cost_usd.map_or(false, |c| st.last_cost_usd.map_or(true, |p| c > p));
        let tokens_changed = tokens.map_or(false, |t| st.last_total_input_tokens.map_or(true, |p| t != p));
        if cost_changed || tokens_changed {
            stdin::debug_log(&data);
        }
    }
    if let Some(ctx) = data.context_window.as_ref() {
        let cost_usd = data.cost.as_ref().and_then(|c| c.total_cost_usd);
        let now = chrono::Utc::now().timestamp();
        let cost_stamp_time = st.last_poll_time.unwrap_or(now);
        st.last_poll_time = Some(now);
        update_cache_state(&mut st, ctx, cost_usd, now, cost_stamp_time);
    }
    let usage = match usage_from_stdin(&data) {
        Some((info, cache)) => {
            st.cached_rate_limits = Some(cache);
            Some(info)
        }
        None => st
            .cached_rate_limits
            .as_ref()
            .and_then(usage_from_cache)
            .or_else(|| {
                let global = state::load_global_rate_limits()?;
                let info = usage_from_cache(&global)?;
                st.cached_rate_limits = Some(global);
                Some(info)
            }),
    };
    let cache_status = CacheStatus {
        last_cache_hit: st.last_cache_hit,
        last_cache_miss: st.last_cache_miss,
    };
    let output = render::render(&data, &config, &transcript_data, git_info, usage.as_ref(), &cache_status);
    println!("{output}");
    state::save_state(&mut st, data.session_id.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::CurrentUsage;

    fn ctx(total: u64, cache_read: u64, cache_creation: u64) -> ContextWindow {
        ContextWindow {
            used_percentage: None,
            total_input_tokens: Some(total),
            current_usage: Some(CurrentUsage {
                cache_creation_input_tokens: Some(cache_creation),
                cache_read_input_tokens: Some(cache_read),
            }),
        }
    }

    #[test]
    fn first_warm_hit_sets_stamp() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(500);
        update_cache_state(&mut st, &ctx(1000, 500, 200), None, 1_000_000, 1_000_000);
        assert_eq!(st.last_total_input_tokens, Some(1000));
        assert_eq!(st.last_cache_hit, Some(1_000_000));
        assert_eq!(st.last_cache_miss, None);
    }

    #[test]
    fn cold_miss_sets_both_stamps() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(500);
        update_cache_state(&mut st, &ctx(1000, 0, 500), None, 1_000_000, 1_000_000);
        assert_eq!(st.last_cache_hit, Some(1_000_000));
        assert_eq!(st.last_cache_miss, Some(1_000_000));
    }

    #[test]
    fn no_cache_event_leaves_stamps_alone() {
        let mut st = State::default();
        st.last_cache_hit = Some(999);
        st.last_cache_miss = Some(888);
        update_cache_state(&mut st, &ctx(1000, 0, 0), None, 1_000_000, 1_000_000);
        // Tokens still update (round-trip happened) but stamps don't.
        assert_eq!(st.last_total_input_tokens, Some(1000));
        assert_eq!(st.last_cache_hit, Some(999));
        assert_eq!(st.last_cache_miss, Some(888));
    }

    #[test]
    fn same_tokens_short_circuits() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(1000);
        st.last_cache_hit = Some(500);
        update_cache_state(&mut st, &ctx(1000, 999, 0), None, 9_999_999, 9_999_999);
        // No update because total_input_tokens didn't change.
        assert_eq!(st.last_cache_hit, Some(500));
    }

    #[test]
    fn warm_hit_clears_stale_miss() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(900);
        st.last_cache_miss = Some(1_000_000);
        // 31s later, a warm hit
        update_cache_state(&mut st, &ctx(1100, 500, 0), None, 1_000_031, 1_000_031);
        assert_eq!(st.last_cache_miss, None);
        assert_eq!(st.last_cache_hit, Some(1_000_031));
    }

    #[test]
    fn warm_hit_preserves_recent_miss() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(900);
        st.last_cache_miss = Some(1_000_000);
        // 20s later, a warm hit (within the 30s window)
        update_cache_state(&mut st, &ctx(1100, 500, 0), None, 1_000_020, 1_000_020);
        assert_eq!(st.last_cache_miss, Some(1_000_000));
        assert_eq!(st.last_cache_hit, Some(1_000_020));
    }

    #[test]
    fn cold_miss_overwrites_existing_miss() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(900);
        st.last_cache_miss = Some(1_000_000);
        update_cache_state(&mut st, &ctx(1100, 0, 500), None, 1_000_500, 1_000_500);
        assert_eq!(st.last_cache_miss, Some(1_000_500));
    }

    #[test]
    fn missing_total_tokens_skips_update() {
        let mut st = State::default();
        let ctx = ContextWindow {
            used_percentage: Some(45),
            total_input_tokens: None,
            current_usage: None,
        };
        update_cache_state(&mut st, &ctx, None, 1_000_000, 1_000_000);
        assert_eq!(st.last_total_input_tokens, None);
        assert_eq!(st.last_cache_hit, None);
    }

    #[test]
    fn cost_delta_with_unchanged_tokens_stamps_hit() {
        // Simulates /recap: tokens and current_usage frozen from the prior real
        // turn, but cumulative cost increased. We should treat that as a cache hit,
        // stamped at cost_stamp_time (the prior poll), not at now.
        let mut st = State::default();
        st.last_total_input_tokens = Some(1000);
        st.last_cost_usd = Some(3.05);
        st.last_cache_hit = Some(500);
        // cost_stamp_time (999_990) < now (1_000_000): simulates prior-poll timestamp
        update_cache_state(&mut st, &ctx(1000, 999, 0), Some(3.10), 1_000_000, 999_990);
        assert_eq!(st.last_cache_hit, Some(999_990));
        assert_eq!(st.last_cost_usd, Some(3.10));
    }

    #[test]
    fn first_poll_initializes_cost_without_stamping() {
        // First time we see cost, we have no baseline — don't stamp from cost path.
        // (Token path may still stamp; isolate it by passing 0/0 cache fields.)
        let mut st = State::default();
        update_cache_state(&mut st, &ctx(1000, 0, 0), Some(3.05), 1_000_000, 1_000_000);
        assert_eq!(st.last_cost_usd, Some(3.05));
        assert_eq!(st.last_cache_hit, None);
    }

    #[test]
    fn no_baseline_skips_cache_stamp() {
        // Stale cache_read in stdin on fresh state must not produce a warm stamp.
        let mut st = State::default();
        update_cache_state(&mut st, &ctx(1000, 500, 0), None, 1_000_000, 1_000_000);
        assert_eq!(st.last_total_input_tokens, Some(1000));
        assert_eq!(st.last_cache_hit, None);
    }

    #[test]
    fn unchanged_cost_does_not_stamp() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(1000);
        st.last_cost_usd = Some(3.05);
        st.last_cache_hit = Some(500);
        update_cache_state(&mut st, &ctx(1000, 999, 0), Some(3.05), 1_000_000, 1_000_000);
        assert_eq!(st.last_cache_hit, Some(500));
    }

    #[test]
    fn token_drop_clears_cache_stamps() {
        // Simulates /compact: cost bumps (compact API call), then tokens drop
        // because the new context is just the compact summary. Any warm stamp
        // from the cost-delta path should be cleared.
        let mut st = State::default();
        st.last_total_input_tokens = Some(100_000);
        st.last_cost_usd = Some(5.00);
        st.last_cache_hit = Some(999);
        st.last_cache_miss = Some(888);
        // Cost increased (compact API call) AND tokens dropped to compact summary size.
        update_cache_state(&mut st, &ctx(12_000, 0, 0), Some(5.80), 1_000_000, 1_000_000);
        assert_eq!(st.last_cache_hit, None);
        assert_eq!(st.last_cache_miss, None);
        assert_eq!(st.last_cost_usd, None);
        assert_eq!(st.last_total_input_tokens, Some(12_000));
    }

    #[test]
    fn token_drop_without_prior_tokens_does_not_clear() {
        // No saved token baseline → can't tell if this is a drop, and can't trust
        // cache fields in stdin (they may be stale). Initialize the count and bail.
        let mut st = State::default();
        st.last_cache_hit = Some(999);
        update_cache_state(&mut st, &ctx(12_000, 500, 0), None, 1_000_000, 1_000_000);
        assert_eq!(st.last_total_input_tokens, Some(12_000));
        assert_eq!(st.last_cache_hit, Some(999)); // unchanged — no stamping without a baseline
    }

    #[test]
    fn branch_inherited_tokens_prevent_spurious_restamp() {
        // Simulates a branched session's first cstat tick: last_cache_hit and
        // last_total_input_tokens were inherited from the parent (same token count
        // as what Claude Code re-emits on the first branch invocation). The
        // equal-tokens short-circuit should fire, leaving last_cache_hit untouched.
        let mut st = State::default();
        st.last_cache_hit = Some(1_000_000); // inherited from parent
        st.last_total_input_tokens = Some(38383); // inherited from parent
        // First branch tick: same tokens as parent (verbatim re-emit), warm cache
        update_cache_state(&mut st, &ctx(38383, 27483, 10893), None, 9_999_999, 9_999_999);
        // Must NOT restamp to 9_999_999 — short-circuit should have fired
        assert_eq!(st.last_cache_hit, Some(1_000_000));
        assert_eq!(st.last_total_input_tokens, Some(38383));
    }

    #[test]
    fn branch_first_real_api_call_stamps_normally() {
        // After the branch's first real API call, tokens differ from inherited baseline.
        // Normal warm-hit stamping should resume.
        let mut st = State::default();
        st.last_cache_hit = Some(1_000_000); // inherited from parent
        st.last_total_input_tokens = Some(38383); // inherited from parent
        // First real branch API call: new tokens
        update_cache_state(&mut st, &ctx(39000, 38376, 615), None, 2_000_000, 2_000_000);
        assert_eq!(st.last_cache_hit, Some(2_000_000));
        assert_eq!(st.last_total_input_tokens, Some(39000));
    }
}
