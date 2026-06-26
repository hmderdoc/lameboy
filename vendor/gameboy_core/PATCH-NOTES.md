# Local patches to gameboy_core 0.3.3

Vendored from crates.io (gameboy_core 0.3.3) and used as a path dependency
(`gameboy_core = { path = "vendor/gameboy_core" }` in ../../Cargo.toml).

## Why

The published crate exposes the `sound::{Sound, PulseChannel, ...}` types but gives
no way to reach the live `Sound` instance from the top-level `Gameboy` (the
`emulator`/`mmu` fields are private). The terminal door's ANSI-music engine needs to
read the APU's pulse-channel tone state each frame. These patches add read-only
accessors only -- no emulation logic is changed.

## Changes (search for "Local patch")

- `src/sound/pulse_channel.rs`: `frequency_hz()` (f = 131072 / (2048 - timer_load))
  and `is_voiced()` (enabled && dac_enabled && output_vol > 0).
- `src/sound/mod.rs`: `Sound::pulse1()` / `Sound::pulse2()`.
- `src/emulator/mod.rs`: `Emulator::get_sound() -> &Sound`.
- `src/lib.rs`: `Gameboy::get_sound() -> &Sound`.

To re-apply after a version bump, re-add these four accessors.
