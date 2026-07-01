#[macro_use]
mod out;
mod ansi_music;
mod apc_audio;
#[cfg(feature = "localaudio")]
mod audio;
mod color;
mod config;
mod door32;
mod term;
mod keys;
mod cp437;
mod framebuffer;
mod input;
mod menu;
mod renderer;
mod save;
mod splash;

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use gameboy_core::{Gameboy, RTC, StepResult};
#[cfg(feature = "localaudio")]
use rodio::{OutputStream, Sink};
use std::io::{self, Write};
use std::path::Path;
#[cfg(feature = "localaudio")]
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "localaudio")]
use audio::{AudioBuffer, GameboyAudioSource};
use color::ColorSetting;
use framebuffer::{DmgPalette, FrameBuffer, GB_HEIGHT, GB_WIDTH};
use input::{evdev_to_button, map_key_to_button, EVDEV_ESC, EVDEV_Q};
use keys::{Input, Key, KeyboardMode};
use menu::{show_menu, SoundMode};
use renderer::{RenderConfig, RenderMode, Renderer};
use save::{get_save_path, load_save, save_game};
use term::Term;

// Button count and index mapping (faster than HashMap for 8 buttons)
const BUTTON_COUNT: usize = 8;

#[inline]
fn button_index(button: gameboy_core::Button) -> usize {
    use gameboy_core::Button;
    match button {
        Button::A => 0,
        Button::B => 1,
        Button::Start => 2,
        Button::Select => 3,
        Button::Up => 4,
        Button::Down => 5,
        Button::Left => 6,
        Button::Right => 7,
    }
}

// Colors. The FPS green is quantized to the active depth at emit time (see the
// overlay in run_game) so it isn't a raw truecolor escape on a lesser terminal.
const RESET: &str = "\x1b[0m";

/// Default transmit frame rate (Hz). The emulator always runs at full Game Boy
/// speed; this only caps how often we repaint the terminal, which bounds the
/// bandwidth sent over a remote link. Tunable with `--fps N`.
const DEFAULT_RENDER_FPS: f64 = 20.0;

/// Adapts the transmit frame rate to what the link can actually carry.
///
/// Frames go out through blocking writes, so when the connection degrades
/// (packet loss, saturated uplink) the write+flush stalls — that stall is the
/// backpressure signal. `note()` times each frame's write; on an overrun the
/// next frames are skipped in proportion (the emulator keeps running and input
/// stays responsive) before probing the link again. Mirrors the `linkPace`
/// approach proven in the spectre door (see xtrn/spectre/docs/DESIGN.md).
struct LinkPace {
    budget: Duration, // the per-frame transmit time slice
    skip: u32,        // frames left to skip before the next probe write
}

impl LinkPace {
    fn new(budget: Duration) -> Self {
        Self { budget, skip: 0 }
    }

    /// True while we are deliberately dropping frames to let the link recover.
    fn skipping(&self) -> bool {
        self.skip > 0
    }

    /// Reports whether the caller should skip transmitting this frame.
    fn skip_frame(&mut self) -> bool {
        if self.skip > 0 {
            self.skip -= 1;
            true
        } else {
            false
        }
    }

    /// Record how long a frame's write+flush took. Anything within 2x budget
    /// counts as keeping up (a little jitter shouldn't throttle us); past that,
    /// skip upcoming frames in proportion to the overrun, capped at ~2s so a
    /// long stall doesn't park the renderer once the link recovers.
    fn note(&mut self, d: Duration) {
        if d <= self.budget * 2 {
            return;
        }
        let budget_ns = self.budget.as_nanos().max(1);
        let mut skip = (d.as_nanos() / budget_ns) as u32;
        let limit = (Duration::from_secs(2).as_nanos() / budget_ns) as u32;
        if skip > limit {
            skip = limit;
        }
        self.skip = skip;
    }
}

struct SimpleRTC;

impl RTC for SimpleRTC {
    fn get_current_time(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
    }
}


fn print_usage(program: &str) {
    eprintln!("ASCII GameBoy Emulator");
    eprintln!();
    eprintln!("Usage: {} <rom_file.gb> [OPTIONS]", program);
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --ascii       ASCII art mode with brightness characters (default)");
    eprintln!("  --block       Unicode block mode with solid half-blocks");
    eprintln!("  --color MODE  Output color depth: auto (default) | truecolor | 256 | 16");
    eprintln!("  --dmg PAL     Non-color (DMG) game palette: gray (default) | green");
    eprintln!("  --mute        Disable audio output");
    eprintln!("  --ansi-music  Approximate music via ANSI/terminal-beeper tones");
    eprintln!();
    eprintln!("Controls:");
    eprintln!("  Arrow keys    D-Pad");
    eprintln!("  Z             A button");
    eprintln!("  X             B button");
    eprintln!("  Enter         Start");
    eprintln!("  Space         Select");
    eprintln!("  Q / Esc       Quit");
}

/// Ask the terminal for its size: park the cursor at the far corner, then request
/// a cursor-position report (`ESC[6n`). The reply (`ESC[row;colR`) is read back
/// through the normal input path and surfaced by `Input::take_cursor`.
///
/// We probe rather than trust an ioctl/winsize because a door's pty size is
/// frozen at launch (and an inherited socket has no winsize at all), so this
/// round-trip is the only way to track the caller's real terminal size.
///
/// When `with_caps` is set, the keyboard-capability probes are folded into the
/// same round-trip (sent until detection resolves, see `Input::caps_resolved`):
/// CTerm Device Attributes (`CSI < c` — physical key reports), the kitty keyboard
/// query (`CSI ? u`), and a Primary DA (`CSI c`) that acts as the reply barrier.
/// Terminals that don't understand a query simply ignore it; all replies are
/// parsed by `KeyDecoder` and never surface as keystrokes.
pub(crate) fn send_size_probe<W: Write + ?Sized>(term: &mut W, with_caps: bool) -> io::Result<()> {
    emit!(term, MoveTo(9998, 9998))?;
    term.write_all(b"\x1b[6n")?;
    if with_caps {
        // Keyboard-capability queries, plus the DECRQSS color probe: set an odd
        // 24-bit fg, ask the terminal to read the active SGR back (`ESC P $ q m
        // ESC \`), then reset. Nothing is drawn. A truecolor terminal echoes
        // `38;2;...`; one that only does 256 quantizes it to `38;5;N` (see
        // `parse_color_readback`). The Primary DA (`ESC [ c`) closes the burst.
        term.write_all(b"\x1b[<c\x1b[?u\x1b[38;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\x1b[c")?;
    }
    term.flush()
}

/// Resolve which keyboard protocol the caller speaks. Detection normally
/// completes during the menu (its size probes carry the capability queries);
/// for a direct ROM launch with no menu, pump input briefly — swallowing the
/// probe replies — until the DA barrier arrives or a short timeout elapses.
fn detect_keyboard(term: &mut dyn Term, input: &mut Input) -> io::Result<KeyboardMode> {
    if !input.caps_resolved() {
        let _ = send_size_probe(&mut *term, true);
        let deadline = Instant::now() + Duration::from_millis(300);
        while !input.caps_resolved() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(15));
            let _ = input.poll(term)?; // feed the replies (no game keys yet)
        }
    }
    Ok(input.keyboard_mode())
}

/// Enable CTerm physical key reports (`CSI = 1 h`) and suppress the duplicate
/// translated input (`CSI = 2 h`) so the game reads only raw evdev edges.
fn enable_physical_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[=1h\x1b[=2h")?;
    term.flush()
}

/// Restore normal translated input (`CSI = 2 l`) and stop physical key reports
/// (`CSI = 1 l`). Safe to call even if they were never enabled.
fn disable_physical_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[=1l\x1b[=2l")?;
    term.flush()
}

/// Push the kitty keyboard flags `2|8` (report event types + all keys as escape
/// codes) so modern terminals send press/release edges for every key.
fn enable_kitty_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[>10u")?;
    term.flush()
}

/// Pop the kitty keyboard flags (`CSI < u`), restoring the host's prior mode.
fn disable_kitty_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[<u")?;
    term.flush()
}

/// Ask the terminal to resize its text area to `rows`x`cols` (xterm
/// `CSI 8 ; rows ; cols t`). xterm-family terminals honor it; SyncTERM/CTerm
/// ignore it harmlessly (their `CSI ... t` is 24-bit colour and needs 4 params).
pub(crate) fn resize_terminal<W: Write + ?Sized>(term: &mut W, rows: u16, cols: u16) -> io::Result<()> {
    write!(term, "\x1b[8;{};{}t", rows, cols)?;
    term.flush()
}

/// The "friendly" part of a ROM file name for display: the title before any
/// parenthesized region/metadata, and without the extension. E.g.
/// "Super Mario Land 2 - 6 Golden Coins (USA, Europe).gb"
///   -> "Super Mario Land 2 - 6 Golden Coins".
fn friendly_rom_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    stem.split('(').next().unwrap_or(&stem).trim().to_string()
}

/// Truncate to at most `max` chars, appending ".." when cut (ASCII only — this
/// goes out raw over the CP437 channel, so no Unicode ellipsis).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(2);
    s.chars().take(keep).collect::<String>() + ".."
}

/// Draw the in-game bottom status line on `row`: FPS docked at the left, then a
/// centered message. Normal play shows the key hints (key labels light-gray,
/// separators dark-gray, actions white); `attract = Some(name)` shows the game's
/// friendly name + a take-over prompt instead. All quantized to `depth`, kept
/// clear of the FPS, and never writing the last column (autowrap is off
/// session-wide, but staying in bounds is belt-and-suspenders against the
/// bottom-row scroll trap).
fn draw_status_bar<W: Write + ?Sized>(
    term: &mut W,
    depth: color::ColorDepth,
    row: u16,
    width: u16,
    fps: f32,
    fps_color: &str,
    attract: Option<&str>,
) -> io::Result<()> {
    const LIGHT: (u8, u8, u8) = (170, 170, 170); // key labels
    const DARK: (u8, u8, u8) = (85, 85, 85); // separators (incl. the "/")
    const WHITE: (u8, u8, u8) = (255, 255, 255); // actions / buttons / title
    let w = width as usize;

    // Build the centered content as (text, color) spans. Attract mode shows the
    // title (truncated to fit) + a take-over prompt; normal play shows the hints.
    let title = attract.map(|name| truncate_chars(name, w.saturating_sub(34).max(8)));
    let content: Vec<(&str, (u8, u8, u8))> = if let Some(t) = title.as_deref() {
        vec![(t, WHITE), (" - press any key to play", LIGHT)]
    } else {
        vec![
            ("ARROWS", LIGHT), (" - ", DARK), ("D-Pad", WHITE), (" | ", DARK),
            // Keys light, buttons white, the slash dark like the other separators.
            ("Z", LIGHT), ("/", DARK), ("X", LIGHT), (" - ", DARK), ("B", WHITE), ("/", DARK), ("A", WHITE), (" | ", DARK),
            ("ENTER", LIGHT), (" - ", DARK), ("Start", WHITE), (" | ", DARK),
            ("SPACE", LIGHT), (" - ", DARK), ("Select", WHITE), (" | ", DARK),
            ("Q", LIGHT), (" - ", DARK), ("Quit", WHITE),
        ]
    };

    // Clear the whole row to a solid black background first, so the content always
    // renders on black and never inherits a stray cell color underneath (which can
    // make even white text look muddy). `\x1b[K` fills to end-of-line with the
    // active bg; autowrap is off session-wide so this can't scroll the last row.
    emit!(term, MoveTo(0, row))?;
    term.write_all(b"\x1b[40m\x1b[K")?;
    // FPS at the left (fg only; the black bg from above stays active). Fixed width.
    write!(term, "{}FPS {:>2.0}", fps_color, fps)?;

    // Center it, but never under the FPS, and never onto the last column (last
    // drawn column must be <= width-2). Skip it if it doesn't fit.
    let content_len: usize = content.iter().map(|(t, _)| t.chars().count()).sum();
    if w > content_len {
        let start = ((w - content_len) / 2).max(8);
        if start + content_len <= w.saturating_sub(1) {
            emit!(term, MoveTo(start as u16, row))?;
            for &(t, (r, g, b)) in &content {
                write!(term, "\x1b[{}m{}", color::fg_sgr(depth, r, g, b), t)?;
            }
        }
    }
    term.write_all(RESET.as_bytes())?;
    term.flush()
}

/// Extract a `--flag <value>` / `--flag=<value>` argument value, if present.
fn parse_value(args: &[String], flag: &str) -> Option<String> {
    let eq = format!("{}=", flag);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix(eq.as_str()) {
            return Some(v.to_string()).filter(|s| !s.is_empty());
        }
        if a == flag {
            return it.next().cloned().filter(|s| !s.is_empty());
        }
    }
    None
}

/// Extract the `--dropfile <path>` value (a DOOR32.SYS).
fn parse_dropfile(args: &[String]) -> Option<String> {
    parse_value(args, "--dropfile")
}

/// Extract the `--user <id>` / `--user=<id>` value, if present. This is the
/// BBS user number (Synchronet substitutes `%4` etc. in the door command line)
/// used to isolate saves and persist per-user preferences.
fn parse_user(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(val) = a.strip_prefix("--user=") {
            return Some(val.to_string()).filter(|s| !s.is_empty());
        }
        if a == "--user" {
            return it.next().cloned().filter(|s| !s.is_empty());
        }
    }
    None
}

/// Extract the `--fps <n>` / `--fps=<n>` transmit cap, clamped to [5, 60].
fn parse_fps(args: &[String]) -> Option<f64> {
    let mut it = args.iter();
    let raw = loop {
        let a = it.next()?;
        if let Some(val) = a.strip_prefix("--fps=") {
            break val.to_string();
        }
        if a == "--fps" {
            break it.next()?.to_string();
        }
    };
    raw.parse::<f64>().ok().map(|f| f.clamp(5.0, 60.0))
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Parse flags independently of the positional ROM argument, so the
    // interactive menu can be reached even when global flags like --mute are
    // present (needed when running as a headless BBS door: the door is launched
    // as `terminal_gameboy --mute`, with no ROM, and shows its ROM browser).
    // (Local patch -- see PATCH-NOTES.md)
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage(&args[0]);
        std::process::exit(0);
    }
    // Sysop defaults from lameboy.ini (next to the binary). Command-line flags
    // override these, so the usual door command line stays short.
    let ini = config::load_door_ini();
    // Capture before `ini` is partially moved (roms) below.
    let apc_tuning = ini.apc_tuning();
    let attract_cfg = ini.attract_cfg();

    // The released door build has no local audio device, so --mute is effectively
    // always on; it's still honored for `localaudio` builds and back-compat.
    let force_mute = args.iter().any(|a| a == "--mute");
    // ANSI music is offered by default (the caller still chooses Off/ANSI/APC in
    // the menu); a sysop can disable it with `ansi_music = false` in lameboy.ini,
    // and `--ansi-music` forces it on.
    let ansi_music_flag =
        args.iter().any(|a| a == "--ansi-music") || ini.ansi_music.unwrap_or(true);
    let cli_mode = if args.iter().any(|a| a == "--block") {
        Some(RenderMode::Block)
    } else if args.iter().any(|a| a == "--ascii") {
        Some(RenderMode::Ascii)
    } else {
        None
    };
    let render_fps = parse_fps(&args).or(ini.fps).unwrap_or(DEFAULT_RENDER_FPS);

    // Color depth default: `--color <auto|truecolor|256|16>` (test/sysop knob),
    // else the `color =` sysop ini default, else Auto (probe + truecolor default).
    // A caller's saved per-user pref, chosen in the menu, overrides this.
    let color_default = parse_value(&args, "--color")
        .and_then(|s| ColorSetting::parse(&s))
        .or_else(|| ini.color.as_deref().and_then(ColorSetting::parse))
        .unwrap_or(ColorSetting::Auto);

    // DMG (non-color game) palette default: `--dmg <gray|green>` (test/sysop knob).
    // A caller's saved per-user pref, chosen in the menu, overrides this.
    let dmg_default = parse_value(&args, "--dmg")
        .and_then(|s| DmgPalette::parse(&s))
        .unwrap_or_default();

    // DOOR32.SYS dropfile: on a BBS that doesn't pass the connection via stdio
    // (e.g. EleBBS/Mystic/Synchronet Win32), it names the inherited socket and
    // the user. The per-user key is the explicit --user, else the dropfile's
    // user number, else none.
    let door = parse_dropfile(&args)
        .as_deref()
        .map(Path::new)
        .and_then(door32::read);
    let user_id = parse_user(&args).or_else(|| door.as_ref().and_then(|d| d.user_key()));
    // ROM directory is a sysop setting: --roms <path>, else lameboy.ini, else
    // the persisted/default location.
    let roms_override = parse_value(&args, "--roms").or(ini.roms);

    // First non-flag argument (if any) is the ROM path. Skip the value that
    // follows a value-taking flag so it isn't mistaken for a ROM path.
    let positional_rom = {
        let mut found = None;
        let mut skip_next = false;
        for a in args.iter().skip(1) {
            if skip_next {
                skip_next = false;
                continue;
            }
            if matches!(a.as_str(), "--user" | "--fps" | "--dropfile" | "--roms" | "--color" | "--dmg") {
                skip_next = true;
                continue;
            }
            if !a.starts_with('-') {
                found = Some(a.clone());
                break;
            }
        }
        found
    };

    // Open the caller connection (inherited socket from the dropfile, or stdio)
    // and enter the alternate screen once for the whole session. The Term owns
    // raw mode and restores it on drop.
    let mut term = term::open(door.as_ref())?;
    let mut input = Input::new();
    emit!(term, EnterAlternateScreen, Hide, Clear(ClearType::All))?;
    // Autowrap off for the whole session: painting the bottom-right cell (a
    // full-screen splash, or a status line on the last row) then can't scroll the
    // screen. Everything positions with absolute cursor moves, so nothing relies
    // on wrap. Restored on exit.
    term.write_all(b"\x1b[?7l")?;
    term.flush()?;

    let result = run_session(
        &mut *term,
        &mut input,
        positional_rom,
        cli_mode,
        force_mute,
        ansi_music_flag,
        user_id.as_deref(),
        roms_override.as_deref(),
        render_fps,
        apc_tuning,
        color_default,
        dmg_default,
        attract_cfg,
    );

    // Restore the screen no matter how the session ended; Term drop restores raw.
    // Also force-restore translated keyboard input in case a game exited via an
    // error path before turning off an enhanced keyboard mode — otherwise the
    // caller's BBS keyboard would stay suppressed after the door returns.
    let _ = disable_physical_keys(&mut *term);
    let _ = disable_kitty_keys(&mut *term);
    let _ = term.write_all(b"\x1b[?7h"); // restore autowrap
    let _ = emit!(term, Show, LeaveAlternateScreen);
    let _ = term.flush();
    result
}

/// Run either a one-shot positional ROM or the interactive menu loop, over the
/// already-open terminal. Quitting a game (Esc/Q) returns to the menu.
#[allow(clippy::too_many_arguments)]
fn run_session(
    term: &mut dyn Term,
    input: &mut Input,
    positional_rom: Option<String>,
    cli_mode: Option<RenderMode>,
    force_mute: bool,
    ansi_music_flag: bool,
    user_id: Option<&str>,
    roms_override: Option<&str>,
    render_fps: f64,
    apc_tuning: config::ApcTuning,
    color_default: ColorSetting,
    dmg_default: DmgPalette,
    attract_cfg: config::AttractCfg,
) -> io::Result<()> {
    if let Some(rom) = positional_rom {
        let mode = cli_mode.unwrap_or(RenderMode::Ascii);
        // A direct ROM launch has no menu, so take the color/palette defaults from
        // the CLI/ini. run_game resolves the depth after the probe answers.
        return run_game(
            term, input, &Path::new(&rom).to_path_buf(), mode,
            !force_mute, ansi_music_flag, false, user_id, render_fps, apc_tuning,
            color_default, dmg_default, false, 0,
        )
        .map(|_| ());
    }

    // Startup splash (once): the lameboy_splash.bin graphic, dismissed by any key
    // or after 10s. It also front-loads size/keyboard/color detection so the menu
    // opens already at the caller's real size and color depth. With no splash
    // graphic present, fall back to the built-in wordmark intro animation.
    let splashed = splash::show_splash(term, input, color_default)?;

    let mut animate = !splashed;
    // Set by the menu's "Best" screen-size mode: the caller's original terminal
    // size, captured before we resized, so we can restore it when the door exits.
    // Persists across the menu↔game cycle so a game keeps the bigger screen.
    let mut screen_orig: Option<(u16, u16)> = None;
    // Attract mode: after an attract game ends on its own (no takeover), the next
    // menu is a short pause before the next random game; otherwise a normal entry.
    let mut attract_pause = false;
    // When the caller takes over an attract game, the next menu opens on that ROM
    // rather than their last explicitly-launched one.
    let mut select_rom: Option<std::path::PathBuf> = None;
    loop {
        // Attract thresholds for this menu entry: (first, subsequent). A pause
        // uses the short menu interval until the caller engages, then the full
        // idle interval. None disables attract mode.
        let attract = if attract_cfg.enabled {
            let idle = Duration::from_secs(attract_cfg.idle_secs);
            let first = if attract_pause {
                Duration::from_secs(attract_cfg.menu_secs)
            } else {
                idle
            };
            Some((first, idle))
        } else {
            None
        };
        match show_menu(
            term, input, user_id, roms_override, animate, &mut screen_orig,
            color_default, dmg_default, attract, select_rom.take(),
        )? {
            Some(config) => {
                let attract_launch = config.attract;
                let mode = cli_mode.unwrap_or(config.render_mode);
                let want_apc = config.sound == SoundMode::Apc;
                // Local PCM device (rodio) only for a non-APC, non-Off mode when
                // not muted. A headless door never opens a device; APC instead
                // streams PCM to the caller.
                let audio_enabled =
                    !force_mute && config.sound != SoundMode::Off && !want_apc;
                let music_enabled = ansi_music_flag && config.sound == SoundMode::Ansi;
                let intervened = run_game(
                    term, input, &config.rom_path, mode, audio_enabled,
                    music_enabled, want_apc, user_id, render_fps, apc_tuning,
                    config.color, config.dmg_palette, attract_launch, attract_cfg.game_secs,
                )?;
                // An attract game that ran its full time (no takeover) queues the
                // next one after a short menu pause; anything else is a normal
                // return to the menu with the full idle countdown.
                attract_pause = attract_launch && !intervened;
                // Taking over an attract game: open the next menu on that ROM.
                select_rom = (attract_launch && intervened).then(|| config.rom_path.clone());
                animate = false;
            }
            None => break,
        }
    }
    // Restore the caller's original terminal size if "Best" resized it.
    if let Some((rows, cols)) = screen_orig {
        let _ = resize_terminal(&mut *term, rows, cols);
    }
    Ok(())
}

/// Run a single ROM to completion. Returns when the player quits the game
/// (Esc/Q), at which point the caller decides whether to re-show the menu.
#[allow(clippy::too_many_arguments)]
fn run_game(
    term: &mut dyn Term,
    input: &mut Input,
    rom_path: &Path,
    mode: RenderMode,
    audio_enabled: bool,
    music_enabled: bool,
    apc_enabled: bool,
    user_id: Option<&str>,
    render_fps: f64,
    apc_tuning: config::ApcTuning,
    color_setting: ColorSetting,
    dmg_palette: DmgPalette,
    // Attract mode: run for at most `attract_game_secs`, and treat any key as the
    // caller taking over. Returns whether the caller intervened (pressed a key);
    // false means the attract timer elapsed. Non-attract launches always return
    // true (the caller quit with Esc/Q).
    attract: bool,
    attract_game_secs: u64,
) -> io::Result<bool> {
    let rom = std::fs::read(rom_path).expect("Failed to read ROM file");

    let mut gameboy =
        Gameboy::from_rom(rom, Box::new(SimpleRTC)).expect("Failed to load ROM");

    // Load existing save file if present
    if let Ok(true) = load_save(&mut gameboy, rom_path, user_id) {
        eprintln!("Loaded save file: {}", get_save_path(rom_path, user_id).display());
        std::thread::sleep(Duration::from_millis(500)); // Brief pause to show message
    }

    // Local PCM playback via rodio is an optional build feature (`localaudio`).
    // The distributed door binary is built WITHOUT it: a door never plays to a
    // local sound card (it streams ANSI/APC audio to the caller), and dropping
    // rodio makes the binary pure-Rust and dependency-free (no libasound2),
    // which is what lets CI cross-compile it for every sysop target. Force the
    // device path off when the feature is absent.
    let audio_enabled = audio_enabled && cfg!(feature = "localaudio");

    // Setup audio output (lock-free buffer). Only open a device when enabled;
    // unconditionally calling OutputStream::try_default() aborts on a headless
    // host. (Local patch -- see PATCH-NOTES.md)
    #[cfg(feature = "localaudio")]
    let audio_buffer = Arc::new(AudioBuffer::new(16384));
    #[cfg(feature = "localaudio")]
    let (_stream, sink) = if audio_enabled {
        let (stream, stream_handle) =
            OutputStream::try_default().expect("Failed to open audio output");
        let sink = Sink::try_new(&stream_handle).expect("Failed to create audio sink");
        (Some(stream), Some(sink))
    } else {
        (None, None)
    };
    // Pre-buffer some frames before starting audio to avoid initial underruns
    #[cfg(feature = "localaudio")]
    let mut pre_buffer_frames = if audio_enabled { 3 } else { 0 };

    // The alternate screen and raw mode are owned by the caller (main) and shared
    // with the menu. Just clear for this game; start from a sane default size and
    // let the live probe below refine it from the caller's real terminal.
    emit!(term, Clear(ClearType::All))?;

    let mut framebuffer = FrameBuffer::new(GB_WIDTH, GB_HEIGHT);
    let _ = send_size_probe(&mut *term, !input.caps_resolved());

    // Resolve the keyboard protocol and switch on the enhanced mode if any:
    // CtermPhysical (SyncTERM) or Kitty (modern terminals) both yield real
    // key-down/up edges and simultaneous keys; the decoder normalises kitty to
    // the same evdev codes, so the in-game edge path is shared. The same probe
    // burst also carries the color probe, so its reply is in by the time we
    // resolve the depth below.
    let kb_mode = detect_keyboard(&mut *term, input)?;
    let edge_input = matches!(kb_mode, KeyboardMode::CtermPhysical | KeyboardMode::Kitty);
    match kb_mode {
        KeyboardMode::CtermPhysical => {
            let _ = enable_physical_keys(&mut *term);
        }
        KeyboardMode::Kitty => {
            let _ = enable_kitty_keys(&mut *term);
            input.set_kitty_active(true);
        }
        KeyboardMode::Legacy => {}
    }

    // Resolve the output color depth now that the probe has (or hasn't) answered,
    // then build the renderer. Auto stays truecolor -> emits 256 for the game
    // (the game's top tier); an explicit 16 takes the classic-ANSI path.
    let depth = color_setting.resolve(input.color_probe());
    // Apply the DMG palette now that the depth is known — green is forced to gray
    // in 16-color (its greens don't quantize well; grayscale hits the ANSI grays).
    framebuffer.set_dmg_palette(dmg_palette.resolved_for(depth));
    let fps_color = format!("\x1b[{}m", color::fg_sgr(depth, 80, 200, 80));
    let mut renderer = Renderer::new(RenderConfig { mode, depth });
    renderer.update_dimensions(80, 24);

    // Use precise floating-point duration for accurate 60 FPS (16.667ms, not 16ms).
    // This paces the *emulator* — the game always runs at full Game Boy speed.
    let frame_duration = Duration::from_secs_f64(1.0 / 60.0);

    // Transmit pacing: repaint the terminal at most `render_fps` times/sec
    // (bounding bandwidth), and skip frames when the link can't keep up.
    let render_interval = Duration::from_secs_f64(1.0 / render_fps.clamp(1.0, 60.0));
    let mut last_render = Instant::now() - render_interval; // render immediately on entry
    let mut pace = LinkPace::new(render_interval);

    // Simulation clock: emulate every GB frame whose wall-clock time has come,
    // rather than one-frame-per-loop + sleep. thread::sleep reliably oversleeps
    // (~16.67ms requested -> ~20ms actual), which otherwise runs the emulator —
    // and thus audio production — below realtime, starving the APC stream. Up to
    // MAX_CATCHUP frames are repaid per loop; beyond that we resync (bounded).
    const MAX_CATCHUP: u32 = 12;
    let mut sim_deadline = Instant::now();

    // Periodic keyframe: force a full (clear-less) repaint every few seconds so a
    // cell corrupted by line noise on a lossy link self-heals — delta encoding
    // otherwise only repaints cells the game itself changes.
    let keyframe_interval = Duration::from_secs(7);
    let mut keyframe_timer = Instant::now();

    // FPS tracking (counts frames actually transmitted)
    let mut frame_count = 0u32;
    let mut fps_timer = Instant::now();
    let mut last_fps = 0.0f32;
    // Redraw the bottom status bar (FPS + key hints) on the next frame — set on
    // the first frame and after a resize (both of which clear the whole screen).
    let mut redraw_status = true;

    // Live-resize polling: a door connection delivers no SIGWINCH/resize events,
    // so we periodically probe the terminal for its size (see send_size_probe).
    let mut size_timer = Instant::now();
    let mut last_size = (80u16, 24u16);

    // ANSI-music engine: approximates the lead pulse channel via terminal beeps.
    let mut ansi = ansi_music::AnsiMusic::new(music_enabled);

    // APC PCM streaming: capture the emulator's audio and ship it to a
    // SyncTERM-APC-capable terminal as chunked clips (~120 ms).
    // Monotonic clock for the APC audio timeline (realtime playback estimate).
    let audio_clock = Instant::now();
    let mut apc = if apc_enabled {
        // The streamer reconciles each clip against wall-clock (speed up small
        // drift, drop big stalls), so there is no cushion to tune. chunk_ms is the
        // min clip / drop granularity; rate is the bandwidth lever (lower = fewer
        // bytes on the shared link). Both are sysop-set in lameboy.ini.
        Some(apc_audio::ApcAudio::new(apc_tuning.chunk_ms, apc_tuning.rate))
    } else {
        None
    };
    // APC audio diagnostics go to a file (NOT stdout/stderr — in a door those
    // are the caller's socket and would corrupt the screen). Best-effort,
    // truncated each session; absent if it can't be opened.
    let mut apc_log = if apc.is_some() {
        std::fs::File::create("apc_audio.log").ok()
    } else {
        None
    };
    let mut apc_log_timer = Instant::now();
    // Periodic APC audio resync ("engine restart"): the producer runs a hair
    // faster than realtime, so the terminal's channel FIFO accrues an unplayed
    // tail (audio drifting behind video) that the baseline emit_ready can't see or
    // clear. Every `resync_secs` we hard-flush that channel and re-anchor, capping
    // the tail. 0 disables. Only meaningful when APC is streaming.
    let resync_interval = match apc_tuning.resync_secs {
        0 => None,
        s => Some(Duration::from_secs(s as u64)),
    };
    let mut resync_timer = Instant::now();

    let mut running = true;
    // Attract mode: play for a limited time, but any key "takes over" — the game
    // keeps running as a normal session (no timer, hints return, saves enabled).
    // `attract_active` tracks the current mode; `intervened` records a takeover.
    let mut attract_active = attract;
    let mut attract_deadline =
        attract.then(|| Instant::now() + Duration::from_secs(attract_game_secs));
    let friendly = friendly_rom_name(rom_path);
    let mut intervened = false;
    // Track when each button was last seen, to release it on timeout.
    let mut button_last_seen: [Option<Instant>; BUTTON_COUNT] = [None; BUTTON_COUNT];
    // Buttons the terminal drives via real key edges. Once a button arrives as an
    // edge we ignore its translated "keypress" duplicate, so an iTerm sending both
    // a CSI-u event and a normal key doesn't double-fire (short jump + long jump).
    let mut edge_driven: [bool; BUTTON_COUNT] = [false; BUTTON_COUNT];
    // No key-up events arrive over a BBS connection, so buttons release by
    // timeout — long enough to span auto-repeat gaps (~150ms).
    let button_timeout = Duration::from_millis(150);

    // Drain input buffered before this game began — physical-key edges that piled
    // up while the menu was open (the menu never consumes edges) or a prior game's
    // trailing key-ups. Otherwise the first frame replays them, and in attract mode
    // any stray edge would trigger an immediate, wrong "takeover". Briefly loop so
    // bytes still trickling in after the mode-enable also get cleared.
    let drain_until = Instant::now() + Duration::from_millis(40);
    while Instant::now() < drain_until {
        let _ = input.poll(term)?;
        let _ = input.take_key_edges();
        std::thread::sleep(Duration::from_millis(5));
    }

    while running {
        // Release any button not seen within the timeout.
        {
            use gameboy_core::Button;
            const ALL_BUTTONS: [Button; BUTTON_COUNT] = [
                Button::A, Button::B, Button::Start, Button::Select,
                Button::Up, Button::Down, Button::Left, Button::Right,
            ];
            for button in ALL_BUTTONS {
                let idx = button_index(button);
                if let Some(last_seen) = button_last_seen[idx] {
                    if last_seen.elapsed() > button_timeout {
                        button_last_seen[idx] = None;
                        gameboy.release_button(button);
                    }
                }
            }
        }

        // Input. Always poll to feed the decoder. In CtermPhysical mode buttons
        // are driven by real key edges (exact press/release, simultaneous keys,
        // no timeout); otherwise the translated-key path with the timeout above.
        let keys = input.poll(term)?;
        let edges = if edge_input { input.take_key_edges() } else { Vec::new() };
        // Attract takeover: any real input (translated key or physical edge — probe
        // replies never surface here) hands the caller the controls. Stay in the
        // game (keep running), drop the timer, switch the status line back to the
        // hints, and consume this input so the wake key itself doesn't act or quit.
        if attract_active && (!keys.is_empty() || !edges.is_empty()) {
            attract_active = false;
            attract_deadline = None;
            intervened = true;
            redraw_status = true;
            continue;
        }
        // Attract time limit reached with no takeover: end the demo.
        if let Some(deadline) = attract_deadline {
            if Instant::now() >= deadline {
                running = false;
            }
        }
        // Enhanced key edges (CtermPhysical, and kitty CSI-u / kitty-form arrows):
        // exact press/release and simultaneous keys. Empty when the terminal
        // isn't sending them, so this is a no-op then.
        for edge in &edges {
            if let Some(button) = evdev_to_button(edge.code) {
                edge_driven[button_index(button)] = true;
                if edge.pressed {
                    gameboy.press_button(button);
                } else {
                    gameboy.release_button(button);
                }
            } else if edge.pressed && (edge.code == EVDEV_ESC || edge.code == EVDEV_Q) {
                running = false;
            }
        }
        // Translated keys — the universal path, identical to the menu's. When an
        // enhanced mode fully owns input (SyncTERM suppresses translated via =2h,
        // a live kitty terminal sends CSI-u), `keys` is empty so there's no double
        // input; otherwise (e.g. a proxy delivering only normal keys) this is what
        // keeps the game playable, with the button-release timeout above.
        for key in keys {
            match key {
                Key::Esc | Key::Char('q') | Key::Char('Q') => {
                    running = false;
                    break;
                }
                k => {
                    if let Some(button) = map_key_to_button(k) {
                        let idx = button_index(button);
                        // This button is driven by precise edges — drop the
                        // translated duplicate so it doesn't double-fire.
                        if edge_driven[idx] {
                            continue;
                        }
                        let is_new = button_last_seen[idx].is_none();
                        button_last_seen[idx] = Some(Instant::now());
                        if is_new {
                            gameboy.press_button(button);
                        }
                    }
                }
            }
        }

        if !running {
            break;
        }

        // Audio FIFO drained (underrun): the realtime estimate is stale, so
        // re-anchor the streamer's timeline to "queued == 0 now".
        if input.take_audio_drain().is_some() {
            if let Some(a) = apc.as_mut() {
                a.notify_drain(&mut *term, audio_clock.elapsed().as_millis() as u64)?;
            }
        }

        // Apply a terminal-size report if the probe was answered (row;col -> cols,rows).
        if let Some((row, col)) = input.take_cursor() {
            let size = (col, row);
            if size.0 > 0 && size.1 > 0 && size != last_size {
                last_size = size;
                renderer.update_dimensions(size.0, size.1);
                redraw_status = true; // the resize cleared the old status bar
            }
        }
        // Re-probe ~1/sec, but not while the link is saturated (its blocking
        // write would stall the loop just like a frame). Keep folding in the
        // capability probes until keyboard detection resolves.
        if !pace.skipping() && size_timer.elapsed() >= Duration::from_millis(1000) {
            size_timer = Instant::now();
            let _ = send_size_probe(&mut *term, !input.caps_resolved());
        }

        // Advance the simulation to wall-clock: run every GB frame whose time
        // has come. Repays sleep overshoot / brief loop stalls by emulating
        // extra frames here, so game speed and audio production stay at realtime.
        let mut frames_run = 0u32;
        while Instant::now() >= sim_deadline && frames_run < MAX_CATCHUP {
            loop {
                match gameboy.emulate(&mut framebuffer) {
                    StepResult::VBlank => break,
                    StepResult::AudioBufferFull => {
                        // Pull PCM if anything consumes it: the local device
                        // (rodio) and/or the APC streamer.
                        if audio_enabled || apc.is_some() {
                            let samples = gameboy.get_audio_buffer();
                            #[cfg(feature = "localaudio")]
                            if audio_enabled {
                                audio_buffer.push_samples(samples);
                            }
                            if let Some(a) = apc.as_mut() {
                                a.push_samples(samples);
                            }
                        }
                    }
                    StepResult::Nothing => {}
                }
            }
            sim_deadline += frame_duration;
            frames_run += 1;
        }
        if frames_run >= MAX_CATCHUP {
            // Can't keep up (or a long stall) — resync so we don't spiral. This
            // is the only place sim time is dropped (a bounded audio gap).
            sim_deadline = Instant::now();
        }

        // Ship APC audio, reconciled against wall-clock (speed up small drift,
        // drop big stalls) so sound stays in sync even while video frames skip.
        if let Some(a) = apc.as_mut() {
            let now_ms = audio_clock.elapsed().as_millis() as u64;
            // Periodic hard resync first: flush the terminal FIFO + re-anchor +
            // re-prime, dropping the accumulated latency, before this tick's clip
            // ships on top of the fresh cushion. (Order matters — flush, then emit.)
            if let Some(iv) = resync_interval {
                if resync_timer.elapsed() >= iv {
                    resync_timer = Instant::now();
                    a.resync(&mut *term, now_ms)?;
                }
            }
            a.emit_ready(&mut *term, now_ms)?;
            // Log stream health ~1/sec to the file (survives frame-skip).
            if apc_log_timer.elapsed() >= Duration::from_secs(1) {
                apc_log_timer = Instant::now();
                if let Some(f) = apc_log.as_mut() {
                    let s = a.stats(now_ms);
                    let _ = writeln!(
                        f,
                        "rate={}Hz lead={}ms drift={:+.2}% cor={:+.2}% drops={}",
                        s.rate, s.lead_ms, s.drift_pct, s.correction_pct, s.drops
                    );
                }
            }
        }

        // Start local audio playback after pre-buffering (localaudio builds only;
        // independent of transmit pacing — audio should start even while video
        // frames are being skipped).
        #[cfg(feature = "localaudio")]
        if pre_buffer_frames > 0 {
            pre_buffer_frames -= 1;
            if pre_buffer_frames == 0 {
                let source = GameboyAudioSource::new(Arc::clone(&audio_buffer));
                if let Some(s) = sink.as_ref() {
                    s.append(source);
                }
            }
        }

        // Transmit a frame at the capped rate, dropping it when the link is
        // behind. The emulator already advanced above, so the game keeps full
        // speed and input stays responsive even while frames are skipped.
        if frames_run > 0 && last_render.elapsed() >= render_interval {
            last_render = Instant::now();
            if !pace.skip_frame() {
                let write_start = Instant::now();

                // Periodic keyframe so corrupted cells self-heal (see above).
                if keyframe_timer.elapsed() >= keyframe_interval {
                    renderer.request_repaint();
                    keyframe_timer = Instant::now();
                }

                // ANSI music first (small), then the frame. Both go through the
                // same blocking writes, so a stalled link is measured below.
                ansi.update(gameboy.get_sound(), &mut *term);
                renderer.render(&framebuffer, &mut *term)?;

                // Bottom status bar: FPS (transmitted rate, refreshed once/sec)
                // docked left, keyboard hints centered. Redrawn on the FPS tick and
                // right after a resize (which cleared the old bar). APC audio
                // diagnostics go to the log only, not the screen.
                frame_count += 1;
                let fps_elapsed = fps_timer.elapsed();
                let fps_tick = fps_elapsed >= Duration::from_secs(1);
                if fps_tick {
                    last_fps = frame_count as f32 / fps_elapsed.as_secs_f32();
                    frame_count = 0;
                    fps_timer = Instant::now();
                }
                if fps_tick || redraw_status {
                    let status = attract_active.then_some(friendly.as_str());
                    draw_status_bar(
                        &mut *term, depth, renderer.fps_row(), last_size.0, last_fps, &fps_color, status,
                    )?;
                    redraw_status = false;
                }

                // Charge the write+flush time to the pacer: if it overran the
                // budget, upcoming frames are skipped until the link recovers.
                pace.note(write_start.elapsed());
            }
        }

        // Sleep until the next frame is due (no busy-spin). Oversleep here is
        // harmless — the catch-up loop above repays it on the next iteration, so
        // the long-run emulation rate stays locked to wall-clock.
        let now = Instant::now();
        if sim_deadline > now {
            std::thread::sleep((sim_deadline - now).min(frame_duration));
        }
    }

    // Flush the channel FIFO so no queued audio plays after we return to the
    // menu (the latency "tail").
    if let Some(a) = apc.as_mut() {
        let _ = a.stop(&mut *term);
    }

    // Restore translated input for the menu (no-op if it was never enabled).
    match kb_mode {
        KeyboardMode::CtermPhysical => {
            let _ = disable_physical_keys(&mut *term);
        }
        KeyboardMode::Kitty => {
            let _ = disable_kitty_keys(&mut *term);
            input.set_kitty_active(false);
        }
        KeyboardMode::Legacy => {}
    }

    // Save on exit, unless this was a pure attract demo that ran its timer out: a
    // game the caller never chose or took over shouldn't touch their saves. A
    // taken-over game (intervened) saves like any normal session.
    if !attract || intervened {
        match save_game(&gameboy, rom_path, user_id) {
            Ok(true) => println!("Game saved to: {}", get_save_path(rom_path, user_id).display()),
            Ok(false) => {} // Game doesn't support saves
            Err(e) => eprintln!("Failed to save game: {}", e),
        }
    }

    Ok(intervened)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_rom_name_strips_region_and_ext() {
        let f = |s: &str| friendly_rom_name(Path::new(s));
        assert_eq!(f("Super Mario Land 2 - 6 Golden Coins (USA, Europe).gb"), "Super Mario Land 2 - 6 Golden Coins");
        assert_eq!(f("Tetris (World) (Rev A).gb"), "Tetris");
        assert_eq!(f("/roms/1942 (USA, Europe).gbc"), "1942");
        assert_eq!(f("Kirby.gbc"), "Kirby"); // no region -> whole stem
    }

    #[test]
    fn truncate_chars_appends_dots_when_cut() {
        assert_eq!(truncate_chars("Hello", 10), "Hello");
        assert_eq!(truncate_chars("HelloWorld", 7), "Hello..");
    }

    #[test]
    fn linkpace_keeps_up_within_2x_budget() {
        let budget = Duration::from_millis(50);
        let mut p = LinkPace::new(budget);
        p.note(budget);          // on time
        p.note(budget * 2);      // 2x — still tolerated
        assert!(!p.skipping());
        assert!(!p.skip_frame());
    }

    #[test]
    fn linkpace_skips_in_proportion_to_overrun() {
        let budget = Duration::from_millis(50);
        let mut p = LinkPace::new(budget);
        p.note(budget * 8);      // 8x over budget -> skip ~8 frames
        let mut skipped = 0;
        while p.skip_frame() { skipped += 1; }
        assert_eq!(skipped, 8);
        assert!(!p.skipping());
    }

    #[test]
    fn linkpace_caps_skip_at_two_seconds() {
        let budget = Duration::from_millis(50);
        let mut p = LinkPace::new(budget);
        p.note(Duration::from_secs(60)); // huge stall
        let mut skipped = 0;
        while p.skip_frame() { skipped += 1; }
        assert_eq!(skipped, 40); // 2s / 50ms
    }

    #[test]
    fn parse_fps_clamps_and_reads() {
        assert_eq!(parse_fps(&["x".into(), "--fps".into(), "30".into()]), Some(30.0));
        assert_eq!(parse_fps(&["x".into(), "--fps=12".into()]), Some(12.0));
        assert_eq!(parse_fps(&["x".into(), "--fps".into(), "999".into()]), Some(60.0));
        assert_eq!(parse_fps(&["x".into(), "--fps".into(), "1".into()]), Some(5.0));
        assert_eq!(parse_fps(&["x".into()]), None);
    }
}
