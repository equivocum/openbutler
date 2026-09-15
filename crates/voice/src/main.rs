// openbutler-voice — voice orchestrator (STT + TTS + dialogue loop).
// Slice 5a: config + brain runner. Slices 5c/5d/5e: TTS mouth, live loop,
// PTT, signals, console verbs.

mod app;
mod brain;
mod config;
mod console;
mod ears;
mod mouth;
mod ptt;
mod signals;
mod vlog;
mod wake;

use openbutler_common as C;

const NAME: &str = "openbutler-voice";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!("{NAME} {VERSION} (rust engine)");
    println!("  --agent-home DIR   agent home (default: discovered)");
    println!("  --config FILE      voice JSON (default: <home>/configs/voice.json;");
    println!("                     sets VOICE_CONFIG for the process)");
    println!("  config             print merged config as JSON");
    println!("  config get <key>   print one setting (dot paths: wake.threshold)");
    println!("  config set [key] [value]  interactive settings session (one flag at a time; Enter keeps)");
    println!("  brain \"MSG\"        run one opencode turn, streaming sentences");
    println!("    [--model M] [--session SID] [--effort low|medium|high|max]");
    println!("    [--perm ask|bypassPermissions]");
    println!("  brain-cmd \"/clear\"  run one console verb (with --session where relevant)");
    println!("  run              live voice loop (PTT + open-mic + typed turns)");
    println!("    [--open-mic] [--wake] [--barge-in] [--model M] [--voice V]");
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut home_flag: Option<String> = None;
    let mut config_flag: Option<String> = None;
    let mut args: Vec<String> = Vec::new();
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
            "--agent-home" if i + 1 < raw.len() => {
                home_flag = Some(raw[i + 1].clone());
                i += 2;
            }
            "--config" if i + 1 < raw.len() => {
                config_flag = Some(raw[i + 1].clone());
                i += 2;
            }
            _ => {
                args.push(raw[i].clone());
                i += 1;
            }
        }
    }
    if let Some(f) = config_flag {
        std::env::set_var("VOICE_CONFIG", f);
    }
    let agent = C::agent_home(home_flag.as_deref());
    match args.first().map(|s| s.as_str()) {
        Some("config") => {
            match args.get(1).map(|s| s.as_str()) {
                None => {
                    let cfg = config::load(&agent);
                    println!("{}", serde_json::to_string_pretty(&cfg).unwrap_or_default());
                }
                Some("get") => {
                    let key = args.get(2).cloned().unwrap_or_default();
                    if key.is_empty() || console::find_setting(&key).is_none() {
                        eprintln!("usage: {NAME} config get <key>");
                        eprintln!(
                            "keys: {}",
                            console::SETTINGS
                                .iter()
                                .map(|s| s.key)
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        std::process::exit(2);
                    }
                    let cfg = config::load(&agent);
                    match console::get_setting(&agent, &cfg, &key) {
                        Some(v) => println!("{key} = {v}"),
                        None => println!("{key} is not set (default applies)"),
                    }
                }
                Some("set") => {
                    let only = args.get(2).cloned().unwrap_or_default();
                    let direct = args.get(3).cloned();
                    if !only.is_empty() && console::find_setting(&only).is_none() {
                        eprintln!("unknown setting {only:?}");
                        eprintln!(
                            "keys: {}",
                            console::SETTINGS
                                .iter()
                                .map(|s| s.key)
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        std::process::exit(2);
                    }
                    let mut cfg = config::load(&agent);
                    let keys: Vec<&str> = if only.is_empty() {
                        console::SETTINGS.iter().map(|s| s.key).collect()
                    } else {
                        vec![
                            console::SETTINGS
                                .iter()
                                .find(|s| s.key == only)
                                .unwrap()
                                .key,
                        ]
                    };
                    use std::io::{BufRead, Write};
                    let stdin = std::io::stdin();
                    let mut lines = stdin.lock().lines();
                    let mut changed = 0u32;
                    for key in keys {
                        let cur = console::get_setting(&agent, &cfg, key)
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "(unset)".into());
                        let prompt = console::find_setting(key).unwrap().prompt;
                        if let Some(v) = direct.clone() {
                            // Non-interactive: `config set speed 1.1`.
                            match console::parse_setting(key, &v) {
                                Ok(val) => {
                                    if console::write_setting(&agent, &mut cfg, key, val.clone()) {
                                        println!("{key} = {val} (saved)");
                                        changed += 1;
                                    } else {
                                        println!(
                                            "{key} = {val} (session only — file not writable)"
                                        );
                                    }
                                }
                                Err(e) => {
                                    eprintln!("{e}");
                                    std::process::exit(2);
                                }
                            }
                            continue;
                        }
                        loop {
                            print!("{prompt}\n  {key} [{cur}]: ");
                            let _ = std::io::stdout().flush();
                            let line = match lines.next() {
                                Some(Ok(l)) => l,
                                _ => break, // EOF (Ctrl-D / pipe): keep the rest
                            };
                            let line = line.trim();
                            if line.is_empty() {
                                break; // keep current
                            }
                            match console::parse_setting(key, line) {
                                Ok(val) => {
                                    if console::write_setting(&agent, &mut cfg, key, val.clone()) {
                                        println!("  saved: {key} = {val}");
                                        changed += 1;
                                    } else {
                                        println!(
                                            "  session only (file not writable): {key} = {val}"
                                        );
                                    }
                                    break;
                                }
                                Err(e) => println!("  {e} — try again (Enter keeps [{cur}])"),
                            }
                        }
                    }
                    println!("{changed} setting(s) updated. Takes effect on next launch.");
                }
                Some(other) => {
                    eprintln!("{NAME}: unknown config verb '{other}' (see --help)");
                    std::process::exit(2);
                }
            }
        }
        Some("brain") => {
            let msg = args.get(1).cloned().unwrap_or_default();
            if msg.is_empty() || msg.starts_with("--") {
                eprintln!("usage: {NAME} brain \"MSG\" [--model M] [--session SID] [--effort E] [--perm P]");
                std::process::exit(2);
            }
            let (mut model, mut sid, mut effort, mut perm) = (None, None, None, None);
            let mut j = 2;
            while j < args.len() {
                match args[j].as_str() {
                    "--model" if j + 1 < args.len() => {
                        model = Some(args[j + 1].clone());
                        j += 1;
                    }
                    "--session" if j + 1 < args.len() => {
                        sid = Some(args[j + 1].clone());
                        j += 1;
                    }
                    "--effort" if j + 1 < args.len() => {
                        effort = Some(args[j + 1].clone());
                        j += 1;
                    }
                    "--perm" if j + 1 < args.len() => {
                        perm = Some(args[j + 1].clone());
                        j += 1;
                    }
                    _ => {}
                }
                j += 1;
            }
            if let Some(e) = &effort {
                if !["low", "medium", "high", "max"].contains(&e.as_str()) {
                    eprintln!("{NAME}: refused /effort {e:?}");
                    std::process::exit(2);
                }
            }
            let cfg = config::load(&agent);
            let get = |k: &str| {
                cfg.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let mut b = brain::Brain::new(
                get("agent_dir"),
                config::discipline(&cfg),
                get("signals_dir"),
                cfg.get("resume_last_session")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                perm.unwrap_or_else(|| get("permission_mode")),
                model,
                cfg.get("opencode_model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                sid,
            );
            if let Some(e) = effort {
                let r = b.command(&format!("/effort {e}"));
                eprintln!("[brain-oc] {r}");
            }
            if let Err(e) = b.start() {
                eprintln!("{NAME}: {e}");
                std::process::exit(1);
            }
            b.load_resume_id();
            let mut on_sentence = |s: &str, dt: f64| println!("  ({dt:4.1}s) {s}");
            match b.run_turn(&msg, &mut on_sentence) {
                Ok(o) => eprintln!(
                    "[brain-oc] turn done: {} sentence(s) sid={} exit={} turns={} in={} out={} cost={:.4}",
                    o.sentences.len(),
                    b.sid.as_deref().unwrap_or("-"),
                    o.exit_code,
                    b.turns,
                    b.in_tokens,
                    b.out_tokens,
                    b.cost
                ),
                Err(e) => {
                    eprintln!("{NAME}: turn failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("brain-cmd") => {
            let cmd = args.get(1).cloned().unwrap_or_default();
            let mut sid = None;
            let mut j = 2;
            while j < args.len() {
                if args[j] == "--session" && j + 1 < args.len() {
                    sid = Some(args[j + 1].clone());
                    j += 1;
                }
                j += 1;
            }
            if cmd.is_empty() {
                eprintln!("usage: {NAME} brain-cmd \"/clear\" [--session SID]");
                std::process::exit(2);
            }
            let cfg = config::load(&agent);
            let get = |k: &str| {
                cfg.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let mut b = brain::Brain::new(
                get("agent_dir"),
                config::discipline(&cfg),
                get("signals_dir"),
                false,
                get("permission_mode"),
                None,
                cfg.get("opencode_model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                sid,
            );
            println!("{}", b.command(&cmd));
        }
        Some("run") => {
            let mut o = app::RunOpts {
                open_mic: false,
                barge_in: false,
                wake: false,
                model: None,
                voice: None,
            };
            let mut j = 1;
            while j < args.len() {
                match args[j].as_str() {
                    "--open-mic" => o.open_mic = true,
                    "--wake" => o.wake = true,
                    "--barge-in" => o.barge_in = true,
                    "--model" if j + 1 < args.len() => {
                        o.model = Some(args[j + 1].clone());
                        j += 1;
                    }
                    "--voice" if j + 1 < args.len() => {
                        o.voice = Some(args[j + 1].clone());
                        j += 1;
                    }
                    _ => {}
                }
                j += 1;
            }
            let cfg = config::load(&agent);
            std::process::exit(app::run(&agent, cfg, o));
        }
        Some(other) => {
            eprintln!("{NAME}: unknown subcommand '{other}' (see --help)");
            std::process::exit(2);
        }
        None => {
            println!("{NAME} {VERSION} (rust engine) — voice orchestrator: config + brain + run");
        }
    }
}
