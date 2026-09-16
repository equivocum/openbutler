// .env read/write. Format: KEY="value" / KEY=value lines, `#` comments,
// one computed line (AGENT_HOME) that is never hand-set. Writes
// regenerate from .env.example with substitutions so new template keys
// arrive automatically; unknown existing keys are preserved at the end.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Template order for known keys (AGENT_HOME stays computed).
pub const KNOWN_KEYS: &[&str] = &[
    "AGENT_NAME",
    "MEMORY_VAULT",
    "VOICE_CONTAINER",
    "FACE_PORT",
    "HANDS_PORT",
    "FACE_NAME",
    "VOICE_NAME",
    "STT_MODEL",
    "GREETING",
    "UPSTREAM_ORG",
    "COPYRIGHT_HOLDER",
];

pub fn env_path(home: &Path) -> PathBuf {
    home.join(".env")
}

/// Parse KEY=VALUE lines (quotes stripped); comments/blank skipped.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (k, v) = line.split_once('=').unwrap();
        let k = k.trim().to_string();
        if k.is_empty() || k.contains(char::is_whitespace) {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(v);
        m.insert(k, v.to_string());
    }
    m
}

pub fn load(home: &Path) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = std::fs::read_to_string(env_path(home))
        .map(|t| parse(&t))
        .unwrap_or_default();
    // The AGENT_HOME line in .env is shell code, not a value (bash computes
    // it on source). Resolve it here so render/check see the real path.
    m.insert("AGENT_HOME".into(), home.to_string_lossy().into_owned());
    m
}

/// Render a fresh .env: template header + computed AGENT_HOME line +
/// known keys in template order + preserved unknown keys.
pub fn render(home: &Path, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::from(
        "# Agent home settings — LOCAL ONLY (gitignored, never commit).\n\
          # Written by `openbutler-setup init`. Template: see `.env.example`.\n\
         # No secrets here, ever: keys live in the OS keychain / provider auth.\n\n\
         # Folder containing this file == the agent home.\n\
         AGENT_HOME=\"$(cd \"$(dirname \"${BASH_SOURCE[0]:-$0}\")\" && pwd)\"\n\n",
    );
    for k in KNOWN_KEYS {
        if let Some(v) = vars.get(*k) {
            out.push_str(&format!("{k}=\"{v}\"\n"));
        }
    }
    for (k, v) in vars {
        if !KNOWN_KEYS.contains(&k.as_str()) && k != "AGENT_HOME" {
            out.push_str(&format!("{k}=\"{v}\"\n"));
        }
    }
    let _ = home;
    out
}

pub fn write(home: &Path, vars: &BTreeMap<String, String>) -> Result<(), String> {
    std::fs::write(env_path(home), render(home, vars)).map_err(|e| format!("write .env: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_and_unquotes() {
        let m = parse("# c\nAGENT_NAME=\"Assistant\"\nPORT=8790\nBAD LINE\n");
        assert_eq!(m.get("AGENT_NAME").map(|s| s.as_str()), Some("Assistant"));
        assert_eq!(m.get("PORT").map(|s| s.as_str()), Some("8790"));
        assert!(!m.contains_key("BAD"));
    }

    #[test]
    fn render_keeps_computed_home_and_order() {
        let mut v = BTreeMap::new();
        v.insert("AGENT_NAME".into(), "Butler".into());
        v.insert("CUSTOM_X".into(), "1".into());
        let t = render(Path::new("/x"), &v);
        assert!(t.contains("AGENT_HOME=\"$(cd"));
        assert!(t.find("AGENT_NAME").unwrap() < t.find("CUSTOM_X").unwrap());
    }

    /* The human template must agree with the const single-home, or fresh
    installs seeded by hand drift from every other consumer. */
    #[test]
    fn template_matches_shared_defaults() {
        use openbutler_common::settings as S;
        let t = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env.example"),
        )
        .unwrap();
        let m = parse(&t);
        let get = |k: &str| m.get(k).cloned().unwrap_or_default();
        assert_eq!(get("AGENT_NAME"), S::DEFAULT_NAME);
        assert_eq!(get("VOICE_CONTAINER"), S::DEFAULT_VOICE_CONTAINER);
        assert_eq!(get("FACE_PORT"), S::DEFAULT_FACE_PORT.to_string());
        assert_eq!(get("HANDS_PORT"), S::DEFAULT_BOARD_PORT.to_string());
        assert_eq!(get("FACE_NAME"), S::DEFAULT_FACE_ID);
        assert_eq!(get("VOICE_NAME"), S::DEFAULT_VOICE_NAME);
        assert_eq!(get("STT_MODEL"), S::DEFAULT_STT_MODEL);
        assert_eq!(get("GREETING"), S::DEFAULT_GREETING);
        assert_eq!(get("UPSTREAM_ORG"), S::DEFAULT_UPSTREAM_ORG);
        assert_eq!(get("COPYRIGHT_HOLDER"), S::DEFAULT_COPYRIGHT_HOLDER);
        assert_eq!(
            openbutler_common::expand(&get("MEMORY_VAULT")),
            S::default_memory_vault()
        );
    }
}
