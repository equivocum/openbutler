// OpenCode brain — port of backtalk/brain_oc.py WarmBrain.
// One `opencode run --format json` process per turn; the opencode SESSION
// (`--session`) carries the conversation. Sentences stream out as each
// step's text arrives; the mouth starts after the first STEP, not token.

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const EMPTY_FALLBACK: &str =
    "My brain came back empty on that one. Say it again and I'll take another run at it.";

fn find_opencode() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join("opencode");
        if is_executable(&p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if p.as_os_str().as_bytes().is_empty() {
        return false;
    }
    unsafe {
        extern "C" {
            fn access(path: *const i8, mode: i32) -> i32;
        }
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        access(c.as_ptr(), 1) == 0
    }
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

pub struct Brain {
    model_flag: Option<String>,
    pub model_label: String,
    variant: Option<String>,
    perm: String,
    pub sid: Option<String>,
    primed: bool,
    agent_dir: String,
    discipline: String,
    resume: bool,
    signals_dir: String,
    /// Live-turn kill handle (slice 5d): the turn thread owns the Child
    /// inside here while it blocks on read/wait, so the main thread can
    /// kill a live turn without touching Brain. The turn thread reaps.
    proc: Arc<Mutex<Option<Child>>>,
    pub turns: u64,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub cost: f64,
}

impl Brain {
    pub fn new(
        agent_dir: String,
        discipline: String,
        signals_dir: String,
        resume: bool,
        perm: String,
        model: Option<String>,
        model_default: Option<String>,
        resume_id: Option<String>,
    ) -> Self {
        let model_flag = model.or(model_default).filter(|s| !s.is_empty());
        let model_label = model_flag
            .clone()
            .unwrap_or_else(|| "opencode (default)".into());
        let perm = if perm == "default" {
            "ask".into()
        } else {
            perm
        };
        Brain {
            model_flag,
            model_label,
            variant: None,
            perm,
            sid: resume_id,
            primed: false,
            agent_dir,
            discipline,
            resume,
            signals_dir,
            proc: Arc::new(Mutex::new(None)),
            turns: 0,
            in_tokens: 0,
            out_tokens: 0,
            cost: 0.0,
        }
    }

    pub fn start(&self) -> Result<(), String> {
        if find_opencode().is_none() {
            return Err("opencode not found on PATH; cannot start the OpenCode brain".into());
        }
        Ok(())
    }

    pub fn set_permission_mode(&mut self, mode: &str) {
        self.perm = if mode == "default" {
            "ask".into()
        } else {
            mode.into()
        };
    }

    fn session_file(&self) -> std::path::PathBuf {
        Path::new(&self.signals_dir).join(".openbutler_session")
    }

    pub fn load_resume_id(&mut self) {
        if !self.resume {
            return;
        }
        if let Ok(s) = std::fs::read_to_string(self.session_file()) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                self.sid = Some(s);
            }
        }
    }

    fn remember_session(&self, sid: &Option<String>) {
        if !self.resume {
            return;
        }
        let sid = match sid {
            Some(s) if !s.is_empty() => s,
            _ => return,
        };
        let _ = std::fs::write(self.session_file(), sid);
    }

    fn cmd(&self, message: &str, sid: &Option<String>) -> Vec<String> {
        // NOTE: no --attach. Plain `run --session` continues correctly
        // every time; the spawn cost is worth correct turns.
        let mut c = vec![
            "opencode".to_string(),
            "run".to_string(),
            "--dir".to_string(),
            self.agent_dir.clone(),
            "--format".to_string(),
            "json".to_string(),
        ];
        if let Some(s) = sid {
            c.push("--session".into());
            c.push(s.clone());
        }
        if let Some(m) = &self.model_flag {
            c.push("-m".into());
            c.push(m.clone());
        }
        if let Some(v) = &self.variant {
            c.push("--variant".into());
            c.push(v.clone());
        }
        if self.perm == "bypassPermissions" {
            c.push("--auto".into());
        }
        c.push(message.to_string());
        c
    }

    /// Run one turn, calling `on_sentence` for each complete sentence.
    /// Returns (sentences, saw_text). Mirrors ask_stream, blocking.
    pub fn run_turn(
        &mut self,
        utterance: &str,
        on_sentence: &mut dyn FnMut(&str, f64),
    ) -> Result<TurnOutcome, String> {
        let t0 = Instant::now();
        // Stale-session retry: at most one fresh restart per turn.
        for attempt in 0..2 {
            let sid_before = self.sid.clone();
            let out = self.run_once(utterance, sid_before.as_ref(), &t0, on_sentence)?;
            if !out.saw_sid && sid_before.is_some() && attempt == 0 {
                eprintln!("[brain-oc] saved session stale, starting fresh");
                self.sid = None;
                self.primed = false;
                continue;
            }
            return Ok(out);
        }
        unreachable!()
    }

    fn run_once(
        &mut self,
        utterance: &str,
        sid: Option<&String>,
        t0: &Instant,
        on_sentence: &mut dyn FnMut(&str, f64),
    ) -> Result<TurnOutcome, String> {
        let message = if self.primed {
            utterance.to_string()
        } else {
            format!("{}\n\n{utterance}", self.discipline)
        };
        let argv = self.cmd(&message, &sid.cloned());
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            // The brain takes its message via argv. Its stdin is nulled
            // on purpose: inheriting the voice's stdin lets a racing
            // opencode process slurp a typed line meant for the voice
            // loop (consumed-stdin mystery, found live: warmup ate four
            // piped lines and the typed reader saw only EOF).
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn opencode: {e}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // The child lives in the shared handle from here to wait: the
        // main thread's interrupt() kills through it while this thread
        // blocks on the pipes, and this thread reaps via take()+wait.
        *self.proc.lock().unwrap() = Some(child);
        let mut buf = String::new();
        let mut sid = sid.cloned();
        let mut tokens: Option<Value> = None;
        let mut cost = 0.0f64;
        let mut saw_sid = false;
        let mut saw_text = false;
        let mut noise: Vec<String> = Vec::new();
        let mut sentences: Vec<String> = Vec::new();

        if let Some(out) = stdout {
            for line in BufReader::new(out).lines() {
                let raw = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let ev: Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => {
                        let t = raw.trim();
                        if !t.is_empty() {
                            let mut s = t.to_string();
                            if s.len() > 300 {
                                s = s[s.len() - 300..].to_string();
                            }
                            noise.push(s);
                            if noise.len() > 5 {
                                noise.remove(0);
                            }
                        }
                        continue;
                    }
                };
                let obj = match ev.as_object() {
                    Some(o) => o,
                    None => continue,
                };
                if !saw_sid {
                    if let Some(s) = obj.get("sessionID").and_then(|v| v.as_str()) {
                        sid = Some(s.to_string());
                        saw_sid = true;
                    }
                }
                match obj.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        let text = obj
                            .get("part")
                            .and_then(|p| p.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if text.is_empty() {
                            continue;
                        }
                        saw_text = true;
                        if !buf.is_empty()
                            && !buf.ends_with([' ', '\n', '\t'])
                            && !text.starts_with([
                                ' ', '\n', '\t', '.', ',', '!', '?', ';', ':', '\'', '"', ')',
                            ])
                        {
                            buf.push(' ');
                        }
                        buf.push_str(text);
                        while let Some((s, rest)) = split_sentence(&buf) {
                            buf = rest;
                            if !s.is_empty() {
                                let dt = t0.elapsed().as_secs_f64();
                                on_sentence(&s, dt);
                                sentences.push(s);
                            }
                        }
                    }
                    Some("step_finish") => {
                        if let Some(part) = obj.get("part").and_then(|v| v.as_object()) {
                            if part.get("tokens").is_some() {
                                tokens = part.get("tokens").cloned();
                            }
                            if let Some(c) = part.get("cost").and_then(|v| v.as_f64()) {
                                cost += c;
                            } else if let Some(c) = part
                                .get("cost")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                            {
                                cost += c;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Drain stderr into noise (bounded) for the empty-turn report.
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines().take(20) {
                if let Ok(l) = line {
                    let t = l.trim().to_string();
                    if !t.is_empty() {
                        let mut s = t;
                        if s.len() > 300 {
                            s = s[s.len() - 300..].to_string();
                        }
                        noise.push(s);
                        if noise.len() > 5 {
                            noise.remove(0);
                        }
                    }
                }
            }
        }
        let exit_code = self
            .proc
            .lock()
            .unwrap()
            .take()
            .map(|mut c| c.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1))
            .unwrap_or(-1);

        self.sid = sid.clone();
        self.primed = true;
        self.tally(tokens.as_ref(), cost);
        self.remember_session(&sid);
        let tail = buf.trim().to_string();
        let mut empty = false;
        if !tail.is_empty() {
            let dt = t0.elapsed().as_secs_f64();
            on_sentence(&tail, dt);
            sentences.push(tail);
        } else if !saw_text {
            eprintln!(
                "[brain-oc] empty turn (exit={exit_code} sid={}): {}",
                if saw_sid { "yes" } else { "no" },
                noise.join(" | ").chars().take(400).collect::<String>()
            );
            let dt = t0.elapsed().as_secs_f64();
            on_sentence(EMPTY_FALLBACK, dt);
            sentences.push(EMPTY_FALLBACK.to_string());
            empty = true;
        }
        Ok(TurnOutcome {
            sentences,
            saw_text,
            saw_sid,
            exit_code,
            empty,
        })
    }

    fn tally(&mut self, tokens: Option<&Value>, cost: f64) {
        self.turns += 1;
        if let Some(t) = tokens.and_then(|v| v.as_object()) {
            self.out_tokens += t.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
            self.in_tokens += t.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
        }
        if cost != 0.0 {
            self.cost += cost;
        }
    }

    /// Console verbs. Side effects match what main.py tells the person.
    pub fn command(&mut self, cmd: &str) -> String {
        let c = cmd.trim();
        if c == "/clear" {
            self.sid = None;
            self.primed = false;
            return "cleared".into();
        }
        if c == "/compact" {
            self.sid = None;
            self.primed = false;
            return "compacted via fresh session".into();
        }
        if let Some(want) = c.strip_prefix("/model") {
            let want = want.trim();
            if want.contains('/') {
                self.model_flag = Some(want.to_string());
                self.model_label = want.to_string();
                return format!("model {want}");
            }
            return format!(
                "refused /model {want:?}: the OpenCode engine needs a provider/model id"
            );
        }
        if let Some(lvl) = c.strip_prefix("/effort") {
            let lvl = lvl.trim().to_lowercase();
            if ["low", "medium", "high", "max"].contains(&lvl.as_str()) {
                self.variant = Some(lvl.clone());
                return format!("effort {lvl}");
            }
            return format!("refused /effort {lvl:?}");
        }
        format!("unknown console command {cmd:?}")
    }

    pub fn interrupt(&self) {
        if let Some(c) = self.proc.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }

    /// Clone the kill handle for threads that must interrupt a live turn
    /// without borrowing Brain.
    pub fn proc_handle(&self) -> Arc<Mutex<Option<Child>>> {
        self.proc.clone()
    }

    pub fn reset_turn(&self) {
        // No shared pipe on this engine: a dead process is the whole story.
        // Kill here; the turn thread (or the next turn) reaps via wait.
        if let Some(c) = self.proc.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }

    pub fn stop(&self) {
        self.reset_turn();
    }
}

pub struct TurnOutcome {
    pub sentences: Vec<String>,
    #[allow(dead_code)]
    pub saw_text: bool,
    pub saw_sid: bool,
    pub exit_code: i32,
    #[allow(dead_code)]
    pub empty: bool,
}

/// Split the first `(?<=[.!?])\s` boundary: returns (sentence, rest).
/// Abbreviations like "U.S.", "U.K.", "Dr." are NOT treated as sentence
/// boundaries — a `. ` after an abbreviation is continuation, not a split.
fn split_sentence(buf: &str) -> Option<(String, String)> {
    const ABBREVS: &[&str] = &[
        "U.S.", "U.S.A.", "U.K.", "E.U.", "U.N.", "Dr.", "Mr.", "Ms.", "Mrs.", "St.", "e.g.",
        "i.e.", "etc.",
    ];
    let mut prev_end = false;
    for (i, c) in buf.char_indices() {
        if prev_end && c.is_whitespace() {
            let end = i + c.len_utf8();
            let candidate = &buf[..i];
            if ABBREVS.iter().any(|a| candidate.ends_with(a)) {
                prev_end = false;
                continue;
            }
            let s = candidate.trim().to_string();
            return Some((s, buf[end..].to_string()));
        }
        prev_end = matches!(c, '.' | '!' | '?');
    }
    None
}
