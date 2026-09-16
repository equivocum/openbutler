// Known-drift repair, one confirm per item. Never bulk-fixes.
//   1. Broken live configs are quarantined to .unwired and re-rendered.
//   2. GREETING wiring: live voice.json greeting is adopted into .env,
//      else the shared default (render seeds JSON from .env when missing).
//   3. configs/override.json is unread by any code (and may carry an
//      elevenlabs.enabled:true landmine) — quarantine to .unwired.
//   4. Template-vs-live values are display-only (informative, no change).

use std::io::{BufRead, Write};
use std::path::Path;

fn confirm(label: &str, def_yes: bool) -> bool {
    let hint = if def_yes { "Y/n" } else { "y/N" };
    print!("{label} ({hint}): ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) {
        Ok(_) => match s.trim().to_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => def_yes,
        },
        Err(_) => def_yes,
    }
}

pub fn run(home: &Path) -> i32 {
    let mut fixed = 0u32;

    // 1. Broken live configs: an unparsable file can neither run nor
    // render (render refuses to overwrite it) — quarantine and re-render.
    {
        let mut broken = vec![];
        for f in [
            "configs/voice.json",
            "configs/face.json",
            "configs/board.json",
        ] {
            let p = home.join(f);
            if p.is_file() && !openbutler_common::is_json_object(&p) {
                broken.push((f, p));
            }
        }
        if !broken.is_empty() {
            println!("broken configs (not valid JSON objects):");
            for (f, _) in &broken {
                println!("  {f}");
            }
            if confirm("quarantine to .unwired and re-render from .env", true) {
                for (f, p) in &broken {
                    let q = p.with_extension("json.unwired");
                    match std::fs::rename(p, &q) {
                        Ok(()) => println!("quarantined {f}."),
                        Err(e) => {
                            eprintln!("rename failed for {f}: {e}");
                            continue;
                        }
                    }
                }
                let vars = openbutler_setup::envfile::load(home);
                match openbutler_setup::render::render_all(home, &vars) {
                    Ok(w) => {
                        println!("rendered {}.", w.join(", "));
                        fixed += 1;
                    }
                    Err(e) => eprintln!("render failed: {e}"),
                }
            }
        } else {
            println!("configs: all parse, ok.");
        }
    }

    // 2. GREETING wiring: adopt the live voice.json greeting when one is
    // set (the file is the runtime truth); else seed the shared default.
    {
        let vars = openbutler_setup::envfile::load(home);
        if vars.contains_key("GREETING") {
            println!("GREETING: present in .env, wired to voice.json greeting (ok).");
        } else {
            let live = openbutler_common::read_json_object(&home.join("configs/voice.json"))
                .get("greeting")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| openbutler_common::settings::DEFAULT_GREETING.into());
            let adopted = live != openbutler_common::settings::DEFAULT_GREETING;
            if confirm(
                &format!(
                    "GREETING missing in .env — {} \"{live}\"",
                    if adopted {
                        "adopt live value"
                    } else {
                        "add default"
                    }
                ),
                true,
            ) {
                let mut v = vars;
                v.insert("GREETING".into(), live);
                match openbutler_setup::envfile::write(home, &v) {
                    Ok(()) => {
                        println!("added GREETING.");
                        fixed += 1;
                    }
                    Err(e) => eprintln!("{e}"),
                }
            }
        }
    }

    // 3. Unwired override.json.
    {
        let p = home.join("configs/override.json");
        let q = home.join("configs/override.json.unwired");
        if p.is_file() && !q.is_file() {
            println!("drift: configs/override.json is read by NO code,");
            println!("       and it sets elevenlabs.enabled=true with an empty voice_id.");
            if confirm("quarantine it to override.json.unwired", true) {
                match std::fs::rename(&p, &q) {
                    Ok(()) => {
                        println!("quarantined.");
                        fixed += 1;
                    }
                    Err(e) => eprintln!("rename failed: {e}"),
                }
            }
        } else if q.is_file() {
            println!("override.json: already quarantined, ok.");
        } else {
            println!("override.json: absent, ok.");
        }
    }

    // 4. Template-vs-live display.
    {
        let live = openbutler_setup::envfile::load(home);
        let example = std::fs::read_to_string(home.join(".env.example"))
            .map(|t| openbutler_setup::envfile::parse(&t))
            .unwrap_or_default();
        let mut shown = false;
        for k in [
            "AGENT_NAME",
            "MEMORY_VAULT",
            "VOICE_CONTAINER",
            "FACE_NAME",
            "STT_MODEL",
            "VOICE_NAME",
        ] {
            let (l, e) = (live.get(k), example.get(k));
            if l != e {
                if !shown {
                    println!("live-vs-template (informative, no change):");
                    shown = true;
                }
                println!(
                    "  {k}: live={} template={}",
                    l.map(|s| s.as_str()).unwrap_or("(unset)"),
                    e.map(|s| s.as_str()).unwrap_or("(unset)")
                );
            }
        }
        if !shown {
            println!("template-vs-live: no drift.");
        }
    }

    println!("{fixed} item(s) fixed.");
    0
}
