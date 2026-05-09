use std::fs;
use std::path::{Path, PathBuf};

use crate::types::{CachedRateLimits, State};

const STATE_VERSION: u32 = 3;
const RAMDISK: &str = "/Volumes/ramdisk";
const FALLBACK: &str = "/tmp";

fn state_dir() -> &'static str {
    if Path::new(RAMDISK).is_dir() {
        RAMDISK
    } else {
        FALLBACK
    }
}

fn global_rate_limits_path() -> PathBuf {
    PathBuf::from(format!("{}/cstat-rate-limits.bin", state_dir()))
}

fn sanitize(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

fn state_path(session_id: &str) -> PathBuf {
    let safe = sanitize(session_id);
    PathBuf::from(format!("{}/cstat-{safe}.bin", state_dir()))
}

pub fn load_state(session_id: Option<&str>) -> State {
    let Some(sid) = session_id else {
        return State::default();
    };
    if sanitize(sid).is_empty() {
        return State::default();
    }

    let path = state_path(sid);
    let Ok(data) = fs::read(&path) else {
        return State::default();
    };

    match bincode::deserialize::<State>(&data) {
        Ok(s) if s.version == STATE_VERSION => s,
        _ => State::default(),
    }
}

pub fn save_state(state: &mut State, session_id: Option<&str>) {
    if let Some(ref cached) = state.cached_rate_limits {
        save_global_rate_limits(cached);
    }

    let Some(sid) = session_id else {
        return;
    };
    if sanitize(sid).is_empty() {
        return;
    }

    let path = state_path(sid);
    state.version = STATE_VERSION;

    if let Ok(data) = bincode::serialize(&state) {
        let _ = fs::write(&path, data);
    }
}

pub fn load_global_rate_limits() -> Option<CachedRateLimits> {
    let data = fs::read(global_rate_limits_path()).ok()?;
    bincode::deserialize(&data).ok()
}

fn save_global_rate_limits(cached: &CachedRateLimits) {
    if let Ok(data) = bincode::serialize(cached) {
        let _ = fs::write(global_rate_limits_path(), data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_path_deterministic() {
        let a = state_path("abc-123-def");
        let b = state_path("abc-123-def");
        assert_eq!(a, b);
    }

    #[test]
    fn state_path_differs_for_different_inputs() {
        let a = state_path("session-a");
        let b = state_path("session-b");
        assert_ne!(a, b);
    }

    #[test]
    fn empty_session_id_returns_default() {
        let s = load_state(None);
        assert_eq!(s.version, 0);
        assert_eq!(s.byte_offset, 0);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let sid = format!("test-roundtrip-{}", std::process::id());

        let mut state = State::default();
        state.byte_offset = 42;
        state.inode = 123;
        state.last_cache_hit = Some(1700000000);
        state.last_cache_miss = Some(1700000005);
        state.last_total_input_tokens = Some(98765);
        save_state(&mut state, Some(&sid));

        let loaded = load_state(Some(&sid));
        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.byte_offset, 42);
        assert_eq!(loaded.inode, 123);
        assert_eq!(loaded.last_cache_hit, Some(1700000000));
        assert_eq!(loaded.last_cache_miss, Some(1700000005));
        assert_eq!(loaded.last_total_input_tokens, Some(98765));

        let _ = fs::remove_file(state_path(&sid));
    }

    #[test]
    fn incompatible_version_discarded() {
        let sid = format!("test-version-{}", std::process::id());

        let mut state = State::default();
        state.version = 999;
        state.byte_offset = 100;
        let data = bincode::serialize(&state).unwrap();
        let path = state_path(&sid);
        fs::write(&path, data).unwrap();

        let loaded = load_state(Some(&sid));
        assert_eq!(loaded.version, 0);
        assert_eq!(loaded.byte_offset, 0);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupt_data_returns_default() {
        let sid = format!("test-corrupt-{}", std::process::id());

        let path = state_path(&sid);
        fs::write(&path, b"garbage").unwrap();

        let loaded = load_state(Some(&sid));
        assert_eq!(loaded.version, 0);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn session_id_with_special_chars_sanitized() {
        let p = state_path("../../../etc/passwd");
        let s = p.to_string_lossy();
        let prefix = format!("{}/cstat-", state_dir());
        assert!(s.starts_with(&prefix));
        let suffix = &s[prefix.len()..];
        // No slashes injected from the session_id portion; only `.bin` extension.
        assert!(!suffix.contains('/'));
        assert!(suffix.ends_with(".bin"));
        assert_eq!(suffix.matches('.').count(), 1);
    }

    #[test]
    fn empty_string_session_id_returns_default() {
        let s = load_state(Some(""));
        assert_eq!(s.version, 0);
    }
}
