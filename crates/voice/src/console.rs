// Voice console — port of the CONSOLE_VERBS block in backtalk/main.py.
//
// Exact spoken phrases, spoken alone, control the session itself so the
// person never goes back to the keyboard. Matching is EXACT after
// normalization (never prefixes); quit phrases are the one exception —
// those are substring matches, as in Python.

// Settings core lives in openbutler-common; voice re-exports it.
pub use openbutler_common::settings::{
    find_setting, get_setting, parse_setting, write_config_key, write_setting, SETTINGS,
};

/// verb -> accepted exact phrases.
pub const CONSOLE_VERBS: &[(&str, &[&str])] = &[
    (
        "clear",
        &[
            "clear the session",
            "clear the context",
            "clear context",
            "fresh slate",
            "slash clear",
        ],
    ),
    (
        "compact",
        &[
            "compact the session",
            "compact the context",
            "compact context",
            "slash compact",
        ],
    ),
    (
        "deep",
        &[
            "switch to the deep model",
            "use the deep model",
            "slash model deep",
        ],
    ),
    (
        "fast",
        &[
            "switch to the fast model",
            "use the fast model",
            "back to the fast model",
            "slash model fast",
        ],
    ),
    ("usage", &["usage report", "slash usage"]),
    (
        "micopen",
        &[
            "go hands free",
            "hands free mode",
            "hands free listening",
            "open mic",
            "open the mic",
        ],
    ),
    (
        "micptt",
        &[
            "push to talk",
            "push to talk mode",
            "back to push to talk",
            "back to the button",
        ],
    ),
    (
        "micwake",
        &[
            "wake word mode",
            "listen for the wake word",
            "only listen for hey jarvis",
            "slash mic wake",
        ],
    ),
    (
        "noask",
        &[
            "stop asking for permission",
            "stop asking permission",
            "stop asking me for permission",
            "turn off the permission prompt",
            "turn off the permission prompts",
            "turn off the permissions prompt",
            "turn off the permissions prompts",
            "turn off permissions",
            "turn off permission checks",
            "disable the permission checks",
            "disable permission checks",
            "auto approve",
            "auto approve mode",
        ],
    ),
    (
        "ask",
        &[
            "start asking again",
            "ask before acting",
            "ask for permission again",
        ],
    ),
];

pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Normalize speech for exact matching: lowercase, hyphens to spaces,
/// collapsed, edge punctuation stripped.
fn norm_verb(text: &str) -> String {
    text.to_lowercase()
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c| matches!(c, '.' | ',' | '!' | '?'))
        .to_string()
}

/// Lowercase, every non-letter to space, collapse. Used for the yes/no
/// permission vocabulary (exact matches only — prefix matching turns
/// "yesterday" into consent).
pub fn norm_speech(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|ch| if ('a'..='z').contains(&ch) { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Exact-yes vocabulary for the spoken permission gate. Unused on the
/// OpenCode engine (no mid-turn tool hook); kept as the ported contract
/// for the gate, whichever engine grows one.
#[allow(dead_code)]
pub const YES: &[&str] = &[
    "yes",
    "yeah",
    "yep",
    "yup",
    "sure",
    "approve",
    "approved",
    "go ahead",
    "do it",
    "yes please",
    "yes sir",
    "yes boss",
    "yes go ahead",
    "go for it",
    "green light",
    "okay",
    "ok",
    "y",
    "permission granted",
    "granted",
    "you have permission",
    "you may",
    "allowed",
    "allow it",
    "confirmed",
    "affirmative",
];

/// Fold a voice name for comparison: lowercase, drop spaces,
/// underscores, hyphens. "bf emma" ≡ "bf_emma" ≡ "BF-EMMA".
pub fn fold_voice(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect()
}

/// Levenshtein distance over chars (voice names are short).
pub fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Rank known voices against a heard name (already folded by the
/// caller — pass fold_voice(want)). Returns (name, distance), best
/// first. No threshold here: callers decide between confirm-ask
/// (close) and closest-3 (far), so a miss degrades to a helpful
/// question instead of a wrong switch.
pub fn rank_voices<'a>(voices: &'a [String], folded_want: &str) -> Vec<(&'a str, usize)> {
    let mut v: Vec<(&str, usize)> = voices
        .iter()
        .map(|n| (n.as_str(), lev(&fold_voice(n), folded_want)))
        .collect();
    v.sort_by_key(|(_, d)| *d);
    v
}

/// Close enough to ask "did you mean …?" — generous on purpose: a
/// confirm-ask costs one turn, a missed switch costs the soak.
pub fn voice_close_enough(folded_want: &str, dist: usize) -> bool {
    dist <= (folded_want.len() / 3).max(3)
}

/// Match one utterance against the console verbs. Returns the verb, or
/// `effort:<lvl>` for the effort phrases. `voice:<name>` for voice-switch
/// phrases ("switch voice to X" / "use voice X" / "slash voice X").
pub fn console_match(text: &str) -> Option<String> {
    let norm = norm_verb(text);
    for (verb, phrases) in CONSOLE_VERBS {
        if phrases.contains(&norm.as_str()) {
            return Some(verb.to_string());
        }
    }
    for lvl in EFFORTS {
        if norm == format!("set effort to {lvl}")
            || norm == format!("effort {lvl}")
            || norm == format!("slash effort {lvl}")
        {
            return Some(format!("effort:{lvl}"));
        }
    }
    for prefix in ["switch voice to ", "use voice ", "slash voice "] {
        if let Some(name) = norm.strip_prefix(prefix) {
            let name = name.trim().replace(' ', "");
            if !name.is_empty() {
                return Some(format!("voice:{name}"));
            }
        }
    }
    None
}

/// Substring quit-phrase test, mirroring `any(q in text.lower() ...)` —
/// "No! Don't hang up, skip it" must NOT quit (handled by callers that
/// check exactness first where needed), but any turn containing a quit
/// phrase hangs up. Both sides are letter-normalized first so STT
/// punctuation ("Goodbye, assistant.") still matches ("goodbye
/// assistant"). Python's copy keeps the raw-substring bug; it is not
/// being fixed — the Python engine is deprecated pending removal.
pub fn is_quit(text: &str, quit_phrases: &[String]) -> bool {
    let low = norm_speech(text);
    quit_phrases.iter().any(|q| {
        let nq = norm_speech(q);
        !nq.is_empty() && low.contains(nq.as_str())
    })
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        return format!("about {} million tokens", round1(n as f64 / 1_000_000.0));
    }
    if n >= 1000 {
        return format!(
            "about {} thousand tokens",
            (n as f64 / 1000.0).round() as u64
        );
    }
    format!("{n} tokens")
}

fn round1(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r == r.trunc() {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// A short CFO brief of the session, written for the ear. The OpenCode
/// engine exposes no context breakdown, so this is turns + spoken-out +
/// cost only (mirrors _spoken_usage with ctx_usage=None).
pub fn spoken_usage(turns: u64, out_tokens: u64, cost: f64) -> String {
    let mut parts = vec![
        format!(
            "{} turn{} this session",
            turns,
            if turns == 1 { "" } else { "s" }
        ),
        fmt_tokens(out_tokens) + " spoken out",
    ];
    let cents = (cost * 100.0).round() as i64;
    if cents >= 1 {
        parts.push(if cents < 100 {
            format!("roughly {cents} cents")
        } else {
            format!("roughly {} dollars", (cents as f64 / 100.0).round() as i64)
        });
    }
    parts.join(". ") + "."
}

/// Scrub terminal-copy artifacts: blockquote gutter glyphs and stray
/// whitespace (copying from a CLI chat render drags bars along).
pub fn clean_typed(line: &str) -> String {
    let mut s = line.trim().to_string();
    loop {
        let t = s.trim_start().to_string();
        if let Some(c) = t.chars().next() {
            if c == '▎' || c == '│' || c == '>' {
                s = t[c.len_utf8()..].trim_start().to_string();
                continue;
            }
        }
        s = t;
        break;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_match_exactly() {
        assert_eq!(console_match("clear the session"), Some("clear".into()));
        assert_eq!(console_match("Go hands free!"), Some("micopen".into()));
        assert_eq!(
            console_match("set effort to high"),
            Some("effort:high".into())
        );
        assert_eq!(
            console_match("slash effort xhigh"),
            Some("effort:xhigh".into())
        );
        assert_eq!(console_match("usage report"), Some("usage".into()));
        // Ordinary sentences never trigger.
        assert_eq!(console_match("please clear the session for me"), None);
        assert_eq!(console_match(""), None);
    }

    #[test]
    fn voice_verb_parses() {
        assert_eq!(
            console_match("switch voice to af_heart"),
            Some("voice:af_heart".into())
        );
        assert_eq!(
            console_match("use voice bm_george"),
            Some("voice:bm_george".into())
        );
        assert_eq!(console_match("switch voice to"), None);
    }

    #[test]
    fn speech_norm_is_exact_only() {
        assert!(YES.contains(&norm_speech("Yes, confirm").as_str()) == false);
        assert!(YES.contains(&norm_speech("yes").as_str()));
        assert!(!YES.contains(&norm_speech("yesterday").as_str()));
    }

    #[test]
    fn quit_is_substring() {
        let q = vec!["goodbye assistant".to_string(), "hang up".to_string()];
        assert!(is_quit("well, goodbye assistant, thanks", &q));
        assert!(is_quit("Goodbye, assistant.", &q));
        assert!(is_quit("GOODBYE ASSISTANT!", &q));
        assert!(!is_quit("hello there", &q));
    }

    #[test]
    fn usage_brief_reads_for_the_ear() {
        assert_eq!(
            spoken_usage(1, 40, 0.0),
            "1 turn this session. 40 tokens spoken out."
        );
        assert_eq!(
            spoken_usage(3, 2500, 0.12),
            "3 turns this session. about 3 thousand tokens spoken out. roughly 12 cents."
        );
    }

    #[test]
    fn typed_scrub_strips_gutters() {
        assert_eq!(clean_typed("▎ hello"), "hello");
        assert_eq!(clean_typed("> > deep"), "deep");
    }

    #[test]
    fn voice_fold_and_rank() {
        assert_eq!(fold_voice("bf emma"), "bfemma");
        assert_eq!(fold_voice("bf_emma"), "bfemma");
        assert_eq!(fold_voice("BF-EMMA"), "bfemma");
        assert_eq!(lev("kitten", "sitting"), 3);
        assert_eq!(lev("", "abc"), 3);
        let vs = vec![
            "af_heart".to_string(),
            "bf_emma".to_string(),
            "bm_george".to_string(),
            "bm_lewis".to_string(),
        ];
        // Soak cases: perfect-STT and mangled hearings all rank bf_emma first.
        for heard in ["bf emma", "beanemma", "bfm"] {
            let r = rank_voices(&vs, &fold_voice(heard));
            assert_eq!(r[0].0, "bf_emma", "heard {heard:?}");
            assert!(
                voice_close_enough(&fold_voice(heard), r[0].1),
                "heard {heard:?} d={}",
                r[0].1
            );
        }
        // Perfect hit is distance zero.
        let r = rank_voices(&vs, &fold_voice("bm_lewis"));
        assert_eq!(r[0], ("bm_lewis", 0));
        // Nonsense is far from everything.
        let r = rank_voices(&vs, &fold_voice("xyzzy plugh"));
        assert!(!voice_close_enough(&fold_voice("xyzzy plugh"), r[0].1));
    }
}
