// openbutler-face — face server. Serves ui/face (faces/* + core.js +
// Serves faces/* + core.js + assets byte-identical; JSON shapes and the
// file-bus protocol are preserved exactly. Linux-only (xdg-open).

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use openbutler_common as C;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const NAME: &str = "openbutler-face";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const STATES: [&str; 4] = ["idle", "listening", "thinking", "speaking"];
const WAVEFORM_STALE_S: f64 = 0.6;

struct FaceCfg {
    name: String,
    badge: String,
    face: String,
    port: u16,
    bus: PathBuf,
    thinking_sound: bool,
}

struct App {
    here: PathBuf,
    cfg: FaceCfg,
    mock: Option<String>,
}

fn str_field(obj: &Map<String, Value>, key: &str, dflt: &str) -> String {
    obj.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(dflt)
        .to_string()
}

fn load_config(path: &Path, home: &Path) -> FaceCfg {
    let user = C::read_json_object(path);
    let has = |k: &str| user.contains_key(k);
    let mut name = str_field(&user, "name", "Assistant");
    let mut face = str_field(&user, "face", "board");
    let mut port: u16 = user
        .get("port")
        .and_then(|v| v.as_u64())
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(8790);
    if !has("name") {
        if let Ok(v) = std::env::var("AGENT_NAME") {
            if !v.is_empty() {
                name = v;
            }
        }
    }
    if !has("face") {
        if let Ok(v) = std::env::var("FACE_NAME") {
            if !v.is_empty() {
                face = v;
            }
        }
    }
    if !has("port") {
        if let Ok(v) = std::env::var("FACE_PORT") {
            if let Ok(n) = v.trim().parse::<u16>() {
                port = n;
            }
        }
    }
    let badge = str_field(&user, "badge", "");
    let thinking_sound = user
        .get("thinking_sound")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let bus = match user.get("bus_dir").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => {
            let e = C::expand(s);
            let p = PathBuf::from(&e);
            if p.is_absolute() {
                p
            } else {
                home.join(p)
            }
        }
        _ => home.join("bus"),
    };
    FaceCfg {
        name,
        badge,
        face,
        port,
        bus,
        thinking_sound,
    }
}

fn list_faces(here: &Path) -> Vec<Value> {
    let mut faces = Vec::new();
    let fdir = here.join("faces");
    let entries = match std::fs::read_dir(&fdir) {
        Ok(r) => r,
        Err(_) => return faces,
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.is_dir() && p.join("index.html").is_file())
        .collect();
    dirs.sort();
    for p in dirs {
        let id = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut meta = Map::new();
        meta.insert("id".into(), Value::String(id.clone()));
        meta.insert("title".into(), Value::String(C::title_case(&id)));
        meta.insert("tagline".into(), Value::String(String::new()));
        if let Ok(t) = std::fs::read_to_string(p.join("face.json")) {
            if let Ok(Value::Object(extra)) = serde_json::from_str(&t) {
                for (k, v) in extra {
                    meta.insert(k, v);
                }
            }
        }
        meta.insert("id".into(), Value::String(id));
        faces.push(Value::Object(meta));
    }
    faces
}

fn num_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn mock_bus(mock: &str) -> Value {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let (level, samples) = if mock == "speaking" {
        let lvl = (t * 6.0).sin().abs() * 0.85;
        let amp = 9000.0 * (0.35 + 0.65 * (t * 2.6).sin().abs());
        let s: Vec<Value> = (0..64)
            .map(|i| {
                let f = i as f64;
                Value::from(
                    ((f * 0.55 + t * 9.0).sin() * 0.6 + (f * 1.7 - t * 13.0).sin() * 0.4) * amp,
                )
            })
            .collect();
        (lvl, s)
    } else {
        (0.0, vec![Value::from(0.0); 64])
    };
    json!({
        "state": mock,
        "level": level,
        "samples": samples,
        "alert": false,
        "loading": mock == "thinking",
        "rate_limits": {
            "five_hour": {"utilization": 0.34, "resets_at": t + 9200.0},
            "seven_day": {"utilization": 0.61, "resets_at": t + 288000.0},
        }
    })
}

fn read_bus(app: &App) -> Value {
    if let Some(m) = &app.mock {
        return mock_bus(m);
    }
    let bus = &app.cfg.bus;
    let mut state = std::fs::read_to_string(bus.join(".voice_state"))
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|_| "idle".into());
    if !STATES.contains(&state.as_str()) {
        state = "idle".into();
    }
    let mut level = 0.0f64;
    let mut samples = vec![Value::from(0.0); 64];
    if let Ok(t) = std::fs::read_to_string(bus.join(".voice_waveform")) {
        if let Ok(Value::Object(p)) = serde_json::from_str::<Value>(&t) {
            let ts = p.get("ts").and_then(num_f64).unwrap_or(0.0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if let Some(Value::Array(raw)) = p.get("samples") {
                if !raw.is_empty() && now - ts < WAVEFORM_STALE_S {
                    // A fresh waveform IS speech, whatever the state file says.
                    if let Some(conv) = raw
                        .iter()
                        .take(64)
                        .map(num_f64)
                        .collect::<Option<Vec<f64>>>()
                    {
                        state = "speaking".into();
                        samples = conv.iter().map(|f| Value::from(*f)).collect();
                        let mean = conv.iter().map(|f| f.abs()).sum::<f64>() / conv.len() as f64;
                        level = (mean / 3000.0).min(1.0);
                    }
                }
            }
        }
    }
    let alert = std::fs::metadata(bus.join(".voice_alert"))
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    let loading = bus.join(".voice_loading_pid").exists();
    let rate_limits = std::fs::read_to_string(bus.join(".voice_rate_limits"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    json!({
        "state": state, "level": level, "samples": samples,
        "alert": alert, "loading": loading, "rate_limits": rate_limits,
    })
}

fn no_store() -> [(axum::http::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

async fn state_handler(State(app): State<Arc<App>>) -> impl IntoResponse {
    (no_store(), Json(read_bus(&app)))
}

async fn config_handler(State(app): State<Arc<App>>) -> impl IntoResponse {
    let out = json!({
        "name": app.cfg.name, "badge": app.cfg.badge,
        "face": app.cfg.face,
        "thinking_sound": app.cfg.thinking_sound,
        "faces": list_faces(&app.here),
    });
    (no_store(), Json(out))
}

async fn static_handler(State(app): State<Arc<App>>, req: axum::extract::Request) -> Response {
    let path = req.uri().path().to_string();
    match C::resolve_static(&app.here, &path) {
        C::StaticHit::File { bytes, ctype } => (
            [
                (header::CONTENT_TYPE, ctype),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        C::StaticHit::Missing => (
            StatusCode::NOT_FOUND,
            [
                (header::CONTENT_TYPE, "text/plain"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            "not found",
        )
            .into_response(),
    }
}

/// Probe whether the port holder answers like us (GET /state -> 200).
fn probe_mine(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr: std::net::SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let mut s = match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.write_all(b"GET /state HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let mut buf = Vec::new();
    if s.read_to_end(&mut buf).is_err() {
        return false;
    }
    let head = String::from_utf8_lossy(&buf);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

fn open_browser(url: String) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        let _ = std::process::Command::new("xdg-open")
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    });
}

fn print_help() {
    println!("{NAME} {VERSION} (rust engine)");
    println!("  --mock [idle|listening|thinking|speaking]  synthesize bus (default speaking)");
    println!("  --no-open        do not auto-open the browser");
    println!("  --port N         listen port (default from configs/face.json)");
    println!(
        "  --face NAME      face id: board/neural/radial/rain (default from configs/face.json)"
    );
    println!("  --agent-home DIR agent home (default: discovered)");
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut mock: Option<String> = None;
    let mut no_open = false;
    let mut port_flag: Option<u16> = None;
    let mut face_flag: Option<String> = None;
    let mut home_flag: Option<String> = None;
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
            "--no-open" => no_open = true,
            "--mock" => {
                let v = raw.get(i + 1).cloned().unwrap_or_else(|| "speaking".into());
                if !v.starts_with("--") {
                    if i + 1 < raw.len() {
                        i += 1;
                    }
                    mock = Some(if STATES.contains(&v.as_str()) {
                        v
                    } else {
                        "speaking".into()
                    });
                } else {
                    mock = Some("speaking".into());
                }
            }
            "--port" => {
                if let Some(n) = raw.get(i + 1).and_then(|s| s.parse::<u16>().ok()) {
                    port_flag = Some(n);
                    i += 1;
                }
            }
            "--face" => {
                if let Some(n) = raw.get(i + 1) {
                    if !n.starts_with("--") {
                        face_flag = Some(n.clone());
                        i += 1;
                    }
                }
            }
            "--agent-home" => {
                if i + 1 < raw.len() {
                    home_flag = Some(raw[i + 1].clone());
                    i += 1;
                }
            }
            _ => {} // tolerate unknown args, like the Python server
        }
        i += 1;
    }

    let agent = C::agent_home(home_flag.as_deref());
    let here = agent.join("ui/face");
    let mut cfg = load_config(&agent.join("configs/face.json"), &agent);
    if let Some(p) = port_flag {
        cfg.port = p;
    }
    if let Some(f) = face_flag.filter(|f| !f.is_empty()) {
        cfg.face = f;
    }
    let port = cfg.port;
    let root = format!("http://127.0.0.1:{port}/");
    let face_idx = here.join("faces").join(&cfg.face).join("index.html");
    let url = if !cfg.face.is_empty() && face_idx.is_file() {
        format!("{root}faces/{}/", cfg.face)
    } else {
        root.clone()
    };
    let mode = match &mock {
        Some(m) => format!("MOCK={m}"),
        None => format!("bus: {}", cfg.bus.display()),
    };

    let app = Arc::new(App { here, cfg, mock });
    let router: Router = Router::new()
        .route("/state", get(state_handler))
        .route("/config", get(config_handler))
        .fallback(static_handler)
        .with_state(app);

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if probe_mine(port) {
                println!("already running at {root}  opening it instead");
                if !no_open {
                    open_browser(url);
                }
                return;
            }
            println!("port {port} is taken by something that is not this server.");
            println!(
                "Close whatever is using it, or set a different \"port\" in configs/face.json."
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{NAME}: cannot bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    println!("openbutler-face on {root}  opening {url}  ({mode})  Ctrl-C stops");
    if !no_open {
        open_browser(url);
    }
    let _ = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
}
