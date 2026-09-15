// Known-drift repair, one confirm per item. Never bulk-fixes.
//   1. GREETING wiring (voice.json greeting) — ensure .env has it.
//   2. configs/override.json is unread by any code (and may carry an
//      elevenlabs.enabled:true landmine) — quarantine to .unwired.
//   3. Template-vs-live values are display-only (informative, no change).

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

    // 1. GREETING wiring: .env GREETING -> voice.json greeting (render).
    {
        let vars = crate::envfile::load(home);
        if vars.contains_key("GREETING") {
            println!("GREETING: present in .env, wired to voice.json greeting (ok).");
        } else if confirm(
            "GREETING missing in .env (needed for greeting wiring) — add default",
            true,
        ) {
            let mut v = vars;
            v.insert(
                "GREETING".into(),
                "Hello, {name}. What are we working on today?".into(),
            );
            match crate::envfile::write(home, &v) {
                Ok(()) => {
                    println!("added GREETING.");
                    fixed += 1;
                }
                Err(e) => eprintln!("{e}"),
            }
        }
    }

    // 2. Unwired override.json.
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

    // 3. Template-vs-live display.
    {
        let live = crate::envfile::load(home);
        let example = std::fs::read_to_string(home.join(".env.example"))
            .map(|t| crate::envfile::parse(&t))
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
