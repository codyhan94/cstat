use crate::types::Config;
use std::path::PathBuf;

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude/plugins/cstat/config.toml"))
}

pub fn load_config() -> Config {
    load_config_from_path(config_path())
}

fn load_config_from_path(path: Option<PathBuf>) -> Config {
    let path = match path {
        Some(p) => p,
        None => return Config::default(),
    };

    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Config::default(),
    };

    match toml::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cstat: invalid config: {e}");
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn load_from_str(s: &str) -> Config {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(s.as_bytes()).unwrap();
        load_config_from_path(Some(f.path().to_path_buf()))
    }

    #[test]
    fn missing_file_returns_defaults() {
        let cfg = load_config_from_path(Some(PathBuf::from("/nonexistent/config.toml")));
        assert_eq!(cfg.separator(), " │ ");
        assert!(cfg.colors());
        assert_eq!(cfg.path_levels(), 1);
    }

    #[test]
    fn no_path_returns_defaults() {
        let cfg = load_config_from_path(None);
        assert!(cfg.colors());
    }

    #[test]
    fn full_config() {
        let cfg = load_from_str(
            r#"
separator = " | "
colors = false
path_levels = 2
context_warning = 60
context_critical = 80
"#,
        );
        assert_eq!(cfg.separator(), " | ");
        assert!(!cfg.colors());
        assert_eq!(cfg.path_levels(), 2);
        assert_eq!(cfg.context_warning, Some(60));
        assert_eq!(cfg.context_critical, Some(80));
    }

    #[test]
    fn partial_config() {
        let cfg = load_from_str("colors = false\n");
        assert!(!cfg.colors());
        assert_eq!(cfg.separator(), " │ ");
        assert_eq!(cfg.path_levels(), 1);
        assert_eq!(cfg.context_warning, None);
    }

    #[test]
    fn malformed_toml_returns_defaults() {
        let cfg = load_from_str("this is not valid toml {{{}}}");
        assert!(cfg.colors());
        assert_eq!(cfg.separator(), " │ ");
    }

    #[test]
    fn empty_file_returns_defaults() {
        let cfg = load_from_str("");
        assert!(cfg.colors());
        assert_eq!(cfg.separator(), " │ ");
    }

    #[test]
    fn path_levels_clamped() {
        let cfg = load_from_str("path_levels = 5\n");
        assert_eq!(cfg.path_levels(), 3);

        let cfg = load_from_str("path_levels = 0\n");
        assert_eq!(cfg.path_levels(), 1);
    }
}
