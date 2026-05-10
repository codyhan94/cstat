use crate::types::StdinData;
use std::io::Read;

pub fn read_stdin() -> StdinData {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
        return StdinData::default();
    }
    serde_json::from_str(&buf).unwrap_or_default()
}

pub fn debug_log(data: &StdinData) {
    use std::io::Write;
    let session_id = data.session_id.as_deref().unwrap_or("");
    let safe_id: String = session_id.chars().filter(|c| c.is_alphanumeric() || *c == '-').take(64).collect();
    let name = if safe_id.is_empty() { "unknown".to_string() } else { safe_id };

    let cw = data.context_window.as_ref();
    let cu = cw.and_then(|c| c.current_usage.as_ref());
    let tokens       = cw.and_then(|c| c.total_input_tokens).unwrap_or(0);
    let ctx_pct      = cw.and_then(|c| c.used_percentage).unwrap_or(0);
    let cache_read   = cu.and_then(|u| u.cache_read_input_tokens).unwrap_or(0);
    let cache_create = cu.and_then(|u| u.cache_creation_input_tokens).unwrap_or(0);
    let session_name = data.session_name.as_deref().unwrap_or("?");
    let cost_usd     = data.cost.as_ref().and_then(|c| c.total_cost_usd).unwrap_or(0.0);

    let log_path = format!("/Volumes/ramdisk/cstat-debug-{name}.log");
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) else { return };

    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = (secs % 86400 / 3600, secs % 3600 / 60, secs % 60);

    let _ = writeln!(f, "{h:02}:{m:02}:{s:02}Z | session=\"{session_name}\" | tokens={tokens} ctx={ctx_pct}% | cache_read={cache_read} cache_create={cache_create} | cost=${cost_usd:.4}");
}
