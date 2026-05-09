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
fn update_cache_state(st: &mut State, ctx: &ContextWindow, now: i64) {
    let Some(curr_tokens) = ctx.total_input_tokens else {
        return;
    };
    let prev_tokens = st.last_total_input_tokens.unwrap_or(0);
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
    if let Some(ctx) = data.context_window.as_ref() {
        update_cache_state(&mut st, ctx, chrono::Utc::now().timestamp());
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
        update_cache_state(&mut st, &ctx(1000, 500, 200), 1_000_000);
        assert_eq!(st.last_total_input_tokens, Some(1000));
        assert_eq!(st.last_cache_hit, Some(1_000_000));
        assert_eq!(st.last_cache_miss, None);
    }

    #[test]
    fn cold_miss_sets_both_stamps() {
        let mut st = State::default();
        update_cache_state(&mut st, &ctx(1000, 0, 500), 1_000_000);
        assert_eq!(st.last_cache_hit, Some(1_000_000));
        assert_eq!(st.last_cache_miss, Some(1_000_000));
    }

    #[test]
    fn no_cache_event_leaves_stamps_alone() {
        let mut st = State::default();
        st.last_cache_hit = Some(999);
        st.last_cache_miss = Some(888);
        update_cache_state(&mut st, &ctx(1000, 0, 0), 1_000_000);
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
        update_cache_state(&mut st, &ctx(1000, 999, 0), 9_999_999);
        // No update because total_input_tokens didn't change.
        assert_eq!(st.last_cache_hit, Some(500));
    }

    #[test]
    fn warm_hit_clears_stale_miss() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(900);
        st.last_cache_miss = Some(1_000_000);
        // 31s later, a warm hit
        update_cache_state(&mut st, &ctx(1100, 500, 0), 1_000_031);
        assert_eq!(st.last_cache_miss, None);
        assert_eq!(st.last_cache_hit, Some(1_000_031));
    }

    #[test]
    fn warm_hit_preserves_recent_miss() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(900);
        st.last_cache_miss = Some(1_000_000);
        // 20s later, a warm hit (within the 30s window)
        update_cache_state(&mut st, &ctx(1100, 500, 0), 1_000_020);
        assert_eq!(st.last_cache_miss, Some(1_000_000));
        assert_eq!(st.last_cache_hit, Some(1_000_020));
    }

    #[test]
    fn cold_miss_overwrites_existing_miss() {
        let mut st = State::default();
        st.last_total_input_tokens = Some(900);
        st.last_cache_miss = Some(1_000_000);
        update_cache_state(&mut st, &ctx(1100, 0, 500), 1_000_500);
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
        update_cache_state(&mut st, &ctx, 1_000_000);
        assert_eq!(st.last_total_input_tokens, None);
        assert_eq!(st.last_cache_hit, None);
    }
}
