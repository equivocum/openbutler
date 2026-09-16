// openbutler-setup — first-use setup + audit for an OpenButler home.
//
//   setup check                 audit everything, exit 1 on any gap
//   setup init [--yes]          interactive first-use walk (identity ->
//                               .env -> render -> models -> devices)
//   setup fix                   repair known drift, one confirm per item
//
// Bash does nothing here: rendering lives in render.rs (native port of
// the old shell+python script); this binary asks, verifies, and fetches.
// Host-buildable: only openbutler-common + serde_json.

mod checks;
mod fix;
mod init;

const NAME: &str = "openbutler-setup";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!("{NAME} {VERSION} — first-use setup + audit");
    println!("  --agent-home DIR   agent home (default: discovered)");
    println!("  check              audit .env, configs, models, devices (exit 1 on gaps)");
    println!("  init [--yes]       interactive setup walk; --yes takes defaults non-interactively");
    println!("  fix                repair known drift, confirming each item");
    println!("  (all settings live under one CLI: `openbutler config set <key> <value>`)");
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut home_flag: Option<String> = None;
    let mut args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "--agent-home" if i + 1 < raw.len() => {
                home_flag = Some(raw[i + 1].clone());
                i += 2;
            }
            _ => {
                args.push(raw[i].clone());
                i += 1;
            }
        }
    }
    let home = openbutler_common::agent_home(home_flag.as_deref());
    if !home.join("Cargo.toml").is_file() || !home.join("crates").is_dir() {
        eprintln!(
            "{NAME}: not an OpenButler home (no Cargo.toml + crates/ under {})",
            home.display()
        );
        std::process::exit(2);
    }
    match args.first().map(|s| s.as_str()) {
        Some("check") => std::process::exit(checks::run(&home)),
        Some("init") => {
            let yes = args.iter().any(|a| a == "--yes" || a == "-y");
            std::process::exit(init::run(&home, yes));
        }
        Some("fix") => std::process::exit(fix::run(&home)),
        Some(other) => {
            eprintln!("{NAME}: unknown subcommand '{other}' (see --help)");
            std::process::exit(2);
        }
        None => print_help(),
    }
}
