# Game Boy — BBS Door

A terminal Game Boy / Game Boy Color emulator packaged as a **BBS door**:
callers connect over telnet/SSH and play retro games in the BBS, with CP437
block/ASCII rendering, ANSI-music or streamed-PCM sound, per-user saves, and a
ROM browser built for slow, remote terminals. It runs on any BBS that launches
a door over stdio or hands it a connection via a **DOOR32.SYS** dropfile —
Synchronet, EleBBS, Mystic, and friends, on Linux, Windows, macOS, or FreeBSD.

> **Provenance.** This started from
> [`terminal_gameboy`](https://github.com/dquigles/terminal_gameboy) by Dillon
> Quigley (MIT), which renders the [`gameboy_core`](https://crates.io/crates/gameboy_core)
> emulator to a local terminal. It has since **diverged substantially** to run as
> a headless, multi-user BBS door — see [BBS modifications](#bbs-modifications)
> and [PATCH-NOTES.md](PATCH-NOTES.md). This is a standalone hobby project, not a
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
- **Cross-BBS connection layer** — the door talks to the caller over whatever the
  BBS provides: an inherited socket named in a **DOOR32.SYS** dropfile (the
  Windows-BBS norm, where the handle is a Winsock `SOCKET`, not an fd), or stdio.
  Its own input decoder and raw output replace a console-only terminal library, so
  it works the same on a Unix pty, a Windows BBS socket, or a redirected pipe.
- **CP437 output layer** — the menu/UI Unicode glyphs are emitted as CP437 bytes
  (e.g. the `0xDF` half-block), which is what BBS terminals expect from a door;
  raw UTF-8 would garble on both CP437 and UTF-8 clients.
- **Door-friendly launch** — flags work without a positional ROM (the door is
  launched with no ROM and shows its browser), it reads the caller's identity from
  the dropfile, and drops back to the menu on quit instead of exiting.
- **ROM browser rebuilt for large libraries** — type-ahead jump, a Game Boy /
  Game Boy Color / All filter, wider names with a GB/GBC type tag, and a
  directory picker.
- **Per-user state** — render mode, sound, and filter preferences persist per BBS
  user, and battery saves are isolated under `.saves/<user>/`.
- **Link backpressure pacing** — the transmit frame rate is capped (default
  20 fps, `--fps N`) and frames are skipped when a slow/congested link can't keep
  up, so the door degrades gracefully instead of flooding the connection.
- **Live resize** — a door connection delivers no `SIGWINCH`/resize events and a
  door pty's size is frozen at launch, so terminal size is tracked by probing with
  a cursor-position report.

## Install (sysops)

Prebuilt, dependency-free binaries are attached to each
[release](../../releases) — Linux (x86_64 / arm64 / armv7 / i686, static), Windows
(x86_64 / i686), macOS (arm64 / x86_64), and FreeBSD (x86_64). No `libasound2` or
other runtime libs required.

1. Unpack the archive for your platform into a directory under your BBS's external
   programs (e.g. `xtrn/gb/`), so you have `…/gb/terminal_gameboy`.
2. Drop your own legally-obtained `.gb` / `.gbc` ROMs into a `roms/` folder beside
   the binary.
3. Add the door in your BBS's door/external-program config with one of the
   command lines below; see [`xtrn.ini.example`](xtrn.ini.example) for ready-to-paste
   stanzas. Recycle/restart the BBS.

Verify a download against `SHA256SUMS.txt` from the release.

## Running as a door

How the door reaches the caller depends on your BBS:

- **stdio BBSes** (Synchronet and others that connect a door over stdin/stdout):
  ```
  ./terminal_gameboy --mute --ansi-music --user %4
  ```
  `%4` is Synchronet's specifier for the zero-padded user number (use your BBS's
  equivalent), which keys per-user saves and preferences.

- **DOOR32.SYS BBSes** (EleBBS, Mystic, Windows BBSes, …) hand the door an
  inherited socket via a dropfile — pass its path with `--dropfile`. The user
  identity is read from the dropfile, so no `--user` is needed:
  ```
  terminal_gameboy.exe --mute --ansi-music --dropfile <path-to-DOOR32.SYS>
  ```

ROMs live in `roms/` next to the binary (gitignored — provide your own). The
**sound mode (Off / ANSI / APC)** is chosen by the caller in the menu and persists
per user; `--ansi-music` enables the ANSI option, APC streams full PCM to terminals
that support SyncTERM audio APCs.

### Door options

| Flag                | Description                                                   |
| ------------------- | ------------------------------------------------------------- |
| `--dropfile <path>` | DOOR32.SYS dropfile: use its inherited socket + user identity |
| `--user <id>`       | Per-user key for saves + preferences (e.g. Synchronet `%4`)   |
| `--mute`            | Never open a local sound device (required on a headless host) |
| `--ansi-music`      | Allow ANSI-music sound (caller still picks Off/ANSI/APC)      |
| `--fps <n>`         | Transmit frame-rate cap, 5–60 (default 20)                    |
| `--block`           | Force Unicode half-block rendering                            |
| `--ascii`           | Force ASCII-art rendering                                     |

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
