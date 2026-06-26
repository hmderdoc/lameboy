# Game Boy — Synchronet BBS Door

A terminal Game Boy / Game Boy Color emulator packaged as a **Synchronet BBS
door**: callers connect over telnet/SSH and play retro games in the BBS, with
CP437 block/ASCII rendering, ANSI-music sound, per-user saves, and a ROM browser
built for slow, remote terminals.

> **Provenance.** This started from
> [`terminal_gameboy`](https://github.com/dquigles/terminal_gameboy) by Dillon
> Quigley (MIT), which renders the [`gameboy_core`](https://crates.io/crates/gameboy_core)
> emulator to a local terminal. It has since **diverged substantially** to run as
> a headless BBS door — see [BBS modifications](#bbs-modifications) and
> [PATCH-NOTES.md](PATCH-NOTES.md). This is a standalone hobby project, not a
> fork tracking upstream; we keep an eye on the original for the occasional
> bugfix but do not intend to stay in sync or merge changes back.

## BBS modifications

What was changed to make a local terminal emulator behave as a multi-user door
over a remote link. Full detail (with file/symbol pointers) is in
[PATCH-NOTES.md](PATCH-NOTES.md).

- **Sound without a sound card** — a door has no local audio device, so the
  Game Boy's audio reaches the caller two ways, selectable in the menu:
  **ANSI** (SyncTERM ANSI-music/MML — universal but monophonic) and **APC**
  (real PCM streamed as SyncTERM audio APCs — full Game Boy audio, on a terminal
  that supports them). Local sound-card playback (rodio) is an optional
  `localaudio` build feature, **off by default**, so the distributed binary is
  pure-Rust with no native audio dependency.
- **CP437 output layer** — the menu/UI Unicode glyphs are emitted as CP437 bytes
  (e.g. the `0xDF` half-block), which is what Synchronet expects from a door;
  raw UTF-8 would garble on both CP437 and UTF-8 clients.
- **Door-friendly launch** — flags work without a positional ROM (the door is
  launched as `terminal_gameboy --mute --ansi-music --user %4` with no ROM), and
  the emulator drops back to the menu on quit instead of exiting the door.
- **ROM browser rebuilt for large libraries** — type-ahead jump, a Game Boy /
  Game Boy Color / All filter, wider names with a GB/GBC type tag, and a
  directory picker.
- **Per-user state** — render mode, audio, and filter preferences persist
  per BBS user, and battery saves are isolated under `.saves/<user>/`.
- **Link backpressure pacing** — the transmit frame rate is capped (default
  20 fps, `--fps N`) and frames are skipped when a slow/congested link can't keep
  up, so the door degrades gracefully instead of flooding the connection.
- **Live resize over a door pty** — a Synchronet door's pty winsize is frozen at
  launch, so terminal size is probed via a cursor-position report rather than
  `SIGWINCH`.

## Install (sysops)

Prebuilt, dependency-free binaries are attached to each
[release](../../releases) — Linux (x86_64 / arm64 / armv7 / i686, static), Windows
(x86_64 / i686), macOS (arm64 / x86_64), and FreeBSD (x86_64). No `libasound2` or
other runtime libs required.

1. Download the archive for your platform and unpack it into your Synchronet
   `xtrn/gb/` directory (so you have `xtrn/gb/terminal_gameboy`).
2. Drop your own legally-obtained `.gb` / `.gbc` ROMs into `xtrn/gb/roms/`.
3. Add the door — either via SCFG → External Programs, or by pasting the stanza
   from [`xtrn.ini.example`](xtrn.ini.example) into `ctrl/xtrn.ini` — then recycle
   the BBS.

Verify a download against `SHA256SUMS.txt` from the release.

## Running as a door

Configured in Synchronet via SCFG (`[prog:GAMES:GB]` in `ctrl/xtrn.ini`); a
ready-to-paste stanza is in [`xtrn.ini.example`](xtrn.ini.example):

```
cmd=./terminal_gameboy --mute --ansi-music --user %4
startup_dir=../xtrn/gb
```

`%4` expands to the zero-padded BBS user number, which keys per-user saves and
preferences. ROMs live in `roms/` next to the binary (gitignored — provide your
own legally obtained `.gb`/`.gbc` files). The **sound mode (Off / ANSI / APC)**
is chosen by the caller in the menu and persists per user; `--ansi-music` enables
the ANSI option.

### Door options

| Flag           | Description                                                  |
| -------------- | ------------------------------------------------------------ |
| `--mute`       | Never open a PCM device (required on a headless host)        |
| `--ansi-music` | Approximate the lead voice via SyncTERM ANSI music           |
| `--user <id>`  | Per-user key for saves + preferences (Synchronet `%4`)       |
| `--fps <n>`    | Transmit frame-rate cap, 5–60 (default 20)                   |
| `--block`      | Force Unicode half-block rendering                           |
| `--ascii`      | Force ASCII-art rendering                                    |

## Building

```bash
cargo build --release                       # door build: pure-Rust, no audio deps
cp target/release/terminal_gameboy ./terminal_gameboy   # deploy next to roms/

cargo build --release --features localaudio # optional: real local sound-card audio
```

The default build omits `rodio`, so the binary is pure-Rust and links no native
audio library — that's what lets release CI cross-compile it for every target.
`--features localaudio` adds rodio for real playback when running locally (needs
ALSA headers on Linux: `libasound2-dev`). The vendored, patched
[`gameboy_core`](vendor/gameboy_core) is built from `vendor/` (see its
`PATCH-NOTES.md` for the APU change that powers ANSI music). `target/`, `roms/`,
and the built binary are gitignored.

Releases are built by [CI](.github/workflows/release.yml) on a `v*` tag.

## Controls

### Menu

| Key            | Action                                  |
| -------------- | --------------------------------------- |
| ↑ / ↓          | Move                                    |
| Type letters   | Jump to a game (type-ahead) in the list |
| ◄ / ► / Space  | Change a setting (render / sound / filter) |
| Z / Enter      | Play the selected game / confirm        |
| R              | Rescan the ROM directory                |
| Esc / Q        | Quit the door                           |

### In-game

| Key        | Action                          |
| ---------- | ------------------------------- |
| Arrow keys | D-Pad                           |
| Z          | A button                        |
| X          | B button                        |
| Enter      | Start                           |
| Space      | Select                          |
| Esc / Q    | Return to the menu              |

## Save files

Battery saves are written automatically on exit and loaded on launch. With a
`--user` key they are isolated under `.saves/<user>/`; without one they fall back
to a shared `.saves/` next to the ROM.

## License

MIT — see [LICENSE](LICENSE). Original work © Dillon Quigley; BBS-door
modifications retain the same license.

## Acknowledgments

- [`terminal_gameboy`](https://github.com/dquigles/terminal_gameboy) — the
  terminal emulator this door is derived from
- [`gameboy_core`](https://crates.io/crates/gameboy_core) — Game Boy emulation
- [`crossterm`](https://crates.io/crates/crossterm) — terminal handling
- [`rodio`](https://crates.io/crates/rodio) — optional local sound-card playback (`localaudio` feature)
