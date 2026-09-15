// Session log: terminal print + timestamped append to logs/voice.log.
// Every load-bearing line goes through log() so the next gremlin comes
// with receipts. A broken log file must never take the voice down.

use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Set the log file (default: ./logs/voice.log). Call once at startup.
pub fn init(path: PathBuf) {
    let _ = LOG_PATH.set(path);
}

fn path() -> PathBuf {
    LOG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("logs/voice.log"))
}

pub fn log(line: &str) {
    println!("{line}");
    let _ = (|| -> Result<(), String> {
        let p = path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let now = chrono_stamp();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .map_err(|e| e.to_string())?;
        use std::io::Write;
        writeln!(f, "{now} {line}").map_err(|e| e.to_string())?;
        Ok(())
    })();
}

fn chrono_stamp() -> String {
    // YYYY-MM-DD HH:MM:SS local, without pulling in chrono for one line.
    // Parses /proc/self/stat? No — use libc localtime via std: build from
    // SystemTime with a tiny civil-date conversion (days -> y/m/d).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    // Local offset: read /etc/localtime? Overkill — log in UTC with a Z-free
    // format identical in shape to Python's. Receipts need order, not zone.
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        m as u32,
        d as u32,
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
        (tod % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stamp_shape() {
        let s = chrono_stamp();
        assert_eq!(s.len(), 19);
        assert_eq!(&s[4..5], "-");
    }
}
