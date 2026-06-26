mod ansi_music;
mod apc_audio;
mod audio;
mod config;
mod cp437;
mod framebuffer;
mod input;
mod menu;
mod renderer;
mod save;

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use gameboy_core::{Gameboy, RTC, StepResult};
use rodio::{OutputStream, Sink};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use audio::{AudioBuffer, GameboyAudioSource};
use framebuffer::{FrameBuffer, GB_HEIGHT, GB_WIDTH};
use input::map_key_to_button;
use menu::{show_menu, SoundMode};
use renderer::{RenderConfig, RenderMode, Renderer};
use save::{get_save_path, load_save, save_game};

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

/// Query the terminal's *current* size by parking the cursor at the far corner
/// and asking where it actually landed (cursor-position report via ESC[6n).
///
/// Unlike `terminal::size()` (which reads the pty's ioctl winsize), this
/// round-trips to the client, so it detects live resizes even inside a
/// Synchronet door -- sbbs sets the door pty's winsize once at launch and never
/// updates it, so the ioctl value and SIGWINCH-based Resize events are frozen.
/// Returns (cols, rows), or None if the terminal didn't answer.
fn query_terminal_size(stdout: &mut io::Stdout) -> Option<(u16, u16)> {
    // The full frame repaint repositions the cursor afterwards, so no save/restore.
    if execute!(stdout, MoveTo(9998, 9998)).is_err() {
        return None;
    }
    crossterm::cursor::position()
        .ok()
        .map(|(col, row)| (col + 1, row + 1))
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
    let user_id = parse_user(&args);
    // Transmit frame-rate cap (Hz). Bounds bandwidth over a remote link; the
    // emulator still runs at full speed regardless.
    let render_fps = parse_fps(&args).unwrap_or(DEFAULT_RENDER_FPS);
    // First non-flag argument (if any) is the ROM path. (Skip the value that
    // follows a value-taking flag (`--user`, `--fps`) so it isn't mistaken for a
    // ROM path.)
    let positional_rom = {
        let mut found = None;
        let mut skip_next = false;
        for a in args.iter().skip(1) {
            if skip_next {
                skip_next = false;
                continue;
            }
            if a == "--user" || a == "--fps" {
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

    if let Some(rom) = positional_rom {
        // Explicit ROM on the command line — play it once and exit.
        let mode = cli_mode.unwrap_or(RenderMode::Ascii);
        return run_game(
            &Path::new(&rom).to_path_buf(),
            mode,
            !force_mute,
            ansi_music_flag,
            false, // APC streaming is only meaningful from the menu/door path
            user_id.as_deref(),
            render_fps,
        );
    }

    // No ROM given: show the interactive menu in a loop, so quitting a game
    // (Esc/Q) returns to the menu rather than exiting the door. --mute always
    // wins over the menu's audio toggle so a muted door can never try to open
    // an audio device; --block/--ascii (if passed) override the menu's mode.
    let mut animate = true;
    loop {
        match show_menu(user_id.as_deref(), animate)? {
            Some(config) => {
                let mode = cli_mode.unwrap_or(config.render_mode);
                let want_apc = config.sound == SoundMode::Apc;
                // Local PCM device (rodio) only for a non-APC, non-Off mode when
                // not muted. The door runs --mute, so it never opens a device;
                // APC instead streams PCM to the terminal.
                let audio_enabled =
                    !force_mute && config.sound != SoundMode::Off && !want_apc;
                // ANSI music plays when the door enabled it (--ansi-music) and the
                // player selected ANSI mode.
                let music_enabled = ansi_music_flag && config.sound == SoundMode::Ansi;
                run_game(
                    &config.rom_path, mode, audio_enabled, music_enabled,
                    want_apc, user_id.as_deref(), render_fps,
                )?;
                // Only play the startup sweep on first entry, not after each game.
                animate = false;
            }
            None => return Ok(()), // User quit the menu
        }
    }
}

/// Run a single ROM to completion. Returns when the player quits the game
/// (Esc/Q), at which point the caller decides whether to re-show the menu.
fn run_game(
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

    // Setup audio output (lock-free buffer)
    // Larger buffer (16K samples = ~180ms stereo) for more headroom
    let audio_buffer = Arc::new(AudioBuffer::new(16384));
    // Only open an audio device when audio is actually enabled. On headless
    // servers (e.g. a BBS door host) there is no PCM device, and unconditionally
    // calling OutputStream::try_default() aborts the process. With --mute we skip
    // device init entirely. (Local patch -- see PATCH-NOTES.md)
    let (_stream, sink) = if audio_enabled {
        let (stream, stream_handle) =
            OutputStream::try_default().expect("Failed to open audio output");
        let sink = Sink::try_new(&stream_handle).expect("Failed to create audio sink");
        (Some(stream), Some(sink))
    } else {
        (None, None)
    };

    // Pre-buffer some frames before starting audio to avoid initial underruns
    let mut pre_buffer_frames = if audio_enabled { 3 } else { 0 };

    let mut stdout = io::stdout();

    // Setup terminal
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, Hide, Clear(ClearType::All))?;

    let mut framebuffer = FrameBuffer::new(GB_WIDTH, GB_HEIGHT);

    let config = RenderConfig { mode };
    let mut renderer = Renderer::new(config);

    // Fit the game to the current terminal size
    if let Ok((cols, rows)) = terminal::size() {
        renderer.update_dimensions(cols, rows);
    }

    // Use precise floating-point duration for accurate 60 FPS (16.667ms, not 16ms).
    // This paces the *emulator* — the game always runs at full Game Boy speed.
    let frame_duration = Duration::from_secs_f64(1.0 / 60.0);

    // Transmit pacing: repaint the terminal at most `render_fps` times/sec
    // (bounding bandwidth), and skip frames when the link can't keep up.
    let render_interval = Duration::from_secs_f64(1.0 / render_fps.clamp(1.0, 60.0));
    let mut last_render = Instant::now() - render_interval; // render immediately on entry
    let mut pace = LinkPace::new(render_interval);

    // Periodic keyframe: force a full (clear-less) repaint every few seconds so a
    // cell corrupted by line noise on a lossy link self-heals — delta encoding
    // otherwise only repaints cells the game itself changes.
    let keyframe_interval = Duration::from_secs(7);
    let mut keyframe_timer = Instant::now();

    // FPS tracking (counts frames actually transmitted)
    let mut frame_count = 0u32;
    let mut fps_timer = Instant::now();

    // Live-resize polling: ask the terminal for its size periodically, since a
    // Synchronet door never gets SIGWINCH/Resize events (see query_terminal_size).
    let mut size_timer = Instant::now();
    let mut last_size = terminal::size().unwrap_or((0, 0));
    let mut resize_probe_enabled = true;

    // ANSI-music engine: approximates the lead pulse channel via terminal beeps.
    let mut ansi = ansi_music::AnsiMusic::new(music_enabled);

    // APC PCM streaming: capture the emulator's audio and ship it to a
    // SyncTERM-APC-capable terminal as chunked clips (~120 ms).
    let mut apc = if apc_enabled {
        // 120 ms chunks, 400 ms pre-roll cushion (FIFO lead for the client mixer).
        Some(apc_audio::ApcAudio::new(120, 400))
    } else {
        None
    };

    let mut running = true;
    // Track when each button was last seen (press or repeat event)
    // Using fixed array instead of HashMap - faster for 8 buttons
    let mut button_last_seen: [Option<Instant>; BUTTON_COUNT] = [None; BUTTON_COUNT];
    // Detect if terminal supports release events (Ghostty, kitty, etc.)
    let mut terminal_supports_release = false;
    // Fallback timeout for terminals without release support (Terminal.app, etc.)
    // Needs to be long enough to span keyboard repeat gaps (~150ms is safe)
    let button_timeout = Duration::from_millis(150);

    while running {
        let frame_start = Instant::now();

        // Only use timeout-based release for terminals that don't support release events
        if !terminal_supports_release {
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

        // Process input events
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Resize(cols, rows) => {
                    // Terminal was resized (e.g., zoom in/out) — refit and clear
                    renderer.update_dimensions(cols, rows);
                }
                Event::Key(key_event) => {
                    // Handle quit keys
                    if key_event.code == KeyCode::Char('q')
                        || key_event.code == KeyCode::Char('Q')
                        || key_event.code == KeyCode::Esc
                    {
                        running = false;
                        break;
                    }

                    // Map to gameboy button and handle press/repeat
                    if let Some(button) = map_key_to_button(key_event.code) {
                    let idx = button_index(button);
                    match key_event.kind {
                        KeyEventKind::Press | KeyEventKind::Repeat => {
                            // Update last seen time (used for fallback timeout)
                            let is_new = button_last_seen[idx].is_none();
                            button_last_seen[idx] = Some(Instant::now());

                            // Only call press_button if this is a new press
                            if is_new {
                                gameboy.press_button(button);
                            }
                        }
                        KeyEventKind::Release => {
                            // Terminal supports release events! Use them directly.
                            terminal_supports_release = true;
                            if button_last_seen[idx].take().is_some() {
                                gameboy.release_button(button);
                            }
                        }
                    }
                }
                }
                _ => {} // Ignore other events (mouse, focus, etc.)
            }
        }

        if !running {
            break;
        }

        // Poll for a live terminal resize (~1/sec). The door's pty winsize is
        // frozen at launch, so we ask the terminal directly rather than wait for
        // a Resize event that will never come. Skip the probe while the link is
        // saturated — its blocking write would stall the loop just like a frame.
        if resize_probe_enabled
            && !pace.skipping()
            && size_timer.elapsed() >= Duration::from_millis(1000)
        {
            size_timer = Instant::now();
            match query_terminal_size(&mut stdout) {
                Some(size) if size.0 > 0 && size.1 > 0 => {
                    if size != last_size {
                        last_size = size;
                        renderer.update_dimensions(size.0, size.1);
                    }
                }
                // Terminal didn't answer the position query: stop probing so a
                // non-responsive client can't cost us a round-trip every second.
                _ => resize_probe_enabled = false,
            }
        }

        // Run emulation until VBlank
        loop {
            let result = gameboy.emulate(&mut framebuffer);
            match result {
                StepResult::VBlank => break,
                StepResult::AudioBufferFull => {
                    // Pull PCM if anything consumes it: the local device (rodio)
                    // and/or the APC streamer.
                    if audio_enabled || apc.is_some() {
                        let samples = gameboy.get_audio_buffer();
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

        // Ship any full APC audio chunks now, regardless of video pacing, so
        // sound keeps flowing even while frames are being skipped.
        if let Some(a) = apc.as_mut() {
            a.emit_ready(&mut stdout)?;
        }

        // Start audio playback after pre-buffering (independent of transmit
        // pacing — audio should start even while video frames are being skipped).
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
        if last_render.elapsed() >= render_interval {
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
                ansi.update(gameboy.get_sound(), &mut stdout);
                renderer.render(&framebuffer, &mut stdout)?;

                // FPS overlay (transmitted frame rate), at most once/sec.
                frame_count += 1;
                let fps_elapsed = fps_timer.elapsed();
                if fps_elapsed >= Duration::from_secs(1) {
                    let fps = frame_count as f32 / fps_elapsed.as_secs_f32();
                    frame_count = 0;
                    fps_timer = Instant::now();
                    execute!(stdout, MoveTo(0, renderer.fps_row()))?;
                    write!(stdout, "{}FPS: {:5.1}  {}", FPS_COLOR, fps, RESET)?;
                    stdout.flush()?;
                }

                // Charge the write+flush time to the pacer: if it overran the
                // budget, upcoming frames are skipped until the link recovers.
                pace.note(write_start.elapsed());
            }
        }

        // Frame timing — pace the emulator to ~60 Hz (game speed), regardless of
        // how many frames were actually transmitted this second.
        let elapsed = frame_start.elapsed();
        if elapsed < frame_duration {
            std::thread::sleep(frame_duration - elapsed);
        }
    }

    // Emit any trailing partial audio chunk before tearing down.
    if let Some(a) = apc.as_mut() {
        let _ = a.flush(&mut stdout);
    }

    // Cleanup terminal
    execute!(stdout, Show, LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

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
