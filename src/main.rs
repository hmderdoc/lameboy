#[macro_use]
mod out;
mod ansi_music;
mod apc_audio;
#[cfg(feature = "localaudio")]
mod audio;
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
use framebuffer::{FrameBuffer, GB_HEIGHT, GB_WIDTH};
use input::map_key_to_button;
use keys::{Input, Key};
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

// Colors
const FPS_COLOR: &str = "\x1b[38;2;80;200;80m";
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
fn send_size_probe<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    emit!(term, MoveTo(9998, 9998))?;
    term.write_all(b"\x1b[6n")?;
    term.flush()
}

/// Extract the `--dropfile <path>` / `--dropfile=<path>` value (a DOOR32.SYS).
fn parse_dropfile(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--dropfile=") {
            return Some(v.to_string()).filter(|s| !s.is_empty());
        }
        if a == "--dropfile" {
            return it.next().cloned().filter(|s| !s.is_empty());
        }
    }
    None
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
    let force_mute = args.iter().any(|a| a == "--mute");
    // ANSI-music engine: only available when explicitly enabled (the BBS door
    // passes --ansi-music). It does NOT open an audio device, so it is independent
    // of --mute. The menu's Audio toggle gates whether it actually plays.
    let ansi_music_flag = args.iter().any(|a| a == "--ansi-music");
    let cli_mode = if args.iter().any(|a| a == "--block") {
        Some(RenderMode::Block)
    } else if args.iter().any(|a| a == "--ascii") {
        Some(RenderMode::Ascii)
    } else {
        None
    };
    let render_fps = parse_fps(&args).unwrap_or(DEFAULT_RENDER_FPS);

    // DOOR32.SYS dropfile: on a BBS that doesn't pass the connection via stdio
    // (e.g. EleBBS/Mystic/Synchronet Win32), it names the inherited socket and
    // the user. The per-user key is the explicit --user, else the dropfile's
    // user number, else none.
    let door = parse_dropfile(&args)
        .as_deref()
        .map(Path::new)
        .and_then(door32::read);
    let user_id = parse_user(&args).or_else(|| door.as_ref().and_then(|d| d.user_key()));

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
            if a == "--user" || a == "--fps" || a == "--dropfile" {
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
    term.flush()?;

    let result = run_session(
        &mut *term,
        &mut input,
        positional_rom,
        cli_mode,
        force_mute,
        ansi_music_flag,
        user_id.as_deref(),
        render_fps,
    );

    // Restore the screen no matter how the session ended; Term drop restores raw.
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
    render_fps: f64,
) -> io::Result<()> {
    if let Some(rom) = positional_rom {
        let mode = cli_mode.unwrap_or(RenderMode::Ascii);
        return run_game(
            term, input, &Path::new(&rom).to_path_buf(), mode,
            !force_mute, ansi_music_flag, false, user_id, render_fps,
        );
    }

    let mut animate = true;
    loop {
        match show_menu(term, input, user_id, animate)? {
            Some(config) => {
                let mode = cli_mode.unwrap_or(config.render_mode);
                let want_apc = config.sound == SoundMode::Apc;
                // Local PCM device (rodio) only for a non-APC, non-Off mode when
                // not muted. A headless door never opens a device; APC instead
                // streams PCM to the caller.
                let audio_enabled =
                    !force_mute && config.sound != SoundMode::Off && !want_apc;
                let music_enabled = ansi_music_flag && config.sound == SoundMode::Ansi;
                run_game(
                    term, input, &config.rom_path, mode, audio_enabled,
                    music_enabled, want_apc, user_id, render_fps,
                )?;
                animate = false;
            }
            None => return Ok(()),
        }
    }
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
) -> io::Result<()> {
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
    let config = RenderConfig { mode };
    let mut renderer = Renderer::new(config);
    renderer.update_dimensions(80, 24);
    let _ = send_size_probe(&mut *term);

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

    // Live-resize polling: a door connection delivers no SIGWINCH/resize events,
    // so we periodically probe the terminal for its size (see send_size_probe).
    let mut size_timer = Instant::now();
    let mut last_size = (80u16, 24u16);

    // ANSI-music engine: approximates the lead pulse channel via terminal beeps.
    let mut ansi = ansi_music::AnsiMusic::new(music_enabled);

    // APC PCM streaming: capture the emulator's audio and ship it to a
    // SyncTERM-APC-capable terminal as chunked clips (~120 ms).
    let mut apc = if apc_enabled {
        // 120 ms chunks, 240 ms pre-roll. The wall-clock loop keeps the stream
        // fed at realtime, so the cushion only needs to cover brief jitter — a
        // smaller lead means less audio-vs-video latency. Raise if gaps return.
        Some(apc_audio::ApcAudio::new(120, 240))
    } else {
        None
    };

    let mut running = true;
    // Track when each button was last seen, to release it on timeout.
    let mut button_last_seen: [Option<Instant>; BUTTON_COUNT] = [None; BUTTON_COUNT];
    // No key-up events arrive over a BBS connection, so buttons release by
    // timeout — long enough to span auto-repeat gaps (~150ms).
    let button_timeout = Duration::from_millis(150);

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

        // Input: decode whatever arrived this frame from the connection.
        for key in input.poll(term)? {
            match key {
                Key::Esc | Key::Char('q') | Key::Char('Q') => {
                    running = false;
                    break;
                }
                k => {
                    if let Some(button) = map_key_to_button(k) {
                        let idx = button_index(button);
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

        // Apply a terminal-size report if the probe was answered (row;col -> cols,rows).
        if let Some((row, col)) = input.take_cursor() {
            let size = (col, row);
            if size.0 > 0 && size.1 > 0 && size != last_size {
                last_size = size;
                renderer.update_dimensions(size.0, size.1);
            }
        }
        // Re-probe ~1/sec, but not while the link is saturated (its blocking
        // write would stall the loop just like a frame).
        if !pace.skipping() && size_timer.elapsed() >= Duration::from_millis(1000) {
            size_timer = Instant::now();
            let _ = send_size_probe(&mut *term);
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

        // Ship any full APC audio chunks now, regardless of video pacing, so
        // sound keeps flowing even while frames are being skipped.
        if let Some(a) = apc.as_mut() {
            a.emit_ready(&mut *term)?;
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

                // FPS overlay (transmitted frame rate), at most once/sec.
                frame_count += 1;
                let fps_elapsed = fps_timer.elapsed();
                if fps_elapsed >= Duration::from_secs(1) {
                    let fps = frame_count as f32 / fps_elapsed.as_secs_f32();
                    frame_count = 0;
                    fps_timer = Instant::now();
                    emit!(term, MoveTo(0, renderer.fps_row()))?;
                    write!(term, "{}FPS: {:5.1}  {}", FPS_COLOR, fps, RESET)?;
                    term.flush()?;
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

    // Emit any trailing partial audio chunk before returning to the menu.
    if let Some(a) = apc.as_mut() {
        let _ = a.flush(&mut *term);
    }

    // Save game on exit
    match save_game(&gameboy, rom_path, user_id) {
        Ok(true) => println!("Game saved to: {}", get_save_path(rom_path, user_id).display()),
        Ok(false) => {} // Game doesn't support saves
        Err(e) => eprintln!("Failed to save game: {}", e),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
