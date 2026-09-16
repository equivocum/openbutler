# Contributing to OpenButler

## Build discipline (learned the hard way — disk + fingerprints)

- Heavy binaries (`stt`, `tts`, `voice`) link C++ (MKL/oneDNN,
  onnxruntime, CTranslate2): build them **in the container**, which has
  g++ (the host typically lacks `libstdc++` and fails at link).
- Never `cargo check` the heavy path: check-profile fingerprints re-run
  the MKL installer (5GB+ transient in `target/`). Use `cargo build`.
- If the MKL install ever fails on disk space, stale
  `target/debug/build/intel-mkl-*` trees (no `mkl/latest`
  inside) are failed-install junk and safe to delete; a tree WITH
  `mkl/latest` is the good one — keep it.
- Light crates (`common`, `face`, `board`, `setup`, `cli`) build on the host:
  `cargo build -p openbutler-common -p openbutler-face -p openbutler-board -p openbutler-setup -p openbutler-cli`.
- `cargo fmt --check` must be clean workspace-wide.

## Run discipline

- Interactive runs (`voice run` with typed/stdin turns) go on **HOST**:
  `toolbox run` (podman exec without `-i`) closes stdin, so piped or
  typed input never arrives in the container.
- The `:8791` single-instance lock is machine-global: a voice line in the
  container blocks one on the host and vice versa. Never run both at once.

## Config discipline

- Never hand-edit `configs/*.json`: use `openbutler config set <key> <value>`
  (writes the live JSON and keeps the `.env` seed in sync; `voice config set`
  still works for tuning keys). Re-render after `.env` edits that render
  owns (`setup init --yes`).
- Templates (`configs/*.example`) stay generic; live files are gitignored.
- Unknown keys in live files are preserved across renders; managed keys
  (`agent_dir`, `name`, `extra_dirs`, `board_state_dir`, `bus_dir`,
  ports, orb paths) are overwritten from `.env`.

## Audio laws (do not "simplify" these)

- ONE long-lived cpal output stream per rate, reused for the life of the
  process (fresh stream per sentence = onset blips / dead air).
- TTS runs in the `openbutler-tts serve` child, never linked into
  `voice` (duplicate protobuf symbols). STT stays in-process (ct2rs).
- Capture entry points are `catch_unwind`'d into device-failure errors:
  a flaky ALSA device must degrade, never take the line down.

## Tests

- Host (light): `cargo test -p openbutler-common -p openbutler-setup -p openbutler-cli`
  covers settings validation/routing, `.env` + render round-trips, unified
  `config` writes to both sides. Container (heavy):
  `toolbox run -c <name> cargo test -p openbutler-voice -p openbutler-tts`
  covers verbs, fuzzy voice matching, spoken-verb persistence. Keep them
  green; add regression tests with every fix.
- One settings core: registry, validation, and file routing live in
  `crates/common/src/settings.rs`, shared by `openbutler config` and
  `voice config` (thin re-exports in `voice/src/console.rs`). Add a
  setting once, there. `setup` owns `.env` + render and exposes them as a
  library for the `openbutler` CLI; runtime crates must never depend on it.
- Comments: block comments on load-bearing invariants only, not every item.
- Live gates before any release: real hands-free turns with latencies in
  `logs/voice.log`, interrupt + post-interrupt turn, quit phrase, clean
  shutdown with no orphaned `serve` children.
