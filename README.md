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

- **Headless audio** — upstream opens a PCM device unconditionally and aborts on
  a server with no sound card. Audio-device init is now optional; the door runs
  `--mute` and never opens a device.
- **ANSI music** — since there's no real audio, the Game Boy's lead pulse channel
  is approximated as SyncTERM ANSI-music (MML) so callers still hear the melody.
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

## Running as a door

Configured in Synchronet via SCFG (`[prog:GAMES:GB]` in `ctrl/xtrn.ini`):

```
cmd=./terminal_gameboy --mute --ansi-music --user %4
startup_dir=../xtrn/gb
```

`%4` expands to the zero-padded BBS user number, which keys per-user saves and
preferences. ROMs live in `roms/` next to the binary (gitignored — provide your
own legally obtained `.gb`/`.gbc` files).

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
cargo build --release
cp target/release/terminal_gameboy ./terminal_gameboy   # deploy next to roms/
```

The vendored, patched [`gameboy_core`](vendor/gameboy_core) is built from
`vendor/` (see its `PATCH-NOTES.md` for the APU change that powers ANSI music).
`target/`, `roms/`, and the built binary are gitignored.

## Controls

### Menu

| Key            | Action                                  |
| -------------- | --------------------------------------- |
| ↑ / ↓          | Move                                    |
| Type letters   | Jump to a game (type-ahead) in the list |
| ◄ / ► / Space  | Change a setting (render / audio / filter) |
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
- [`rodio`](https://crates.io/crates/rodio) — audio playback
