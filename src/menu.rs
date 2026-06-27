use crossterm::{
    cursor::MoveTo,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{Clear, ClearType},
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::thread;

use crate::config;
use crate::cp437::Cp437Writer;
use crate::keys::{Input, Key};
use crate::renderer::RenderMode;
use crate::term::Term;

/// Configuration selected from the menu
pub struct MenuConfig {
    pub rom_path: PathBuf,
    pub render_mode: RenderMode,
    pub sound: SoundMode,
}

/// How many ROM rows are shown in the game list at once.
const VISIBLE_ROWS: usize = 8;

/// Width (in columns) of the ROM-name field in the game list.
const NAME_WIDTH: usize = 52;

/// Type-ahead buffer resets after this much idle time between keystrokes.
const TYPEAHEAD_RESET: Duration = Duration::from_millis(1000);

/// Filter applied to the game list, by ROM type.
#[derive(Clone, Copy, PartialEq)]
enum RomFilter {
    All,
    GameBoy,      // .gb
    GameBoyColor, // .gbc
}

impl RomFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::GameBoy,
            Self::GameBoy => Self::GameBoyColor,
            Self::GameBoyColor => Self::All,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::All => Self::GameBoyColor,
            Self::GameBoy => Self::All,
            Self::GameBoyColor => Self::GameBoy,
        }
    }

    /// Short label shown in the toggle.
    fn label(self) -> &'static str {
        match self {
            Self::All => "All ",
            Self::GameBoy => "GB  ",
            Self::GameBoyColor => "GBC ",
        }
    }

    /// Stable code persisted to the config file.
    fn code(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::GameBoy => "gb",
            Self::GameBoyColor => "gbc",
        }
    }

    fn from_code(s: &str) -> Self {
        match s {
            "gb" => Self::GameBoy,
            "gbc" => Self::GameBoyColor,
            _ => Self::All,
        }
    }

    /// Does a ROM with the given (lowercased) extension pass this filter?
    fn matches(self, ext: &str) -> bool {
        match self {
            Self::All => true,
            Self::GameBoy => ext == "gb",
            Self::GameBoyColor => ext == "gbc",
        }
    }
}

/// How game sound is delivered to the caller.
#[derive(Clone, Copy, PartialEq)]
pub enum SoundMode {
    Off,
    Ansi, // ANSI-music (MML beeps) — monophonic, universal
    Apc,  // SyncTERM APC PCM stream — full Game Boy audio
}

impl SoundMode {
    fn next(self) -> Self {
        match self {
            Self::Off => Self::Ansi,
            Self::Ansi => Self::Apc,
            Self::Apc => Self::Off,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Off => Self::Apc,
            Self::Ansi => Self::Off,
            Self::Apc => Self::Ansi,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "Off ",
            Self::Ansi => "ANSI",
            Self::Apc => "APC ",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ansi => "ansi",
            Self::Apc => "apc",
        }
    }

    fn from_code(s: &str) -> Self {
        match s {
            "off" => Self::Off,
            "apc" => Self::Apc,
            _ => Self::Ansi,
        }
    }
}

/// Menu state
struct MenuState {
    // Settings
    render_mode: RenderMode,
    sound: SoundMode,
    filter: RomFilter,
    roms_dir: PathBuf,

    // Per-user preference key (BBS user number); None for standalone use.
    user: Option<String>,

    // Available ROMs (master list) and the filtered view into it.
    rom_files: Vec<PathBuf>,
    filtered: Vec<usize>, // indices into rom_files passing `filter`
    selected_rom_index: usize, // index into `filtered`

    // UI state
    current_section: MenuSection,
    scroll_offset: usize,
    typeahead: String,
}

#[derive(Clone, Copy, PartialEq)]
enum MenuSection {
    RenderMode,
    Audio,
    Filter,
    RomsDirectory,
    GameList,
}

impl MenuSection {
    fn next(self) -> Self {
        match self {
            Self::RenderMode => Self::Audio,
            Self::Audio => Self::Filter,
            Self::Filter => Self::RomsDirectory,
            Self::RomsDirectory => Self::GameList,
            Self::GameList => Self::RenderMode,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::RenderMode => Self::GameList,
            Self::Audio => Self::RenderMode,
            Self::Filter => Self::Audio,
            Self::RomsDirectory => Self::Filter,
            Self::GameList => Self::RomsDirectory,
        }
    }
}

// Colors - Navy/Blue theme inspired by GBC
const HIGHLIGHT_BG: Color = Color::Rgb { r: 30, g: 60, b: 114 };   // Navy blue
const HIGHLIGHT_FG: Color = Color::Rgb { r: 130, g: 180, b: 255 }; // Light blue
const DIM_COLOR: Color = Color::Rgb { r: 80, g: 100, b: 140 };     // Muted blue-gray
const TEXT_COLOR: Color = Color::Rgb { r: 200, g: 210, b: 230 };   // Light blue-white
const ACCENT_COLOR: Color = Color::Rgb { r: 100, g: 200, b: 255 }; // Cyan accent
const BORDER_COLOR: Color = Color::Rgb { r: 60, g: 90, b: 150 };   // Border blue

// Logo lines (without border - we draw that separately)
const LOGO_LINES: [&str; 12] = [
    "  ████████╗███████╗██████╗ ███╗   ███╗██╗███╗   ██╗ █████╗ ██╗     ",
    "  ╚══██╔══╝██╔════╝██╔══██╗████╗ ████║██║████╗  ██║██╔══██╗██║     ",
    "     ██║   █████╗  ██████╔╝██╔████╔██║██║██╔██╗ ██║███████║██║     ",
    "     ██║   ██╔══╝  ██╔══██╗██║╚██╔╝██║██║██║╚██╗██║██╔══██║██║     ",
    "     ██║   ███████╗██║  ██║██║ ╚═╝ ██║██║██║ ╚████║██║  ██║███████╗",
    "     ╚═╝   ╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚═╝╚═╝  ╚═══╝╚═╝  ╚═╝╚══════╝",
    "   ██████╗  █████╗ ███╗   ███╗███████╗██████╗  ██████╗ ██╗   ██╗   ",
    "  ██╔════╝ ██╔══██╗████╗ ████║██╔════╝██╔══██╗██╔═══██╗╚██╗ ██╔╝   ",
    "  ██║  ███╗███████║██╔████╔██║█████╗  ██████╔╝██║   ██║ ╚████╔╝    ",
    "  ██║   ██║██╔══██║██║╚██╔╝██║██╔══╝  ██╔══██╗██║   ██║  ╚██╔╝     ",
    "  ╚██████╔╝██║  ██║██║ ╚═╝ ██║███████╗██████╔╝╚██████╔╝   ██║      ",
    "   ╚═════╝ ╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝╚═════╝  ╚═════╝    ╚═╝      ",
];

const LOGO_WIDTH: usize = 70;

// GBC startup colors (matching the actual GBC boot sequence)
const GBC_BLUE: Color = Color::Rgb { r: 0, g: 0, b: 255 };
const GBC_GREEN: Color = Color::Rgb { r: 0, g: 255, b: 0 };
const GBC_MAGENTA: Color = Color::Rgb { r: 255, g: 0, b: 255 };
const GBC_RED: Color = Color::Rgb { r: 255, g: 0, b: 0 };
const GBC_YELLOW: Color = Color::Rgb { r: 255, g: 255, b: 0 };

// The color cycle order for the sweep (yellow leads, blue trails)
const GBC_CYCLE: [Color; 5] = [GBC_YELLOW, GBC_RED, GBC_MAGENTA, GBC_GREEN, GBC_BLUE];

// How many characters each color spans in the rainbow trail
// GBC shows ~85% rainbow coverage when sweep reaches end: 70 * 0.85 / 5 colors ≈ 12
const COLOR_SPAN: usize = 12;

/// Play the GBC-style startup animation on the actual title position
fn play_startup_animation(stdout: &mut impl Write) -> io::Result<()> {
    emit!(stdout, Clear(ClearType::All))?;
    
    // Draw the border first (same position as menu)
    emit!(stdout, SetForegroundColor(BORDER_COLOR))?;
    emit!(stdout, MoveTo(2, 0), Print(format!("╔{}╗", "═".repeat(LOGO_WIDTH))))?;
    emit!(stdout, MoveTo(2, (LOGO_LINES.len() + 1) as u16), Print(format!("╚{}╝", "═".repeat(LOGO_WIDTH))))?;
    
    // Draw side borders
    for i in 0..LOGO_LINES.len() {
        emit!(stdout, MoveTo(2, (i + 1) as u16), SetForegroundColor(BORDER_COLOR), Print("║"))?;
        emit!(stdout, MoveTo((3 + LOGO_WIDTH) as u16, (i + 1) as u16), SetForegroundColor(BORDER_COLOR), Print("║"))?;
    }
    stdout.flush()?;
    
    thread::sleep(Duration::from_millis(200));
    
    // Animation: Color sweep from left to right across all logo lines
    // Need extra width for the full rainbow trail to sweep through and settle
    let total_width = LOGO_WIDTH + (GBC_CYCLE.len() * COLOR_SPAN);
    
    for sweep_col in 0..total_width {
        // Draw all logo lines with the color sweep
        for (line_idx, line) in LOGO_LINES.iter().enumerate() {
            emit!(stdout, MoveTo(3, (line_idx + 1) as u16))?;
            
            for (char_idx, ch) in line.chars().enumerate() {
                if char_idx > sweep_col {
                    // Not yet reached by sweep - invisible (print space to blend with background)
                    emit!(stdout, Print(' '))?;
                } else {
                    // Calculate which color in the cycle based on distance from sweep
                    // Each color spans COLOR_SPAN characters for a longer rainbow trail
                    let dist = sweep_col - char_idx;
                    let color_index = dist / COLOR_SPAN;
                    let color = if color_index < GBC_CYCLE.len() {
                        GBC_CYCLE[color_index]
                    } else {
                        // After sweep passes, settle to blue
                        GBC_BLUE
                    };
                    
                    emit!(stdout, SetForegroundColor(color), Print(ch))?;
                }
            }
        }
        
        stdout.flush()?;
        thread::sleep(Duration::from_millis(8));
    }
    
    // Final frame: everything in solid blue
    for (line_idx, line) in LOGO_LINES.iter().enumerate() {
        emit!(
            stdout,
            MoveTo(3, (line_idx + 1) as u16),
            SetForegroundColor(GBC_BLUE),
            Print(line)
        )?;
    }
    stdout.flush()?;
    
    // Brief hold before showing full menu
    thread::sleep(Duration::from_millis(600));
    
    Ok(())
}

impl MenuState {
    fn new(user: Option<&str>) -> Self {
        // Load persisted prefs for this user; fall back to cwd if no roms_dir
        // saved or the saved path no longer exists.
        let saved = config::load(user);
        let roms_dir = saved
            .roms_dir
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            });
        let rom_files = scan_for_roms(&roms_dir);

        let mut state = Self {
            render_mode: if saved.render_block.unwrap_or(false) {
                RenderMode::Block
            } else {
                RenderMode::Ascii
            },
            sound: saved.sound.as_deref().map(SoundMode::from_code).unwrap_or(SoundMode::Ansi),
            filter: saved.filter.as_deref().map(RomFilter::from_code).unwrap_or(RomFilter::All),
            roms_dir,
            user: user.map(|u| u.to_string()),
            rom_files,
            filtered: Vec::new(),
            selected_rom_index: 0,
            current_section: MenuSection::RenderMode,
            scroll_offset: 0,
            typeahead: String::new(),
        };
        state.rebuild_filter();
        state
    }

    fn refresh_roms(&mut self) {
        self.rom_files = scan_for_roms(&self.roms_dir);
        self.typeahead.clear();
        self.rebuild_filter();
    }

    /// Recompute the filtered view and reset the cursor to the top.
    fn rebuild_filter(&mut self) {
        self.filtered = self
            .rom_files
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                let ext = p
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                self.filter.matches(&ext)
            })
            .map(|(i, _)| i)
            .collect();
        self.selected_rom_index = 0;
        self.scroll_offset = 0;
    }

    /// The ROM currently under the cursor, if any.
    fn selected_rom(&self) -> Option<&PathBuf> {
        self.filtered
            .get(self.selected_rom_index)
            .map(|&i| &self.rom_files[i])
    }

    /// Keep the selected row inside the visible window.
    fn ensure_visible(&mut self) {
        if self.selected_rom_index < self.scroll_offset {
            self.scroll_offset = self.selected_rom_index;
        } else if self.selected_rom_index >= self.scroll_offset + VISIBLE_ROWS {
            self.scroll_offset = self.selected_rom_index + 1 - VISIBLE_ROWS;
        }
    }

    /// Jump to the first ROM in the filtered list whose name starts with the
    /// current type-ahead buffer (case-insensitive). No-op if nothing matches.
    fn typeahead_jump(&mut self) {
        if self.typeahead.is_empty() {
            return;
        }
        let needle = self.typeahead.to_lowercase();
        if let Some(pos) = self.filtered.iter().position(|&i| {
            self.rom_files[i]
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase().starts_with(&needle))
                .unwrap_or(false)
        }) {
            self.selected_rom_index = pos;
            self.ensure_visible();
        }
    }

    /// Persist render/audio/filter prefs for this user, preserving whatever
    /// roms_dir was explicitly saved (that is only set via "Set & Save").
    fn persist_prefs(&self) {
        let saved_dir = config::load(self.user.as_deref()).roms_dir;
        let _ = config::save(
            self.user.as_deref(),
            &config::Config {
                roms_dir: saved_dir,
                render_block: Some(self.render_mode == RenderMode::Block),
                sound: Some(self.sound.code().to_string()),
                filter: Some(self.filter.code().to_string()),
            },
        );
    }
}

/// Scan a directory for .gb and .gbc files
fn scan_for_roms(dir: &Path) -> Vec<PathBuf> {
    let mut roms = Vec::new();
    
    // Also check for a "roms" subdirectory
    // Note: only use lowercase "roms" to avoid duplicates on case-insensitive filesystems (macOS)
    let dirs_to_scan = vec![
        dir.to_path_buf(),
        dir.join("roms"),
        dir.join("games"),
    ];
    
    for scan_dir in dirs_to_scan {
        if let Ok(entries) = std::fs::read_dir(&scan_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    let ext = ext.to_string_lossy().to_lowercase();
                    if ext == "gb" || ext == "gbc" {
                        // Canonicalize to avoid duplicates from symlinks or case differences
                        let canonical = path.canonicalize().unwrap_or(path);
                        if !roms.iter().any(|p| p == &canonical) {
                            roms.push(canonical);
                        }
                    }
                }
            }
        }
    }
    
    // Sort by filename
    roms.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .cmp(&b.file_name().unwrap_or_default().to_string_lossy().to_lowercase())
    });
    
    roms
}

/// Display the menu and return the selected configuration.
///
/// `user` is an optional per-user key (the BBS user number) used to load and
/// persist that caller's render/audio/filter preferences. `animate` controls
/// whether the GBC startup sweep plays (skipped when returning from a game).
pub fn show_menu(
    term: &mut dyn Term,
    input: &mut Input,
    user: Option<&str>,
    animate: bool,
) -> io::Result<Option<MenuConfig>> {
    let mut state = MenuState::new(user);

    // Play the GBC-style startup animation (first entry only). The alternate
    // screen / raw mode are owned by the caller and shared with the game.
    if animate {
        let mut w = Cp437Writer::new(&mut *term);
        play_startup_animation(&mut w)?;
    }

    run_menu_loop(term, input, &mut state)
}

fn run_menu_loop(
    term: &mut dyn Term,
    input: &mut Input,
    state: &mut MenuState,
) -> io::Result<Option<MenuConfig>> {
    // Tracks the last keystroke time so the type-ahead buffer can reset after
    // a pause. Starts far enough in the past that the first keypress is fresh.
    let mut last_key = Instant::now() - TYPEAHEAD_RESET * 2;

    loop {
        // Draw through a CP437 adapter over the shared terminal, then release the
        // borrow so we can read input from the same terminal.
        {
            let mut stdout = Cp437Writer::new(&mut *term);
            draw_menu(&mut stdout, state)?;
        }

        // Wait for a key (None = the caller hung up).
        let key = match input.wait(term)? {
            Some(k) => k,
            None => return Ok(None),
        };

        // ── Game-list type-ahead ────────────────────────────────────────────
        // While the game list is focused, printable keys drive an incremental
        // search instead of menu shortcuts (so a title like "Zelda" doesn't
        // trip the "Z = confirm" shortcut). Enter still confirms, Esc backs
        // out, and the arrows/Tab still navigate.
        if state.current_section == MenuSection::GameList {
            match key {
                Key::Char(c) if !c.is_control() => {
                    if last_key.elapsed() > TYPEAHEAD_RESET {
                        state.typeahead.clear();
                    }
                    last_key = Instant::now();
                    state.typeahead.push(c);
                    state.typeahead_jump();
                    continue;
                }
                Key::Backspace => {
                    state.typeahead.pop();
                    last_key = Instant::now();
                    state.typeahead_jump();
                    continue;
                }
                _ => {}
            }
        }

        match key {
            Key::Char('q') | Key::Char('Q') | Key::Esc => {
                return Ok(None);
            }
            Key::Char('x') | Key::Char('X') => {
                return Ok(None);
            }
            Key::Up => {
                if state.current_section == MenuSection::GameList && !state.filtered.is_empty() {
                    if state.selected_rom_index > 0 {
                        state.selected_rom_index -= 1;
                        state.ensure_visible();
                    } else {
                        // At the top of the game list — step back into settings
                        state.typeahead.clear();
                        state.current_section = state.current_section.prev();
                    }
                } else {
                    state.current_section = state.current_section.prev();
                }
            }
            Key::Down => {
                if state.current_section == MenuSection::GameList && !state.filtered.is_empty() {
                    if state.selected_rom_index + 1 < state.filtered.len() {
                        state.selected_rom_index += 1;
                        state.ensure_visible();
                    } else {
                        // At the bottom of the game list — wrap back to settings
                        state.typeahead.clear();
                        state.current_section = state.current_section.next();
                    }
                } else {
                    state.current_section = state.current_section.next();
                }
            }
            Key::Tab => {
                state.typeahead.clear();
                state.current_section = state.current_section.next();
            }
            Key::BackTab => {
                state.typeahead.clear();
                state.current_section = state.current_section.prev();
            }
            // Left/Right and Space (Select): Toggle options
            Key::Left | Key::Right | Key::Char(' ') => {
                match state.current_section {
                    MenuSection::RenderMode => {
                        state.render_mode = match state.render_mode {
                            RenderMode::Ascii => RenderMode::Block,
                            RenderMode::Block => RenderMode::Ascii,
                        };
                        state.persist_prefs();
                    }
                    MenuSection::Audio => {
                        state.sound = if key == Key::Left {
                            state.sound.prev()
                        } else {
                            state.sound.next()
                        };
                        state.persist_prefs();
                    }
                    MenuSection::Filter => {
                        state.filter = if key == Key::Left {
                            state.filter.prev()
                        } else {
                            state.filter.next()
                        };
                        state.rebuild_filter();
                        state.persist_prefs();
                    }
                    _ => {}
                }
            }
            // Z (A button) or Enter (Start): Select/Confirm
            Key::Char('z') | Key::Char('Z') | Key::Enter => {
                match state.current_section {
                    MenuSection::GameList => {
                        if let Some(rom_path) = state.selected_rom().cloned() {
                            return Ok(Some(MenuConfig {
                                rom_path,
                                render_mode: state.render_mode,
                                sound: state.sound,
                            }));
                        }
                    }
                    MenuSection::RomsDirectory => {
                        if let Some(chosen) = pick_directory(term, input, &state.roms_dir)? {
                            state.roms_dir = chosen.clone();
                            state.refresh_roms();
                            // Persist the chosen directory alongside current prefs
                            let _ = config::save(
                                state.user.as_deref(),
                                &config::Config {
                                    roms_dir: Some(chosen),
                                    render_block: Some(state.render_mode == RenderMode::Block),
                                    sound: Some(state.sound.code().to_string()),
                                    filter: Some(state.filter.code().to_string()),
                                },
                            );
                        }
                    }
                    MenuSection::RenderMode => {
                        state.render_mode = match state.render_mode {
                            RenderMode::Ascii => RenderMode::Block,
                            RenderMode::Block => RenderMode::Ascii,
                        };
                        state.persist_prefs();
                    }
                    MenuSection::Audio => {
                        state.sound = state.sound.next();
                        state.persist_prefs();
                    }
                    MenuSection::Filter => {
                        state.filter = state.filter.next();
                        state.rebuild_filter();
                        state.persist_prefs();
                    }
                }
            }
            Key::Char('r') | Key::Char('R') => {
                state.refresh_roms();
            }
            _ => {}
        }
    }
}

fn draw_menu(stdout: &mut impl Write, state: &MenuState) -> io::Result<()> {
    emit!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    
    // Draw top border
    emit!(stdout, SetForegroundColor(BORDER_COLOR))?;
    emit!(stdout, MoveTo(2, 0), Print(format!("╔{}╗", "═".repeat(LOGO_WIDTH))))?;
    
    // Draw logo in solid blue (matching the GBC startup end state)
    for (i, line) in LOGO_LINES.iter().enumerate() {
        emit!(stdout, MoveTo(2, (i + 1) as u16), SetForegroundColor(BORDER_COLOR), Print("║"))?;
        emit!(stdout, MoveTo(3, (i + 1) as u16), SetForegroundColor(GBC_BLUE), Print(line))?;
        emit!(stdout, MoveTo((3 + LOGO_WIDTH) as u16, (i + 1) as u16), SetForegroundColor(BORDER_COLOR), Print("║"))?;
    }
    
    // Draw bottom border
    emit!(stdout, SetForegroundColor(BORDER_COLOR))?;
    emit!(stdout, MoveTo(2, (LOGO_LINES.len() + 1) as u16), Print(format!("╚{}╝", "═".repeat(LOGO_WIDTH))))?;
    
    let menu_start_y = LOGO_LINES.len() as u16 + 3;
    
    // Draw settings section
    emit!(stdout, MoveTo(4, menu_start_y))?;
    emit!(stdout, SetForegroundColor(ACCENT_COLOR), Print("═══ Settings ═══"), ResetColor)?;
    
    // Render Mode
    draw_option(
        stdout, 
        4, 
        menu_start_y + 2, 
        "Render Mode", 
        &format!("◄ {} ►", match state.render_mode {
            RenderMode::Ascii => "ASCII ",
            RenderMode::Block => "Block ",
        }),
        state.current_section == MenuSection::RenderMode
    )?;
    
    // Sound (Off / ANSI music / APC PCM)
    draw_option(
        stdout,
        4,
        menu_start_y + 3,
        "Sound      ",
        &format!("◄ {} ►", state.sound.label()),
        state.current_section == MenuSection::Audio
    )?;
    
    // Filter (game type) — All / GB / GBC
    draw_option(
        stdout,
        4,
        menu_start_y + 4,
        "Filter     ",
        &format!("◄ {} ►", state.filter.label()),
        state.current_section == MenuSection::Filter
    )?;

    // ROMs Directory — show full path, indicate if it's the persisted default
    let saved_roms_dir = config::load(state.user.as_deref()).roms_dir;
    let is_saved = saved_roms_dir.as_deref() == Some(state.roms_dir.as_path());
    let dir_display = state.roms_dir.to_string_lossy().to_string();
    let dir_label = if is_saved {
        format!("{} [saved]", truncate_str(&dir_display, 32))
    } else {
        truncate_str(&dir_display, 40)
    };
    draw_option(
        stdout,
        4,
        menu_start_y + 5,
        "Directory  ",
        &dir_label,
        state.current_section == MenuSection::RomsDirectory
    )?;

    // Games section header: active filter + how many ROMs match
    emit!(stdout, MoveTo(4, menu_start_y + 7))?;
    emit!(
        stdout,
        SetForegroundColor(ACCENT_COLOR),
        Print(format!("═══ Games: {} ({}) ═══", state.filter.label().trim(), state.filtered.len())),
        ResetColor
    )?;
    // Type-ahead indicator (shown while searching in the game list)
    if state.current_section == MenuSection::GameList && !state.typeahead.is_empty() {
        emit!(
            stdout,
            MoveTo(40, menu_start_y + 7),
            SetForegroundColor(HIGHLIGHT_FG),
            Print(format!("Find: {}", truncate_str(&state.typeahead, 24))),
            ResetColor
        )?;
    }

    let games_start_y = menu_start_y + 9;

    if state.filtered.is_empty() {
        let msg = if state.rom_files.is_empty() {
            "No ROM files found (.gb, .gbc)"
        } else {
            "No games match this filter — change Filter above"
        };
        emit!(
            stdout,
            MoveTo(4, games_start_y),
            SetForegroundColor(DIM_COLOR),
            Print(msg),
            ResetColor
        )?;
        emit!(
            stdout,
            MoveTo(4, games_start_y + 1),
            SetForegroundColor(DIM_COLOR),
            Print("Press [R] to refresh"),
            ResetColor
        )?;
    } else {
        let visible_count = VISIBLE_ROWS.min(state.filtered.len());
        let is_game_list_selected = state.current_section == MenuSection::GameList;

        for (i, &rom_idx) in state.filtered.iter()
            .skip(state.scroll_offset)
            .take(visible_count)
            .enumerate()
        {
            let rom = &state.rom_files[rom_idx];
            let actual_index = state.scroll_offset + i;
            let is_selected = actual_index == state.selected_rom_index;

            // Title without the extension; the type tag conveys GB vs GBC.
            let title = rom.file_stem()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            let is_gbc = rom.extension()
                .map(|e| e.to_string_lossy().eq_ignore_ascii_case("gbc"))
                .unwrap_or(false);
            let tag = if is_gbc { "GBC" } else { "GB" };
            let tag_color = if is_gbc { ACCENT_COLOR } else { TEXT_COLOR };

            draw_rom_row(
                stdout,
                games_start_y + i as u16,
                &title,
                tag,
                tag_color,
                is_selected,
                is_selected && is_game_list_selected,
            )?;
        }

        // Scroll indicators
        let arrow_x: u16 = 65;
        if state.scroll_offset > 0 {
            emit!(
                stdout,
                MoveTo(arrow_x, games_start_y),
                SetForegroundColor(ACCENT_COLOR),
                Print("▲"),
                ResetColor
            )?;
        }
        if state.scroll_offset + visible_count < state.filtered.len() {
            emit!(
                stdout,
                MoveTo(arrow_x, games_start_y + visible_count as u16 - 1),
                SetForegroundColor(ACCENT_COLOR),
                Print("▼"),
                ResetColor
            )?;
        }
    }

    // Help text
    let help_y = games_start_y + VISIBLE_ROWS as u16 + 2;
    emit!(stdout, MoveTo(4, help_y), SetForegroundColor(DIM_COLOR))?;
    emit!(stdout, Print("↑↓ Move   Type to jump   Z/Enter Play   ◄► Change setting   Esc Quit"))?;
    emit!(stdout, MoveTo(4, help_y + 1), SetForegroundColor(DIM_COLOR))?;
    emit!(stdout, Print("Filter: All / GB / GBC    Directory: Enter to browse, S to save    R Refresh"))?;
    emit!(stdout, ResetColor)?;

    stdout.flush()?;
    Ok(())
}

/// Draw a single ROM row: " ▶ <title>   <TAG> ", with the type tag in its own
/// color so Game Boy (GB) vs Game Boy Color (GBC) is visible at a glance.
fn draw_rom_row(
    stdout: &mut impl Write,
    y: u16,
    title: &str,
    tag: &str,
    tag_color: Color,
    marker: bool,
    highlight: bool,
) -> io::Result<()> {
    let prefix = if marker { " ▶ " } else { "   " };
    let name = format!("{}{:<width$} ", prefix, truncate_str(title, NAME_WIDTH), width = NAME_WIDTH);
    emit!(stdout, MoveTo(4, y))?;
    if highlight {
        emit!(
            stdout,
            SetBackgroundColor(HIGHLIGHT_BG),
            SetForegroundColor(HIGHLIGHT_FG),
            Print(name),
            SetForegroundColor(tag_color),
            Print(format!("{:<3} ", tag)),
            ResetColor
        )?;
    } else {
        let name_color = if marker { TEXT_COLOR } else { DIM_COLOR };
        emit!(
            stdout,
            SetForegroundColor(name_color),
            Print(name),
            SetForegroundColor(tag_color),
            Print(format!("{:<3} ", tag)),
            ResetColor
        )?;
    }
    Ok(())
}

fn draw_option(stdout: &mut impl Write, x: u16, y: u16, label: &str, value: &str, selected: bool) -> io::Result<()> {
    emit!(stdout, MoveTo(x, y))?;
    
    if selected {
        emit!(
            stdout,
            SetBackgroundColor(HIGHLIGHT_BG),
            SetForegroundColor(HIGHLIGHT_FG),
            Print(format!(" {} ", label)),
            SetForegroundColor(ACCENT_COLOR),
            Print(format!("{} ", value)),
            ResetColor
        )?;
    } else {
        emit!(
            stdout,
            SetForegroundColor(TEXT_COLOR),
            Print(format!(" {} ", label)),
            SetForegroundColor(DIM_COLOR),
            Print(format!("{} ", value)),
            ResetColor
        )?;
    }
    
    Ok(())
}

fn truncate_str(s: &str, max_len: usize) -> String {
    // Count/slice by chars (not bytes) so a multi-byte name can't panic on a
    // mid-codepoint slice, and the width matches the visible column count.
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}

/// Count .gb/.gbc files directly inside a directory (non-recursive).
fn count_roms_in(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    e.path()
                        .extension()
                        .map(|x| {
                            let x = x.to_string_lossy().to_lowercase();
                            x == "gb" || x == "gbc"
                        })
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Read the immediate subdirectories of `dir`, sorted case-insensitively.
fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    // Skip hidden directories (start with '.')
                    let name = p.file_name()?.to_string_lossy().to_string();
                    if name.starts_with('.') {
                        return None;
                    }
                    if p.is_dir() { Some(p) } else { None }
                })
                .collect()
        })
        .unwrap_or_default();

    dirs.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .cmp(&b.file_name().unwrap_or_default().to_string_lossy().to_lowercase())
    });
    dirs
}

/// Interactive directory browser. Returns the chosen PathBuf if the user
/// pressed S (Set & Save), or None if they cancelled.
///
/// Runs inside the already-active alternate screen / raw mode.
fn pick_directory(
    term: &mut dyn Term,
    input: &mut Input,
    start_dir: &Path,
) -> io::Result<Option<PathBuf>> {
    let mut current = start_dir.to_path_buf();
    let mut subdirs = list_subdirs(&current);
    // Index 0 = ".." (go up), indices 1.. = subdirs
    let mut selected: usize = 0;
    let mut scroll: usize = 0;
    let max_visible: usize = 16;

    // Input mode state
    let mut typing_path = false;
    let mut typed_input = String::new();
    let mut status_msg: Option<(String, bool)> = None; // (msg, is_error)

    loop {
        // ── Draw the browser ──────────────────────────────────────────────
        {
        let mut stdout = Cp437Writer::new(&mut *term);
        emit!(stdout, Clear(ClearType::All))?;

        // Title bar
        emit!(
            stdout,
            MoveTo(2, 0),
            SetForegroundColor(ACCENT_COLOR),
            Print("╔══ Browse ROM Directory ══╗"),
            ResetColor
        )?;

        // Current path
        emit!(
            stdout,
            MoveTo(2, 1),
            SetForegroundColor(TEXT_COLOR),
            Print("Path: "),
            SetForegroundColor(HIGHLIGHT_FG),
            Print(truncate_str(&current.to_string_lossy(), 60)),
            ResetColor
        )?;

        // ROM count in current dir
        let rom_count = count_roms_in(&current);
        let rom_hint = if rom_count == 0 {
            format!("  ({} ROMs here)", rom_count)
        } else {
            format!("  ({} ROM{} here ✓)", rom_count, if rom_count == 1 { "" } else { "s" })
        };
        emit!(
            stdout,
            MoveTo(2, 2),
            SetForegroundColor(if rom_count > 0 { ACCENT_COLOR } else { DIM_COLOR }),
            Print(&rom_hint),
            ResetColor
        )?;

        emit!(
            stdout,
            MoveTo(2, 3),
            SetForegroundColor(BORDER_COLOR),
            Print("─".repeat(60)),
            ResetColor
        )?;

        // Build entry list: ".." first, then subdirs
        let total_entries = 1 + subdirs.len(); // 0 = "..", 1.. = subdirs
        let list_start_y: u16 = 4;

        // Clamp scroll so selected is always visible
        if selected < scroll {
            scroll = selected;
        } else if selected >= scroll + max_visible {
            scroll = selected.saturating_sub(max_visible - 1);
        }

        for i in 0..max_visible {
            let entry_idx = scroll + i;
            if entry_idx >= total_entries {
                break;
            }
            let y = list_start_y + i as u16;
            let is_selected = entry_idx == selected;

            if entry_idx == 0 {
                // ".." entry
                emit!(stdout, MoveTo(2, y))?;
                if is_selected {
                    emit!(
                        stdout,
                        SetBackgroundColor(HIGHLIGHT_BG),
                        SetForegroundColor(HIGHLIGHT_FG),
                        Print(format!(" ▶ ../  (go up) {:>35} ", "")),
                        ResetColor
                    )?;
                } else {
                    emit!(
                        stdout,
                        SetForegroundColor(DIM_COLOR),
                        Print("   ../  (go up)"),
                        ResetColor
                    )?;
                }
            } else {
                let sub = &subdirs[entry_idx - 1];
                let name = sub
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "?".to_string());
                let roms_here = count_roms_in(sub);
                let suffix = if roms_here > 0 {
                    format!("  [{} ROM{}]", roms_here, if roms_here == 1 { "" } else { "s" })
                } else {
                    String::new()
                };

                emit!(stdout, MoveTo(2, y))?;
                if is_selected {
                    emit!(
                        stdout,
                        SetBackgroundColor(HIGHLIGHT_BG),
                        SetForegroundColor(HIGHLIGHT_FG),
                        Print(format!(" ▶ {:<44}{}", truncate_str(&name, 44), truncate_str(&suffix, 14))),
                        ResetColor
                    )?;
                } else {
                    emit!(
                        stdout,
                        SetForegroundColor(TEXT_COLOR),
                        Print(format!("   {:<44}", truncate_str(&name, 44))),
                        SetForegroundColor(ACCENT_COLOR),
                        Print(truncate_str(&suffix, 14)),
                        ResetColor
                    )?;
                }
            }
        }

        // Scroll indicators
        let vis_end_y = list_start_y + max_visible.min(total_entries) as u16;
        if scroll > 0 {
            emit!(stdout, MoveTo(64, list_start_y), SetForegroundColor(ACCENT_COLOR), Print("▲"), ResetColor)?;
        }
        if scroll + max_visible < total_entries {
            emit!(stdout, MoveTo(64, vis_end_y - 1), SetForegroundColor(ACCENT_COLOR), Print("▼"), ResetColor)?;
        }

        // Separator
        let sep_y = list_start_y + max_visible as u16 + 1;
        emit!(
            stdout,
            MoveTo(2, sep_y),
            SetForegroundColor(BORDER_COLOR),
            Print("─".repeat(60)),
            ResetColor
        )?;

        // Status / path-input line
        let status_y = sep_y + 1;
        if typing_path {
            emit!(
                stdout,
                MoveTo(2, status_y),
                SetForegroundColor(ACCENT_COLOR),
                Print("Path: "),
                SetForegroundColor(TEXT_COLOR),
                Print(&typed_input),
                Print("█"),  // cursor
                ResetColor
            )?;
        } else if let Some((ref msg, is_err)) = status_msg {
            emit!(
                stdout,
                MoveTo(2, status_y),
                SetForegroundColor(if is_err {
                    Color::Rgb { r: 255, g: 80, b: 80 }
                } else {
                    ACCENT_COLOR
                }),
                Print(msg),
                ResetColor
            )?;
        }

        // Help footer
        let help_y = status_y + 2;
        emit!(
            stdout,
            MoveTo(2, help_y),
            SetForegroundColor(DIM_COLOR),
            Print("↑↓ Move  Enter Navigate  S Set&Save  P Type path  Esc/X Cancel"),
            ResetColor
        )?;

        stdout.flush()?;
        } // end draw scope — release the terminal borrow so we can read input

        // ── Handle input ──────────────────────────────────────────────────
        {
            let key = match input.wait(term)? {
                Some(k) => k,
                None => return Ok(None),
            };
            status_msg = None; // clear status on next keypress

            if typing_path {
                match key {
                    Key::Esc => {
                        typing_path = false;
                        typed_input.clear();
                    }
                    Key::Enter => {
                        // Confirm the typed path
                        let expanded = if typed_input.is_empty() {
                            None
                        } else {
                            config::expand_tilde(typed_input.trim())
                        };
                        if let Some(p) = expanded {
                            if p.is_dir() {
                                current = p;
                                subdirs = list_subdirs(&current);
                                selected = 0;
                                scroll = 0;
                                typing_path = false;
                                typed_input.clear();
                                status_msg = Some(("Directory set.".to_string(), false));
                            } else {
                                status_msg = Some(("Path not found or not a directory.".to_string(), true));
                            }
                        } else {
                            status_msg = Some(("Invalid path.".to_string(), true));
                        }
                        if !typing_path { typed_input.clear(); }
                    }
                    Key::Backspace => {
                        typed_input.pop();
                    }
                    Key::Char(c) => {
                        typed_input.push(c);
                    }
                    _ => {}
                }
            } else {
                match key {
                    Key::Esc | Key::Char('x') | Key::Char('X') => {
                        return Ok(None);
                    }
                    Key::Up => {
                        if selected > 0 { selected -= 1; }
                    }
                    Key::Down => {
                        if selected + 1 < 1 + subdirs.len() { selected += 1; }
                    }
                    Key::Enter | Key::Char('z') | Key::Char('Z') => {
                        if selected == 0 {
                            // Go up
                            if let Some(parent) = current.parent().map(|p| p.to_path_buf()) {
                                current = parent;
                                subdirs = list_subdirs(&current);
                                selected = 0;
                                scroll = 0;
                            }
                        } else {
                            // Enter subdirectory
                            let target = subdirs[selected - 1].clone();
                            current = target;
                            subdirs = list_subdirs(&current);
                            selected = 0;
                            scroll = 0;
                        }
                    }
                    Key::Char('s') | Key::Char('S') => {
                        // Set & Save current directory
                        return Ok(Some(current));
                    }
                    Key::Char('p') | Key::Char('P') => {
                        // Switch to manual path entry mode
                        typing_path = true;
                        typed_input.clear();
                    }
                    _ => {}
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_by_extension() {
        assert!(RomFilter::All.matches("gb") && RomFilter::All.matches("gbc"));
        assert!(RomFilter::GameBoy.matches("gb") && !RomFilter::GameBoy.matches("gbc"));
        assert!(RomFilter::GameBoyColor.matches("gbc") && !RomFilter::GameBoyColor.matches("gb"));
    }

    #[test]
    fn filter_code_roundtrips() {
        for f in [RomFilter::All, RomFilter::GameBoy, RomFilter::GameBoyColor] {
            assert!(RomFilter::from_code(f.code()) == f);
        }
    }
}
