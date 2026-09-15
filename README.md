# OpenButler — a local-first voice companion you can build on

Talk to your machine: hands-free, push-to-talk, or wake-word mode. On-device
speech recognition (Whisper) and speech synthesis (Kokoro) keep your voice
off the cloud; an expressive face UI and a shared visual board round it out.
One Rust workspace, two dependency-free web UIs, no Python anywhere.

## What you get

- **Voice line** (`openbutler-voice`): mic capture with VAD endpointing,
  Whisper STT in-process, an AI brain turn (via the `opencode` CLI),
  Kokoro TTS speech with interruption (barge-in), spoken settings verbs,
  wake-word gating.
- **Face** (`openbutler-face`): local web UI (`ui/face`) with several faces,
  driven by a tiny file bus. Any program can drive it by writing files.
- **Board** (`openbutler-board`): shared visual board (`ui/board`) — cards,
  images, notes, and a 3D-props airlock, plus `cmd` / `state` CLI verbs.
- **Setup** (`openbutler-setup`): first-use wizard and auditor
  (`check` / `init` / `fix`).

## Prerequisites

- **OS**: Fedora with `toolbox`/`podman` (heavy crates link C++ and must
  build inside the container; the host typically lacks `libstdc++` for
  linking). Other Linuxes work too — install the same native toolchain
  (`cmake`, `gcc`, `clang`, ALSA dev headers, `espeak-ng` + dev headers,
  `ffmpeg`) and build natively.
- **Rust**: recent stable via [rustup](https://rustup.rs) (shared into the
  container through `$HOME/.cargo`).
- **`opencode` CLI** on `PATH`, logged in — the brain runs one
  `opencode run` process per turn. No opencode, no answers.
- **Host audio helpers**: `espeak-ng` + `ffmpeg` installed on the host
  itself (`setup check` verifies both).
- **Mic + speaker** (any PipeWire/ALSA device), `curl` for model fetches.
- **Disk + network**: several GB free. First builds download prebuilt
  MKL/oneDNN math libraries; first runs download voice models (see below).

## Install

```bash
git clone <your-fork> && cd openbutler

# 1. System deps inside the build container (idempotent):
toolbox run -c openbutler ./toolbox-setup.sh
# (Use your container name if it differs from .env VOICE_CONTAINER.)

# 2. Build — heavy crates INSIDE the container, light ones anywhere:
toolbox run -c openbutler cargo build
# Host-only alternative for the light crates:
# cargo build -p openbutler-common -p openbutler-face -p openbutler-board -p openbutler-setup

# 3. First-use setup: identity Q&A -> .env -> rendered configs -> models:
./target/debug/openbutler-setup init
./target/debug/openbutler-setup check   # see "expected gaps" below
```

`setup check` audits 12 items and exits 1 on gaps. Two gaps are normal on
a fresh machine, not failures:

- `toolbox`: your container name doesn't exist yet — `toolbox create`
  it, or point `VOICE_CONTAINER` at the one you have.
- `input`: no `/dev/input` access — push-to-talk degrades (hands-free is
  unaffected). Fix with the [push-to-talk](#push-to-talk-needs-one-sudo-step) step.

## Models (fetched, never committed)

Weights live in `~/.cache` (override with `KOKORO_DIR`, `JARVIS_WAKE_DIR`,
`HF_HUB_CACHE`):

| Model | How it arrives | Size |
|---|---|---|
| Wake word (`hey_jarvis` + mel + embedding ONNX) | `setup init` downloads from openWakeWord releases (pins + sha256 in `models/wake.json`) | ~4MB |
| Kokoro TTS (`kokoro-v1.0.onnx` + `voices-v1.0.bin`) | `setup init` fetches from the thewh1teagle `model-files-v1.0` release; manual fallback: drop any `*.onnx` + `voices-v1.0.bin` into `~/.cache/jarvis/kokoro` ([model](https://github.com/taylorchu/kokoro-onnx/releases/tag/v0.2.0), [voices](https://github.com/thewh1teagle/kokoro-onnx/releases/tag/model-files-v1.0)) | ~340MB |
| Whisper STT (`small.en` default) | auto-downloads from HuggingFace Hub on first listen | hundreds of MB |

## Make it yours: pick a name

The **name is set during setup** — `init` asks for it (default
"Assistant"), and you can use anything you like. It shows up in the
spoken greeting, the face and board titles, and — lowercased — in the
hang-up phrase: with name "Butler" you say *"goodbye butler"*.

Change it later without losing tuning: edit `AGENT_NAME` in `.env`, then
`openbutler-setup init --yes` to re-render (managed keys only; your voice,
speed, effort and other tuning survive).

## Launch

```bash
# From a real terminal, not an AI session:
./start.sh            # face + voice
./start.sh --no-face  # voice only
```

- Face: http://127.0.0.1:8790/faces/neural/ (faces: board, neural,
  radial, rain — switch with `config set face.face radial`).
- Talk: just speak — the default is **hands-free** (`open`) mode.
  Say *"goodbye \<name\>"* to hang up. Ctrl-C stops everything.
- Ports (loopback only): `8790` face, `8794` board, `8791` voice lock.
  The `:8791` lock is machine-global — never run two voice lines at once.
- Logs: `logs/voice.log` (brains + latencies), `logs/stts-serve.log`.

### Other mic modes

```bash
./target/debug/openbutler-voice config set mic_mode wake   # "wake word" mode
./target/debug/openbutler-voice config set mic_mode ptt    # push-to-talk
./target/debug/openbutler-voice run --open-mic --barge-in  # one-shot flags
```

Spoken equivalents: "go hands free", "wake word mode", "push to talk
mode". Wake sensitivity: `wake.threshold` / `wake.patience` /
`wake.attention_s`.

### Push-to-talk needs one sudo step

The talk key reads `/dev/input` (evdev), which needs an access rule:

```bash
sudo cp udev/70-openbutler-input.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
# then re-login (or: sudo usermod -aG input $USER)
```

Without it the line still runs — PTT just reports itself unavailable and
hands-free carries on.

## Configure

`openbutler-setup init` renders `configs/{voice,face,board}.json` from
`.env`; re-render safely anytime (`init --yes` keeps tuning — managed keys
only). Or tweak live:

```bash
./target/debug/openbutler-voice config get speed
./target/debug/openbutler-voice config set speed 1.1
./target/debug/openbutler-voice config set voice af_heart
```

| Key | What |
|---|---|
| `voice` / `speed` (0.5–2.0) | TTS voice (54 built in) and rate |
| `effort` | low / medium / high / max reasoning effort |
| `mic_mode` | `open` hands-free (default), `ptt`, `wake` |
| `stt_model` | e.g. `small.en`, `medium.en` (larger = slower, sharper) |
| `wake.threshold` / `patience` / `attention_s` | wake sensitivity, confirmations, follow-up window |
| `face.*` / `board.*` | `face.name`, `face.face`, `face.port`, `board.name`, `board.port` (ports validated 1–65535) |

Spoken equivalents exist for the common ones ("go hands free", "wake word
mode", "push to talk mode", "switch voice to …", "usage report"). `name`
itself is set via `.env` + re-render (see above), not `config set`.

## Extend it

- **New voice verbs**: add a matcher in `crates/voice/src/console.rs`
  (`SETTINGS` for persisted settings, `console_match` for one-shots).
- **New faces**: drop a folder with `index.html` (+ optional `face.json`)
  into `ui/face/faces/` — it is listed and served automatically.
- **New board verbs**: extend `ALLOWED` in `crates/board/src/main.rs`.
- **New voices**: Kokoro ships 54 across 9 languages; point `voice` at any
  of them (`af_*`, `bf_*`, `bm_*`, `ef_*`, `ff_*`, `hf_*`, `if_*`, `jf_*`,
  `pf_*`, `zf_*`).
- **Models**: pins + sha256 in `models/wake.json`; weights live in
  `~/.cache` and are never committed.

See `CONTRIBUTING.md` for build discipline (heavy-binary rules that will
save you gigabytes and hours) and `docs/STATUS.md` for the verified
phase history.

## Troubleshooting

- **`cargo build` fails with `unable to find library -lasound` /
  `libstdc++`**: you're building heavy crates on the host — build them in
  the container (`toolbox run -c <name> cargo build`).
- **MKL install eats the disk**: stale
  `target/debug/build/intel-*-prebuild-*` trees *without* `mkl/latest`
  inside are failed-install junk, safe to delete.
- **`ANOTHER VOICE LINE IS ALREADY RUNNING`**: a line holds `:8791` —
  use that window, or kill the `openbutler-voice` process first.
- **"opencode not found on PATH"**: install + log in the opencode CLI.
- **PTT unavailable**: do the [sudo step](#push-to-talk-needs-one-sudo-step).
- **No mic audio**: check PipeWire/ALSA device + desktop mic-privacy
  settings; `mic_device` in `voice.json` can pin a device by name
  substring.
- **TypeError-ish silence after interrupt**: fixed long ago (stale-stop
  guard) — report with `logs/voice.log` lines if you ever hit a mute.

## License

AGPL-3.0-or-later — see `LICENSE`. Model weights carry their own licenses
(wake graphs: Apache-2.0; Kokoro-82M: Apache-2.0).
