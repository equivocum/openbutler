// openbutler-board — board server + CLI (`cmd` and `state` subcommands).
// (Replaces the legacy Python board server and shell CLIs.)
// Static UI (stage.html, media airlock) served from ../ui/board verbatim.

use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json,
};
use openbutler_common as C;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const NAME: &str = "openbutler-board";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const ORB_STATES: [&str; 4] = ["idle", "listening", "thinking", "speaking"];
const ALLOWED: [&str; 13] = [
    "add_img",
    "add_card",
    "clear",
    "reset",
    "hand",
    "give",
    "yank",
    "hover",
    "scroll_note",
    "widget",
    "explode",
    "assemble",
    "present",
];
const AIRLOCK_VERBS: [&str; 4] = ["add_img", "hand", "give", "present"];
const PROPS_EXTS: [&str; 8] = [
    ".png", ".jpg", ".jpeg", ".webp", ".gif", ".webm", ".glb", ".gltf",
];
const MAX_HEARTBEAT: usize = 262144;

struct BoardCfg {
    name: String,
    port: u16,
    orbs: Vec<Map<String, Value>>,
    state_timeout_s: f64,
}

struct Board {
    state_bytes: Vec<u8>,
    cmds: Vec<Value>,
}

struct App {
    here: PathBuf,
    state_dir: PathBuf,
    cfg: BoardCfg,
    board: Mutex<Board>,
}

// ---------- config ----------

fn str_field(obj: &Map<String, Value>, key: &str, dflt: &str) -> String {
    obj.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(dflt)
        .to_string()
}

fn load_config(path: &Path) -> BoardCfg {
    let mut user = C::read_json_object(path);
    let has_name = user.contains_key("name");
    let has_port = user.contains_key("port");
    if !has_name {
        if let Ok(v) = std::env::var("AGENT_NAME") {
            if !v.is_empty() {
                user.insert("name".into(), Value::String(v));
            }
        }
    }
    let mut orbs: Vec<Map<String, Value>> = user
        .get("orbs")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_object().cloned()).collect())
        .unwrap_or_default();
    if orbs.is_empty() {
        orbs = vec![
            ["title", "Notes", "path", "sample-notes", "kind", "notes"],
            ["title", "Props", "path", "media", "kind", "media"],
        ]
        .into_iter()
        .map(|a| {
            a.chunks(2)
                .map(|kv| (kv[0].to_string(), Value::String(kv[1].to_string())))
                .collect()
        })
        .collect();
    }
    for orb in &mut orbs {
        let p = orb.get("path").and_then(|v| v.as_str()).unwrap_or("");
        orb.insert("path".into(), Value::String(C::expand(p)));
    }
    let name = str_field(&user, "name", C::settings::DEFAULT_NAME);
    let mut port: u16 = user
        .get("port")
        .and_then(|v| v.as_u64())
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(C::settings::DEFAULT_BOARD_PORT);
    if !has_port {
        if let Ok(v) = std::env::var("HANDS_PORT") {
            if let Ok(n) = v.trim().parse::<u16>() {
                port = n;
            }
        }
    }
    let state_timeout_s = user
        .get("state_timeout_s")
        .and_then(|v| v.as_f64())
        .unwrap_or(600.0);
    BoardCfg {
        name,
        port,
        orbs,
        state_timeout_s,
    }
}

fn media_root(here: &Path, cfg: &BoardCfg) -> PathBuf {
    for orb in &cfg.orbs {
        if orb.get("kind").and_then(|v| v.as_str()) == Some("media") {
            let q = PathBuf::from(orb.get("path").and_then(|v| v.as_str()).unwrap_or("media"));
            let q = if q.is_absolute() { q } else { here.join(q) };
            if let Ok(c) = q.canonicalize() {
                return c;
            }
            return q;
        }
    }
    here.join("media")
}

/// Resolve orb index (Python list semantics incl. negative wrap) to its
/// notes jail root, or None.
fn orb_root(here: &Path, cfg: &BoardCfg, idx_raw: &str) -> Option<(i64, PathBuf)> {
    let n = cfg.orbs.len() as i64;
    let i: i64 = idx_raw.trim().parse().ok()?;
    let j = if i < 0 { n + i } else { i };
    if j < 0 || j >= n {
        return None;
    }
    let orb = &cfg.orbs[j as usize];
    if orb.get("kind").and_then(|v| v.as_str()) != Some("notes") {
        return None;
    }
    let p = PathBuf::from(orb.get("path").and_then(|v| v.as_str()).unwrap_or(""));
    let p = if p.is_absolute() { p } else { here.join(p) };
    Some((i, p.canonicalize().ok()?))
}

/// Strict parents-only containment (mirrors `root in target.parents`:
///
/// target == root does NOT count).
fn inside_parents(root: &Path, target: &Path) -> bool {
    let mut a = target.parent();
    while let Some(p) = a {
        if p == root {
            return true;
        }
        a = p.parent();
    }
    false
}

// ---------- /tree + /props walks ----------

fn sorted_entries(d: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(d)
        .map(|r| r.filter_map(|e| e.ok().map(|x| x.path())).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn walk_notes(root: &Path, idx: i64, d: &Path) -> Result<Value, ()> {
    let mut notes = Vec::new();
    let mut dirs = Vec::new();
    for p in sorted_entries(d) {
        let fname = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if fname.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            let sub = walk_notes(root, idx, &p)?;
            let has = sub
                .get("notes")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
                || sub
                    .get("dirs")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
            if has {
                dirs.push(sub);
            }
        } else if p.extension().and_then(|e| e.to_str()) == Some("md")
            && fname != "AGENTS.md"
            && fname != "CLAUDE.md"
        {
            let stem = p
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let rel = p
                .strip_prefix(root)
                .map_err(|_| ())?
                .to_string_lossy()
                .replace('\\', "/");
            notes.push(json!({"title": stem, "file": format!("{idx}/{rel}")}));
        }
    }
    let name = d
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok(json!({"name": name, "notes": notes, "dirs": dirs}))
}

fn walk_props(mroot: &Path, d: &Path) -> Value {
    let mut items = Vec::new();
    let mut dirs = Vec::new();
    for p in sorted_entries(d) {
        let fname = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if fname.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            let sub = walk_props(mroot, &p);
            let documented = p.join("README.md").is_file();
            let has = sub
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
                || sub
                    .get("dirs")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
            if has || documented {
                dirs.push(sub);
            }
        } else if p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| PROPS_EXTS.contains(&format!(".{}", e.to_ascii_lowercase()).as_str()))
            .unwrap_or(false)
        {
            let rel = p
                .strip_prefix(mroot)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| fname.clone());
            items.push(Value::String(rel));
        }
    }
    let name = d
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    json!({"name": name, "items": items, "dirs": dirs})
}

fn media_basename_heal(media: &Path, name: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![media.to_path_buf()];
    while let Some(d) = stack.pop() {
        for p in sorted_entries(&d) {
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file()
                && p.file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .as_deref()
                    == Some(name)
            {
                hits.push(p);
            }
        }
    }
    hits
}

// ---------- query parsing (first-value-wins, like parse_qs) ----------

fn query_first(uri: &str, key: &str) -> Option<String> {
    let q = uri.split('?').nth(1)?.split('#').next().unwrap_or("");
    for pair in q.split('&') {
        let (k, v) = match pair.find('=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, ""),
        };
        if k == key {
            return Some(C::percent_decode(v));
        }
    }
    None
}

// ---------- handlers ----------

fn json_nostore(v: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(v)).into_response()
}

async fn post_state(State(app): State<Arc<App>>, body: Bytes) -> Response {
    // Mirrors: body = rfile.read(n) if 0 < n < 262144 else b"{}"
    let keep = !body.is_empty() && body.len() < MAX_HEARTBEAT;
    let mut b = app.board.lock().unwrap();
    b.state_bytes = if keep { body.to_vec() } else { b"{}".to_vec() };
    let n = 8.min(b.cmds.len());
    let out: Vec<Value> = b.cmds.drain(..n).collect();
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&out).unwrap_or_default(),
    )
        .into_response()
}

async fn post_cmd(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let mut cmd: Map<String, Value> = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(m)) => m,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    if !cmd
        .get("a")
        .and_then(|v| v.as_str())
        .map(|a| ALLOWED.contains(&a))
        .unwrap_or(false)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let verb = cmd["a"].as_str().unwrap_or("").to_string();
    let has_src = cmd
        .get("src")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if AIRLOCK_VERBS.iter().any(|v| *v == verb) && has_src {
        let media = media_root(&app.here, &app.cfg);
        let mut rel = cmd["src"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();
        if let Some(stripped) = rel.strip_prefix("media/") {
            rel = stripped.to_string();
        }
        let mut ok = false;
        if let Ok(target) = (media.join(&rel)).canonicalize() {
            if inside_parents(&media, &target) && target.is_file() {
                let relp = target
                    .strip_prefix(&media)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                cmd.insert("src".into(), Value::String(format!("/media/{relp}")));
                ok = true;
            }
        }
        if !ok {
            // UNIQUE basename self-heal, else 400.
            let base = Path::new(&rel)
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let hits = if base.is_empty() {
                Vec::new()
            } else {
                media_basename_heal(&media, &base)
            };
            if hits.len() != 1 {
                return StatusCode::BAD_REQUEST.into_response();
            }
            let relp = hits[0]
                .strip_prefix(&media)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            cmd.insert("src".into(), Value::String(format!("/media/{relp}")));
        }
    }
    app.board.lock().unwrap().cmds.push(Value::Object(cmd));
    StatusCode::NO_CONTENT.into_response()
}

async fn get_config(State(app): State<Arc<App>>) -> Response {
    let orbs: Vec<Value> = app
        .cfg
        .orbs
        .iter()
        .map(|o| {
            json!({"title": o.get("title").and_then(|v| v.as_str()).unwrap_or("?"),
                   "kind": o.get("kind").and_then(|v| v.as_str()).unwrap_or("notes")})
        })
        .collect();
    json_nostore(json!({"name": app.cfg.name, "orbs": orbs}))
}

async fn get_tree(State(app): State<Arc<App>>, req: axum::extract::Request) -> Response {
    let uri = req.uri().to_string();
    let idx_raw = query_first(&uri, "orb").unwrap_or_else(|| "0".into());
    let (idx, root) = match orb_root(&app.here, &app.cfg, &idx_raw) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                json_nostore(json!({"name": "?", "notes": [], "dirs": []})),
            )
                .into_response()
        }
    };
    if !root.is_dir() {
        return (
            StatusCode::NOT_FOUND,
            json_nostore(json!({"name": "?", "notes": [], "dirs": []})),
        )
            .into_response();
    }
    match walk_notes(&root, idx, &root) {
        Ok(mut tree) => {
            let n = app.cfg.orbs.len();
            let j = if idx < 0 { n as i64 + idx } else { idx } as usize;
            if let Some(title) = app
                .cfg
                .orbs
                .get(j)
                .and_then(|o| o.get("title"))
                .and_then(|v| v.as_str())
            {
                if let Some(o) = tree.as_object_mut() {
                    o.insert("name".into(), Value::String(title.to_string()));
                }
            }
            json_nostore(tree).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json_nostore(json!({"name": "?", "notes": [], "dirs": []})),
        )
            .into_response(),
    }
}

async fn get_props(State(app): State<Arc<App>>) -> Response {
    let mroot = media_root(&app.here, &app.cfg);
    let mut tree = walk_props(&mroot, &mroot);
    if let Some(o) = tree.as_object_mut() {
        o.insert("name".into(), Value::String("Props".into()));
    }
    json_nostore(tree).into_response()
}

async fn get_orb(State(app): State<Arc<App>>) -> Response {
    let s_dir = &app.state_dir;
    let mut out = json!({"state": "idle", "mood": "green", "wave": Value::Null});
    if let Ok(s) = std::fs::read_to_string(s_dir.join("state")) {
        let s = s.trim().to_lowercase();
        if ORB_STATES.contains(&s.as_str()) {
            let age = C::mtime_age(&s_dir.join("state")).unwrap_or(f64::MAX);
            if s == "idle" || age < app.cfg.state_timeout_s {
                out["state"] = Value::String(s);
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(s_dir.join("mood.json")) {
        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&t) {
            let ts = m.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if now - ts < 45.0 {
                out["mood"] = m
                    .get("mood")
                    .cloned()
                    .unwrap_or(Value::String("green".into()));
            }
        }
    }
    if out["state"] == Value::String("speaking".into()) {
        if let Ok(t) = std::fs::read_to_string(s_dir.join("wave.json")) {
            if let Ok(Value::Object(w)) = serde_json::from_str::<Value>(&t) {
                let ts = w.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                if now - ts < 0.6 {
                    let samples: Vec<Value> = w
                        .get("samples")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().take(64).cloned().collect())
                        .unwrap_or_default();
                    out["wave"] = Value::Array(samples);
                }
            }
        }
    }
    json_nostore(out).into_response()
}

async fn get_state(State(app): State<Arc<App>>) -> Response {
    let b = app.board.lock().unwrap().state_bytes.clone();
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        b,
    )
        .into_response()
}

async fn get_note(State(app): State<Arc<App>>, req: axum::extract::Request) -> Response {
    let uri = req.uri().to_string();
    let rel = query_first(&uri, "f").unwrap_or_default();
    let (idx_raw, rel) = match rel.find('/') {
        Some(i) => (rel[..i].to_string(), rel[i + 1..].to_string()),
        None => (rel, String::new()),
    };
    let root = match orb_root(&app.here, &app.cfg, &idx_raw) {
        Some((_, r)) => r,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let target = match (root.join(&rel)).canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    if !inside_parents(&root, &target)
        || target.extension().and_then(|e| e.to_str()) != Some("md")
        || !target.is_file()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let bytes = std::fs::read(&target).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).into_owned().into_bytes();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

fn html_404() -> Response {
    let body = "<!DOCTYPE HTML PUBLIC \"-//W3C//DTD HTML 4.01//EN\"\n        \"http://www.w3.org/TR/html4/strict.dtd\">\n<html><head><meta http-equiv=\"Content-Type\" content=\"text/html;charset=utf-8\">\n<title>Error response</title></head>\n<body><h1>Error response</h1><p>Error code: 404.<p>Message: File not found.<p>Error code explanation: Nothing matches the given URI.</body></html>\n";
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/html")],
        body,
    )
        .into_response()
}

async fn static_handler(State(app): State<Arc<App>>, req: axum::extract::Request) -> Response {
    let full = req.uri().to_string();
    let path = req.uri().path().to_string();
    let is_media = path == "/media" || path.starts_with("/media/");
    // NOTE: stricter than Python on airlock escapes (404 instead of serving
    // the airlock root listing) and no directory listings. Success paths identical.
    let (root, rel): (PathBuf, String) = if is_media {
        let media = media_root(&app.here, &app.cfg);
        let rel = C::percent_decode(path.trim_start_matches("/media").trim_start_matches('/'));
        let target = match (media.join(&rel)).canonicalize() {
            Ok(p) => p,
            Err(_) => return html_404(),
        };
        if target != media && !inside_parents(&media, &target) {
            return html_404();
        }
        (media, format!("/{}", rel))
    } else {
        (app.here.clone(), path.clone())
    };
    void(&full);
    match C::resolve_static(&root, &rel) {
        C::StaticHit::File { bytes, ctype } => {
            if rel.ends_with("stage.html") || path.ends_with("stage.html") {
                (
                    [
                        (header::CONTENT_TYPE, ctype),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    bytes,
                )
                    .into_response()
            } else {
                ([(header::CONTENT_TYPE, ctype)], bytes).into_response()
            }
        }
        C::StaticHit::Missing => html_404(),
    }
}

fn void(_: &str) {}

// ---------- minimal localhost HTTP client (replaces curl in cmd/state) ----------

fn http_roundtrip(port: u16, req: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String> {
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("bad port: {e}"))?;
    let mut s = std::net::TcpStream::connect_timeout(&addr, timeout)
        .map_err(|_| format!("connect 127.0.0.1:{port} failed"))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    s.write_all(req.as_bytes())
        .map_err(|e| format!("send failed: {e}"))?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    Ok((code, buf[head_end..].to_vec()))
}

fn discover_port(cfg_path: &Path, port_flag: Option<u16>) -> u16 {
    if let Some(p) = port_flag {
        return p;
    }
    let user = C::read_json_object(cfg_path);
    if let Some(n) = user
        .get("port")
        .and_then(|v| v.as_u64())
        .and_then(|n| u16::try_from(n).ok())
    {
        return n;
    }
    if !user.contains_key("port") {
        if let Ok(v) = std::env::var("HANDS_PORT") {
            if let Ok(n) = v.trim().parse::<u16>() {
                return n;
            }
        }
    }
    C::settings::DEFAULT_BOARD_PORT
}

fn cmd_mode(cfg_path: &Path, port_flag: Option<u16>, json_arg: Option<String>) -> i32 {
    let arg = match json_arg {
        Some(a) if !a.is_empty() => a,
        _ => {
            eprintln!("usage: openbutler-board cmd <json-command>");
            return 1;
        }
    };
    let port = discover_port(cfg_path, port_flag);
    let req = format!(
        "POST /cmd HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        arg.len(),
        arg
    );
    match http_roundtrip(port, &req, Duration::from_secs(5)) {
        Ok((code, _)) => {
            println!("{code}");
            0
        }
        Err(e) => {
            eprintln!("{NAME}: {e}");
            1
        }
    }
}

fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn num_or(v: Option<&Value>, dflt: f64) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(dflt),
        _ => dflt,
    }
}

fn zone(x: f64, y: f64) -> String {
    let h = if x < 0.33 {
        "left"
    } else if x < 0.67 {
        "center"
    } else {
        "right"
    };
    let v = if y < 0.33 {
        "top"
    } else if y < 0.67 {
        "middle"
    } else {
        "bottom"
    };
    if h == "center" && v == "middle" {
        "center".into()
    } else {
        format!("{v}-{h}")
    }
}

fn basename(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or("").to_string()
}

fn state_mode(cfg_path: &Path, port_flag: Option<u16>) -> i32 {
    let port = discover_port(cfg_path, port_flag);
    let req = "GET /state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let body = match http_roundtrip(port, req, Duration::from_secs(3)) {
        Ok((_, b)) => b,
        Err(_) => {
            println!("The board is dark — the openbutler-board server isn't running.");
            return 1;
        }
    };
    let d: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let empty = match &d {
        Value::Null | Value::Bool(false) => true,
        Value::Bool(true) => false,
        Value::Number(n) => n.as_f64().map(|f| f == 0.0).unwrap_or(false),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    };
    if empty {
        println!("Server is up, but no tracker page has connected yet.");
        return 0;
    }
    let items: Vec<&Map<String, Value>> = d
        .get("items")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_object()).collect())
        .unwrap_or_default();
    if items.is_empty() {
        println!("The board is EMPTY (as of the tracker's last heartbeat).");
        return 0;
    }
    println!(
        "ON THE BOARD — {} item(s), last tracker heartbeat:",
        items.len()
    );
    for it in items {
        let t = it.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        let title = it.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let src = basename(it.get("src").and_then(|v| v.as_str()).unwrap_or(""));
        let desc = match t {
            "card" => {
                let body: String = it
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .replace('\n', " ")
                    .chars()
                    .take(70)
                    .collect();
                if body.is_empty() {
                    format!("card \"{title}\"")
                } else {
                    format!("card \"{title}\" — {body}")
                }
            }
            "img" => {
                let mut s = format!("image {src}");
                if is_truthy(it.get("fxf")) {
                    s += " (fx, frameless)";
                }
                if is_truthy(it.get("vd")) {
                    s += " (video)";
                }
                s
            }
            "model" => {
                let mode = if it.get("mm").and_then(|v| v.as_str()) == Some("holo") {
                    "hologram wireframe"
                } else {
                    "solid"
                };
                let mut s = format!("3D model {src} ({mode})");
                let ex = num_or(it.get("ex"), 0.0);
                if ex > 0.02 {
                    s += &format!(", EXPLODED {}%", (ex * 100.0).round_ties_even() as i64);
                }
                s
            }
            "panel" => format!("open note \"{title}\""),
            "browser" => format!("file browser \"{title}\""),
            "widget" => "the assistant ring".to_string(),
            "orb" => format!("orb \"{title}\""),
            _ => {
                let s = if !title.is_empty() { title } else { &src };
                format!("{t} \"{s}\"")
            }
        };
        let mut flags: Vec<&str> = Vec::new();
        if is_truthy(it.get("g")) {
            flags.push("IN THE USER'S HAND");
        }
        let sc = match it.get("scale") {
            Some(Value::Number(n)) => {
                let f = n.as_f64().unwrap_or(1.0);
                if f == 0.0 {
                    1.0
                } else {
                    f
                }
            }
            _ => 1.0,
        };
        if sc >= 1.6 {
            flags.push("blown up large");
        } else if sc <= 0.55 {
            flags.push("shrunk small");
        }
        let faded = match it.get("op") {
            None => false,
            Some(Value::Null) => false,
            Some(Value::Number(n)) => n.as_f64().map(|f| f < 0.5).unwrap_or(false),
            _ => false,
        };
        if faded {
            flags.push("faded out");
        }
        let x = match it.get("x") {
            Some(Value::Number(n)) => {
                let f = n.as_f64().unwrap_or(0.5);
                if f == 0.0 {
                    0.5
                } else {
                    f
                }
            }
            _ => 0.5,
        };
        let y = match it.get("y") {
            Some(Value::Number(n)) => {
                let f = n.as_f64().unwrap_or(0.5);
                if f == 0.0 {
                    0.5
                } else {
                    f
                }
            }
            _ => 0.5,
        };
        let mut line = format!("  - {desc} @ {}", zone(x, y));
        if !flags.is_empty() {
            line += &format!("  [{}]", flags.join(", "));
        }
        println!("{line}");
    }
    0
}

fn print_help() {
    println!("{NAME} {VERSION} (rust engine)");
    println!("  serve [--port N] [--agent-home DIR]  run the board server (default)");
    println!("  cmd [--port N] <json-command>        POST one command (prints HTTP code)");
    println!("  state [--port N]                     render /state one line per item");
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut home_flag: Option<String> = None;
    let mut port_flag: Option<u16> = None;
    let mut sub: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "-V" | "--version" => {
                println!("{NAME} {VERSION} (rust engine)");
                return;
            }
            "-h" | "--help" => {
                print_help();
                return;
            }
            "--port" => {
                if let Some(n) = raw.get(i + 1).and_then(|s| s.parse::<u16>().ok()) {
                    port_flag = Some(n);
                    i += 1;
                }
            }
            "--agent-home" => {
                if i + 1 < raw.len() {
                    home_flag = Some(raw[i + 1].clone());
                    i += 1;
                }
            }
            "cmd" | "state" | "serve" if sub.is_none() => sub = Some(raw[i].clone()),
            _ => positional.push(raw[i].clone()),
        }
        i += 1;
    }
    let agent = C::agent_home(home_flag.as_deref());
    let here = agent.join("ui/board");
    let cfg_file = agent.join("configs/board.json");
    let state_dir = agent.join("state");

    match sub.as_deref() {
        Some("cmd") => std::process::exit(cmd_mode(
            &cfg_file,
            port_flag,
            positional.into_iter().next(),
        )),
        Some("state") => std::process::exit(state_mode(&cfg_file, port_flag)),
        _ => {}
    }

    let mut cfg = load_config(&cfg_file);
    if let Some(p) = port_flag {
        cfg.port = p;
    }
    let port = cfg.port;
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        eprintln!("{NAME}: cannot create state dir: {e}");
        std::process::exit(1);
    }
    let app = Arc::new(App {
        here: here.clone(),
        state_dir,
        cfg,
        board: Mutex::new(Board {
            state_bytes: b"{}".to_vec(),
            cmds: Vec::new(),
        }),
    });
    let router: axum::Router = axum::Router::new()
        .route("/state", post(post_state).get(get_state))
        .route("/cmd", post(post_cmd))
        .route("/config", get(get_config))
        .route("/tree", get(get_tree))
        .route("/props", get(get_props))
        .route("/orb", get(get_orb))
        .route("/note", get(get_note))
        .fallback(static_handler)
        .with_state(app);
    // NOTE: no single-instance probe here — matches server.py, which binds
    // unconditionally and fails loudly on conflict.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{NAME}: cannot bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    println!("openbutler-board up: http://127.0.0.1:{port}/stage.html");
    println!("  tracker (camera): open that URL in Chrome");
    println!("  render (overlay): same URL + ?role=render");
    let _ = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
}
