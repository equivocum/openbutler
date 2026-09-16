// openbutler-common — shared helpers for the OpenButler engine.
// Agent-home discovery, JSON config + key-presence overlay, $VAR/~
// expansion, traversal-safe static file resolution, minimal mime table.

use std::path::{Path, PathBuf};

/// Shared settings core (registry, validation, file routing, reads/writes)
/// for every config CLI.
pub mod settings;

/// Locate the agent home: `--agent-home` value > $AGENT_HOME > walk up from
/// the executable (or cwd) to the dir containing Cargo.toml (workspace root).
pub fn agent_home(cli: Option<&str>) -> PathBuf {
    if let Some(a) = cli {
        let p = PathBuf::from(expand(a));
        if p.is_dir() {
            return p;
        }
    }
    if let Ok(h) = std::env::var("AGENT_HOME") {
        let p = PathBuf::from(h);
        if p.is_dir() {
            return p;
        }
    }
    for start in [std::env::current_exe().ok(), std::env::current_dir().ok()]
        .into_iter()
        .flatten()
    {
        let mut d = Some(start.as_path());
        while let Some(dir) = d {
            if dir.join("Cargo.toml").is_file() && dir.join("crates").is_dir() {
                return dir.to_path_buf();
            }
            d = dir.parent();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Expand leading `~` and `$VAR` / `${VAR}` references (mirrors
/// os.path.expanduser + os.path.expandvars as used by both servers).
pub fn expand(s: &str) -> String {
    let mut out = s.to_string();
    if out == "~" || out.starts_with("~/") {
        if let Ok(h) = std::env::var("HOME") {
            out = format!("{h}{}", &out[1..]);
        }
    }
    let mut res = String::with_capacity(out.len());
    let b = out.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && i + 1 < b.len() {
            let (name, next) = if b[i + 1] == b'{' {
                match out[i + 2..].find('}') {
                    Some(e) => (out[i + 2..i + 2 + e].to_string(), i + 3 + e),
                    None => ("".to_string(), i + 1),
                }
            } else {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                (out[i + 1..j].to_string(), j)
            };
            if !name.is_empty() {
                res.push_str(&std::env::var(&name).unwrap_or_default());
            } else {
                res.push('$');
            }
            i = next;
        } else {
            res.push(b[i] as char);
            i += 1;
        }
    }
    res
}

/// Read a JSON object file; missing/unreadable/invalid -> empty object.
/// (Both Python servers treat a missing config as defaults.)
pub fn read_json_object(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Key-presence check for the JSON > env > defaults overlay.
pub fn json_has_key(path: &Path, key: &str) -> bool {
    read_json_object(path).contains_key(key)
}

/// Strict shape check: the file must exist and parse as a JSON object.
/// Lenient readers (`read_json_object`) fold missing AND broken into empty;
/// repair paths use this to tell the two apart.
pub fn is_json_object(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(t) => matches!(
            serde_json::from_str::<serde_json::Value>(&t),
            Ok(serde_json::Value::Object(_))
        ),
        Err(_) => false,
    }
}

/// Minimal mime table covering everything the face/board static trees serve.
/// (Python uses mimetypes.guess_type; these are the types it yields for our
/// extensions on a standard Linux install.)
pub fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "wav" => "audio/x-wav", // match Python mimetypes on Linux
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "webm" => "video/webm",
        "mp4" => "video/mp4",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain",
        "glb" => "model/gltf-binary",
        "gltf" => "model/gltf+json",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Decode %XX sequences in a URL path (leaves `+` alone: paths, not queries).
pub fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve a request path under `root` with symlink-aware containment.
/// `/` -> `/index.html`; a directory -> its `index.html`.
/// Mirrors `(HERE / path).resolve()` + `HERE in target.parents` from server.py.
pub enum StaticHit {
    File { bytes: Vec<u8>, ctype: &'static str },
    Missing,
}

pub fn resolve_static(root: &Path, req_path: &str) -> StaticHit {
    let clean = req_path.split('?').next().unwrap_or(req_path);
    let clean = clean.split('#').next().unwrap_or(clean);
    let rel = if clean == "/" { "/index.html" } else { clean };
    let canon_root = match root.canonicalize() {
        Ok(p) => p,
        Err(_) => return StaticHit::Missing,
    };
    let target = canon_root.join(percent_decode(rel).trim_start_matches('/'));
    // Resolve symlinks *before* the containment check (same as .resolve()).
    let target = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // Fall through for a possibly-missing leaf under an existing dir:
            // canonicalize the parent instead so `is_file` below decides.
            match target.parent().and_then(|p| p.canonicalize().ok()) {
                Some(p) => p.join(target.file_name().unwrap_or_default()),
                None => return StaticHit::Missing,
            }
        }
    };
    if target != canon_root && !target.starts_with(&canon_root) {
        return StaticHit::Missing;
    }
    let file = if target.is_dir() {
        target.join("index.html")
    } else {
        target
    };
    if !file.is_file() {
        return StaticHit::Missing;
    }
    // Re-check containment for the dir -> index.html join.
    if let Ok(c) = file.canonicalize() {
        if c != canon_root && !c.starts_with(&canon_root) {
            return StaticHit::Missing;
        }
        let ctype = mime_for(&c);
        match std::fs::read(&c) {
            Ok(bytes) => StaticHit::File { bytes, ctype },
            Err(_) => StaticHit::Missing,
        }
    } else {
        StaticHit::Missing
    }
}

/// Python `str.title()` approximation for single-word face ids.
pub fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Seconds since mtime; None if stat fails.
pub fn mtime_age(path: &Path) -> Option<f64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    std::time::SystemTime::now()
        .duration_since(m)
        .ok()
        .map(|d| d.as_secs_f64())
}

/// Poison-tolerant mutex lock for audio threads: a caught backend panic
/// must degrade the stream, never wedge it forever on a poisoned mutex.
pub fn ml<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
