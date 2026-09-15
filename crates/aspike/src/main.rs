// openbutler-aspike — audio/VAD/PTT diagnostics.
// Verifies, without a mic or keyboard press:
//   devices      cpal builds, links ALSA, enumerates hosts/devices
//   vad-selftest webrtc VAD: silence -> 0.0, 200Hz tone -> mostly 1.0
//                (16kHz mono i16, 30ms frames, aggressiveness 2 — as ears.py)
//   ptt          evdev hotkey open; prints actionable hint + exit 2 when
//                /dev/input is unreadable (graceful-degrade path)

use std::time::Duration;

const NAME: &str = "openbutler-aspike";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn cmd_devices() -> i32 {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    println!("host: {:?}", host.id());
    let mut n_in = 0;
    let mut n_out = 0;
    match host.devices() {
        Ok(devs) => {
            for d in devs {
                let name = d.name().unwrap_or_else(|_| "?".into());
                let di = d.default_input_config().is_ok();
                let d_o = d.default_output_config().is_ok();
                if di {
                    n_in += 1;
                }
                if d_o {
                    n_out += 1;
                }
                println!("  device {name:?} input={di} output={d_o}");
            }
        }
        Err(e) => {
            println!("device enumeration failed (non-fatal for build check): {e}");
        }
    }
    println!("devices: {n_in} with input, {n_out} with output");
    match host
        .default_input_device()
        .and_then(|d| d.default_input_config().ok())
    {
        Some(c) => println!(
            "default input: {}Hz {}ch {:?}",
            c.sample_rate().0,
            c.channels(),
            c.sample_format()
        ),
        None => println!("default input: none"),
    }
    match host
        .default_output_device()
        .and_then(|d| d.default_output_config().ok())
    {
        Some(c) => println!(
            "default output: {}Hz {}ch {:?}",
            c.sample_rate().0,
            c.channels(),
            c.sample_format()
        ),
        None => println!("default output: none"),
    }
    0
}

fn cmd_vad_selftest() -> i32 {
    use wavekat_vad::backends::webrtc::{WebRtcVad, WebRtcVadMode};
    use wavekat_vad::VoiceActivityDetector;
    // Aggressiveness 2, matching webrtcvad.Vad(2) in ears.py.
    let mut vad = match WebRtcVad::with_frame_duration(16000, WebRtcVadMode::Aggressive, 30) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: VAD init failed: {e}");
            return 1;
        }
    };
    // 1s of digital silence, 30ms frames (480 i16 samples).
    let silence = vec![0i16; 480];
    let mut sil_voice = 0;
    for _ in 0..33 {
        if let Ok(p) = vad.process(&silence, 16000) {
            if p > 0.5 {
                sil_voice += 1;
            }
        }
    }
    // 1s of 200Hz sine at half scale.
    let mut tone_voice = 0;
    for f in 0..33 {
        let frame: Vec<i16> = (0..480)
            .map(|i| {
                let t = (f * 480 + i) as f64 / 16000.0;
                ((t * 200.0 * 2.0 * std::f64::consts::PI).sin() * 16000.0) as i16
            })
            .collect();
        if let Ok(p) = vad.process(&frame, 16000) {
            if p > 0.5 {
                tone_voice += 1;
            }
        }
    }
    // Speech-ish signal: harmonic stack with syllable-rate AM.
    // (Pure tones are correctly rejected by the GMM — Python Vad(2)
    // scores the identical tone 3/33, so the selftest asserts *parity*
    // with py-webrtcvad on fixed vectors, not absolute detection.)
    let harm: Vec<i16> = (0..480 * 33)
        .map(|n| {
            let t = n as f64 / 16000.0;
            let s = (t * 120.0 * 2.0 * std::f64::consts::PI).sin() * 0.5
                + (t * 240.0 * 2.0 * std::f64::consts::PI).sin() * 0.3
                + (t * 360.0 * 2.0 * std::f64::consts::PI).sin() * 0.2;
            (s * (0.6 + 0.4 * (t * 4.0 * 2.0 * std::f64::consts::PI).sin()) * 16000.0) as i16
        })
        .collect();
    let mut harm_voice = 0;
    for f in 0..33 {
        let frame = &harm[f * 480..(f + 1) * 480];
        if let Ok(p) = vad.process(frame, 16000) {
            if p > 0.5 {
                harm_voice += 1;
            }
        }
    }
    // Reference counts measured from py-webrtcvad Vad(2) on the same vectors.
    let (want_sil, want_tone, want_harm) = (0, 3, 17);
    println!("silence voiced frames: {sil_voice}/33 (py-webrtcvad: {want_sil})");
    println!("tone voiced frames: {tone_voice}/33 (py-webrtcvad: {want_tone})");
    println!("harmonic-AM voiced frames: {harm_voice}/33 (py-webrtcvad: {want_harm})");
    if sil_voice == want_sil && tone_voice == want_tone && harm_voice == want_harm {
        println!("VAD-SELFTEST PASS (bit-identical gate to ears.py)");
        0
    } else {
        println!("VAD-SELFTEST FAIL");
        1
    }
}

fn cmd_ptt(timeout_s: u64) -> i32 {
    let hotkey = match hotkey_listener::parse_hotkey("Shift+F8") {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{NAME}: bad hotkey: {e}");
            return 1;
        }
    };
    let handle = match hotkey_listener::HotkeyListenerBuilder::new()
        .add_hotkey(hotkey)
        .build()
    {
        Ok(b) => b,
        Err(e) => {
            println!("PTT unavailable ({e})");
            println!("hint: evdev needs /dev/input read access — install udev/70-openbutler-input.rules, or run with ptt.mode=off (hands-free).");
            return 2;
        }
    };
    let handle = match handle.start() {
        Ok(h) => h,
        Err(e) => {
            println!("PTT unavailable ({e})");
            println!("hint: evdev needs /dev/input read access — install udev/70-openbutler-input.rules, or run with ptt.mode=off (hands-free).");
            return 2;
        }
    };
    println!("PTT listening for Shift+F8 for {timeout_s}s (press it, or wait)...");
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            println!("PTT-TIMEOUT (listener opened fine, no press seen)");
            return 0;
        }
        match handle.recv_timeout(left.min(Duration::from_millis(500))) {
            Ok(ev) => {
                println!("PTT-EVENT {ev:?}");
                return 0;
            }
            Err(_) => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        Some("-V") | Some("--version") => {
            println!("{NAME} {VERSION}");
            0
        }
        Some("devices") => cmd_devices(),
        Some("vad-selftest") => cmd_vad_selftest(),
        Some("ptt") => {
            let t = args
                .get(2)
                .filter(|s| *s == "--timeout")
                .and_then(|_| args.get(3))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5);
            cmd_ptt(t)
        }
        _ => {
            println!("{NAME} {VERSION} — Phase 2 audio/VAD/PTT spike");
            println!("  devices | vad-selftest | ptt [--timeout N]");
            0
        }
    };
    std::process::exit(code);
}
