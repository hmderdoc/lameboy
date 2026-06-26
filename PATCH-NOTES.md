# BBS divergence notes

How this door differs from upstream
[`terminal_gameboy`](https://github.com/dquigles/terminal_gameboy) (started from
v0.1.2) and why. This is a standalone project, not a tracked fork — these notes
document the divergence for anyone reading the code, not a re-apply recipe.
See SCFG entry `[prog:GAMES:GB]` in `ctrl/xtrn.ini`.

## Patch: CP437 output layer (`src/cp437.rs`)

Synchronet treats door output as CP437. The menu/UI draw Unicode glyphs (box
drawing, the `▀` half-block, arrows); a `Cp437Writer` wraps stdout and maps those
to the matching CP437 bytes (e.g. `▀` → `0xDF`), passing ANSI escapes and ASCII
through untouched. Emitting raw UTF-8 would garble on both CP437 and UTF-8
clients.

## Patch: ANSI-music sound (`src/ansi_music.rs`)

A headless host has no audio, so the Game Boy's lead pulse channel is translated
to SyncTERM ANSI-music (MML) sequences (`ESC[|` … `0x0E`) that the terminal plays
on its beeper. Monophonic and lossy, but it carries the melody. Requires the
vendored `gameboy_core` APU patch (see `vendor/gameboy_core/PATCH-NOTES.md`).

## Patch: ROM browser + per-user state (`src/menu.rs`, `src/config.rs`, `src/save.rs`)

The interactive menu was rebuilt for a large, remote ROM library:
- **Type-ahead** jump-to-title in the game list, with an idle reset.
- **Filter** by Game Boy / Game Boy Color / All; wider name column with a colored
  GB/GBC type tag.
- **Per-user preferences** (render mode, audio, filter) persisted to
  `~/.config/terminal_gameboy/config-<user>`, keyed by the `--user` value.
- **Per-user saves** isolated under `.saves/<user>/` (falling back to the shared
  `.saves/` when no user is given).
- **Esc returns to the menu** rather than exiting the door; the menu runs in a
  loop so quitting a game re-shows the browser.

## Patch: link backpressure pacing (`src/main.rs`)

Upstream repaints the whole screen at 60 fps with blocking writes, which floods a
remote link (the SSH channel window overruns and the door "plays catch-up"). The
transmit rate is now capped (`--fps`, default 20) and a `LinkPace` skips frames in
proportion to write+flush stalls — the emulator keeps full speed and input stays
responsive while frames drop. Mirrors the approach in the spectre door
(`xtrn/spectre/docs/DESIGN.md`).

## Patch: live resize over a door pty (`src/main.rs`)

A Synchronet door's pty winsize is frozen at launch and never gets `SIGWINCH`, so
terminal size is probed ~1/sec via a cursor-position report (`ESC[6n`) instead of
relying on resize events. The probe is skipped while the link is saturated.

## Patch: make audio device optional (`src/main.rs`)

Upstream calls `OutputStream::try_default().expect(...)` unconditionally at startup,
even when `--mute` is passed. On a headless BBS host there is no PCM audio device, so
the process aborts immediately (SIGABRT / exit 134:
`Failed to open audio output: DeviceNotAvailable`).

The patch wraps audio-device init in `if audio_enabled { ... }`, making `_stream`/`sink`
`Option`s, and guards the single `sink.append(source)` use site with `if let Some(s) = sink.as_ref()`.
The door runs with `--mute`, so no device is ever opened.

To re-apply after pulling upstream: search `main.rs` for `OutputStream::try_default()` and
`sink.append(` and reproduce the two edits above.

## Patch: flags work without a positional ROM (`src/main.rs`)

Upstream selects the interactive menu only when `args.len() < 2`, so any flag (e.g. `--mute`)
forces the CLI path and the flag gets misread as the ROM filename. The door is launched as
`terminal_gameboy --mute` with no ROM, so arg parsing was rewritten to:
- parse `--mute` / `--block` / `--ascii` / `--help` as flags regardless of position,
- treat the first non-`-` argument as the ROM path,
- show the menu when no positional ROM is present,
- make `--mute` force audio off even over the menu's Audio=On toggle (so a muted door can
  never open an audio device), and `--block`/`--ascii` override the menu's mode toggle.

This keeps `terminal_gameboy pokemon.gb --block` working as before.

## Build

    cargo build --release
    cp target/release/terminal_gameboy ./terminal_gameboy

`target/` is gitignored (build artifacts).
