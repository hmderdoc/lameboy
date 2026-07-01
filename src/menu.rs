use crossterm::{
    cursor::MoveTo,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{Clear, ClearType},
    Command,
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::thread;

use crate::config;
use crate::cp437::Cp437Writer;
use crate::keys::{Input, Key, MenuEvent};
use crate::renderer::RenderMode;
use crate::term::Term;

/// Configuration selected from the menu
pub struct MenuConfig {
    pub rom_path: PathBuf,
    pub render_mode: RenderMode,
    pub sound: SoundMode,
}

/// Fallback ROM-row count used until the terminal answers a size probe. The live
/// count is derived from the real terminal height (see `visible_for`).
const VISIBLE_ROWS: usize = 12;

/// Row of the "Games:" header and the first ROM row. Everything above (the hint
/// lines, the wordmark + settings band) is fixed height; the list below grows to
/// fill whatever screen height the caller has.
const GAMES_HEADER_Y: u16 = 9;
const GAMES_START_Y: u16 = 10;

/// ROM rows that fit under the header on a `term_rows`-tall screen (1 row of
/// bottom margin), floored so the list is always usable.
fn visible_for(term_rows: u16) -> usize {
    (term_rows as usize)
        .saturating_sub(GAMES_START_Y as usize + 1)
        .max(4)
}

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

/// Terminal screen-size handling.
#[derive(Clone, Copy, PartialEq)]
pub enum ScreenSize {
    /// Use the caller's terminal as-is (the list fills whatever height it has).
    Auto,
    /// Ask the terminal to resize to the ideal size for the game render, and
    /// restore the original size on exit. No-ops on terminals that ignore the
    /// resize sequence (e.g. SyncTERM), where Auto behavior remains.
    Best,
}

/// Ideal render size in character cells (≈1 char-row per 2 Game Boy pixels).
pub const BEST_COLS: u16 = 162;
pub const BEST_ROWS: u16 = 74;

impl ScreenSize {
    fn next(self) -> Self {
        match self {
            Self::Auto => Self::Best,
            Self::Best => Self::Auto,
        }
    }

    fn prev(self) -> Self {
        self.next()
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Best => "Best",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Best => "best",
        }
    }

    fn from_code(s: &str) -> Self {
        match s {
            "best" => Self::Best,
            _ => Self::Auto,
        }
    }
}

/// Menu state
struct MenuState {
    // Settings
    render_mode: RenderMode,
    sound: SoundMode,
    filter: RomFilter,
    screen: ScreenSize,

    // Per-user preference key (BBS user number); None for standalone use.
    user: Option<String>,

    // Available ROMs (master list) and the filtered view into it.
    rom_files: Vec<PathBuf>,
    filtered: Vec<usize>, // indices into rom_files passing `filter`
    selected_rom_index: usize, // index into `filtered`

    // UI state
    current_section: MenuSection,
    // Which settings row to return to when Tab jumps back from the game list,
    // so focus is preserved instead of always snapping to the top setting.
    last_settings: MenuSection,
    scroll_offset: usize,
    // How many ROM rows currently fit, derived from the live terminal height.
    visible_rows: usize,
    typeahead: String,
}

// The ROM directory is a sysop setting (config / --roms), NOT a caller-facing
// option — a filesystem browser in the door would let any caller wander the
// server's disk. So there's no RomsDirectory section.
#[derive(Clone, Copy, PartialEq)]
enum MenuSection {
    RenderMode,
    Audio,
    Filter,
    Screen,
    GameList,
}

impl MenuSection {
    fn next(self) -> Self {
        match self {
            Self::RenderMode => Self::Audio,
            Self::Audio => Self::Filter,
            Self::Filter => Self::Screen,
            Self::Screen => Self::GameList,
            Self::GameList => Self::RenderMode,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::RenderMode => Self::GameList,
            Self::Audio => Self::RenderMode,
            Self::Filter => Self::Audio,
            Self::Screen => Self::Filter,
            Self::GameList => Self::Screen,
        }
    }
}

// Colors — the original Game Boy (DMG) LCD palette: dark green ink on the pale
// pea-green "screen". Emphasis comes from value (how dark), not hue, the way the
// real hardware did it. Hex refs: CADC9F / 8bac0f / 306230 / 0f380f.
const SCREEN_BG: Color = Color::Rgb { r: 202, g: 220, b: 159 }; // #CADC9F — LCD background
const INK: Color = Color::Rgb { r: 15, g: 56, b: 15 };         // #0f380f — darkest
const INK_MID: Color = Color::Rgb { r: 48, g: 98, b: 48 };     // #306230 — mid green
const MOSS: Color = Color::Rgb { r: 139, g: 172, b: 15 };      // #8bac0f — light olive
const LIME: Color = Color::Rgb { r: 155, g: 188, b: 15 };      // #9bbc0f — classic GB green

const HIGHLIGHT_BG: Color = INK;   // selected row: dark "pixel" bar...
const HIGHLIGHT_FG: Color = LIME;  // ...with bright GB-green text
const DIM_COLOR: Color = MOSS;     // secondary / receding text
const TEXT_COLOR: Color = INK_MID; // primary body text
const ACCENT_COLOR: Color = INK_MID; // headers
const KEY_COLOR: Color = INK;      // emphasis (keys, wordmark, selection marker) — darkest pops

/// Reset to the menu's "normal" cell — dark ink on the LCD background. Used in
/// place of `ResetColor`, which would clear the background to the terminal
/// default and punch dark holes in the green screen.
struct Normal;
impl Command for Normal {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        SetForegroundColor(TEXT_COLOR).write_ansi(f)?;
        SetBackgroundColor(SCREEN_BG).write_ansi(f)
    }
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        // ANSI-only door; the Windows console-API path is never taken.
        Ok(())
    }
}
const NORMAL: Normal = Normal;

/// The wordmark is a 1-bit pixel bitmap (9 pixel rows tall, `#` = on). It's
/// rendered with CP437 half-blocks — each character cell stacks two pixel rows
/// (`█` both, `▀` upper, `▄` lower) — which doubles the vertical resolution and
/// smooths the diagonals/curves that plain full blocks leave jagged.
const LOGO_TEXT: &str = "LAME BOY";
const LOGO_PX_H: usize = 9;

fn logo_glyph(c: char) -> [&'static str; LOGO_PX_H] {
    match c {
        'L' => ["##   ", "##   ", "##   ", "##   ", "##   ", "##   ", "##   ", "#####", "#####"],
        'A' => [" ### ", "## ##", "## ##", "## ##", "#####", "#####", "## ##", "## ##", "## ##"],
        'M' => ["##   ##", "### ###", "## # ##", "##   ##", "##   ##", "##   ##", "##   ##", "##   ##", "##   ##"],
        'E' => ["#####", "##   ", "##   ", "#### ", "#### ", "##   ", "##   ", "#####", "#####"],
        'B' => ["#### ", "## ##", "## ##", "#### ", "#### ", "## ##", "## ##", "## ##", "#### "],
        'O' => [" ### ", "## ##", "## ##", "## ##", "## ##", "## ##", "## ##", "## ##", " ### "],
        'Y' => ["## ##", "## ##", " ### ", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ", "  #  "],
        _ => ["  ", "  ", "  ", "  ", "  ", "  ", "  ", "  ", "  "], // word gap
    }
}

/// Per-pixel-row italic shear, in columns, measured from the bottom. Two pixel
/// rows that share a character cell differ by ~1 column, so a vertical stroke
/// renders as `▄█▀` (a smooth half-block gradient) rather than a hard 1-column
/// jump — that's what de-jags the slant.
fn logo_shear(pixel_row: usize) -> usize {
    (LOGO_PX_H - 1 - pixel_row) / 2
}

/// Draw the "LAME BOY" wordmark at (x, y): half-block rendered with the italic
/// slant baked into the bitmap (so the diagonals smooth out), in the darkest ink.
fn draw_logo<W: Write + ?Sized>(stdout: &mut W, x: u16, y: u16) -> io::Result<()> {
    // Assemble the upright pixel bitmap: one bool row per pixel line, 1-col gap.
    let mut px: Vec<Vec<bool>> = vec![Vec::new(); LOGO_PX_H];
    for ch in LOGO_TEXT.chars() {
        let g = logo_glyph(ch);
        for (r, row) in px.iter_mut().enumerate() {
            row.extend(g[r].chars().map(|c| c == '#'));
            row.push(false); // inter-letter gap
        }
    }
    // A pixel at source column j on row r lands at output column j + logo_shear(r).
    let src_w = px.iter().map(|r| r.len()).max().unwrap_or(0);
    let width = src_w + logo_shear(0);
    let char_rows = LOGO_PX_H.div_ceil(2);

    // Sample the (possibly out-of-range) pixel for output column `c` on row `r`.
    let sample = |r: usize, c: usize| -> bool {
        c.checked_sub(logo_shear(r))
            .and_then(|j| px[r].get(j))
            .copied()
            .unwrap_or(false)
    };

    emit!(stdout, SetForegroundColor(KEY_COLOR), SetBackgroundColor(SCREEN_BG))?;
    for cr in 0..char_rows {
        let top_r = cr * 2;
        let bot_r = cr * 2 + 1;
        let mut s = String::new();
        for c in 0..width {
            let t = sample(top_r, c);
            let b = bot_r < LOGO_PX_H && sample(bot_r, c);
            s.push(match (t, b) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        emit!(stdout, MoveTo(x, y + cr as u16), Print(s.trim_end()))?;
    }
    emit!(stdout, NORMAL)?;
    Ok(())
}

/// Draw a selection caret: blinking when its menu zone is focused, dimmed (not
/// hidden) when it isn't, so both menus always show where the cursor sits.
fn draw_caret<W: Write + ?Sized>(stdout: &mut W, active: bool, active_fg: Color) -> io::Result<()> {
    if active {
        emit!(stdout, SetAttribute(Attribute::SlowBlink), SetForegroundColor(active_fg), Print("▶"), SetAttribute(Attribute::NoBlink))
    } else {
        emit!(stdout, SetForegroundColor(DIM_COLOR), Print("▶"))
    }
}

/// Draw a "══ Label ══" zone header. The flanking `══` blink when this zone holds
/// the cursor (Settings vs Games), a second at-a-glance cue for which menu you're
/// on; the label itself stays steady. Exactly one zone is active at a time.
fn draw_zone_header<W: Write + ?Sized>(
    stdout: &mut W,
    x: u16,
    y: u16,
    label: &str,
    active: bool,
) -> io::Result<()> {
    emit!(stdout, MoveTo(x, y), SetForegroundColor(ACCENT_COLOR))?;
    if active {
        emit!(
            stdout,
            SetAttribute(Attribute::SlowBlink), Print("══"), SetAttribute(Attribute::NoBlink),
            Print(format!(" {} ", label)),
            SetAttribute(Attribute::SlowBlink), Print("══"), SetAttribute(Attribute::NoBlink),
            NORMAL
        )
    } else {
        emit!(stdout, Print(format!("══ {} ══", label)), NORMAL)
    }
}

/// Brief splash before the menu (first entry only). Kept tiny so it works on a
/// short screen and doesn't feel like a separate app.
fn play_startup_animation<W: Write + ?Sized>(stdout: &mut W) -> io::Result<()> {
    emit!(stdout, SetBackgroundColor(SCREEN_BG), Clear(ClearType::All))?;
    draw_logo(stdout, 16, 8)?; // centered-ish splash
    stdout.flush()?;
    thread::sleep(Duration::from_millis(450));
    Ok(())
}

impl MenuState {
    fn new(user: Option<&str>, roms_override: Option<&str>) -> Self {
        let saved = config::load(user);
        // ROM directory precedence (all sysop-controlled): explicit --roms,
        // then a persisted roms_dir, then the working directory (scan_for_roms
        // also checks ./roms and ./games).
        let roms_dir = roms_override
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| saved.roms_dir.filter(|p| p.is_dir()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let rom_files = scan_for_roms(&roms_dir);

        let mut state = Self {
            // New users default to Block (the richer renderer); a saved choice
            // (Some(true/false)) always wins.
            render_mode: if saved.render_block.unwrap_or(true) {
                RenderMode::Block
            } else {
                RenderMode::Ascii
            },
            sound: saved.sound.as_deref().map(SoundMode::from_code).unwrap_or(SoundMode::Ansi),
            filter: saved.filter.as_deref().map(RomFilter::from_code).unwrap_or(RomFilter::All),
            screen: saved.screen.as_deref().map(ScreenSize::from_code).unwrap_or(ScreenSize::Auto),
            user: user.map(|u| u.to_string()),
            rom_files,
            filtered: Vec::new(),
            selected_rom_index: 0,
            // Open focused on the game list (this state is rebuilt on every menu
            // entry, so it also lands here after a game exits), not the settings.
            current_section: MenuSection::GameList,
            last_settings: MenuSection::RenderMode,
            scroll_offset: 0,
            visible_rows: VISIBLE_ROWS,
            typeahead: String::new(),
        };
        state.rebuild_filter();
        // Reopen on the game the user last launched, if it's still in the list.
        if let Some(name) = saved.last_game.as_deref() {
            state.select_by_filename(name);
        }
        state
    }

    // Setting toggles, shared by the ◄►/Enter handlers and the R/S/F hotkeys.
    fn toggle_render(&mut self) {
        self.render_mode = match self.render_mode {
            RenderMode::Ascii => RenderMode::Block,
            RenderMode::Block => RenderMode::Ascii,
        };
        self.persist_prefs();
    }

    fn cycle_sound(&mut self) {
        self.sound = self.sound.next();
        self.persist_prefs();
    }

    fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.rebuild_filter();
        self.persist_prefs();
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
        let win = self.visible_rows.max(1);
        if self.selected_rom_index < self.scroll_offset {
            self.scroll_offset = self.selected_rom_index;
        } else if self.selected_rom_index >= self.scroll_offset + win {
            self.scroll_offset = self.selected_rom_index + 1 - win;
        }
    }

    /// Scroll the game list one visible page, keeping the cursor at the SAME row
    /// within the viewport (so paging feels like turning a page, not jumping the
    /// selection to the window edge). When already on the first/last page, the
    /// cursor moves to the very first/last game instead.
    fn page(&mut self, up: bool) {
        if self.filtered.is_empty() {
            return;
        }
        self.typeahead.clear();
        let n = self.filtered.len();
        let win = self.visible_rows.max(1);
        let max_off = n.saturating_sub(win); // top offset that still fills the window
        let rel = self.selected_rom_index.saturating_sub(self.scroll_offset);
        if up {
            let new_off = self.scroll_offset.saturating_sub(win);
            if new_off == self.scroll_offset {
                self.selected_rom_index = 0; // already at the top page
            } else {
                self.scroll_offset = new_off;
                self.selected_rom_index = self.scroll_offset + rel;
            }
        } else {
            let new_off = (self.scroll_offset + win).min(max_off);
            if new_off == self.scroll_offset {
                self.selected_rom_index = n - 1; // already at the bottom page
            } else {
                self.scroll_offset = new_off;
                self.selected_rom_index = (self.scroll_offset + rel).min(n - 1);
            }
        }
        self.ensure_visible();
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

    /// Move the cursor to the ROM whose file name matches `name` exactly (the
    /// persisted last game). No-op if it's gone or filtered out of the list.
    fn select_by_filename(&mut self, name: &str) {
        if let Some(pos) = self.filtered.iter().position(|&i| {
            self.rom_files[i]
                .file_name()
                .map(|n| n.to_string_lossy() == name)
                .unwrap_or(false)
        }) {
            self.selected_rom_index = pos;
            self.ensure_visible();
        }
    }

    /// Persist render/audio/filter prefs for this user, preserving whatever
    /// roms_dir and last_game were saved (those are set elsewhere).
    fn persist_prefs(&self) {
        let saved = config::load(self.user.as_deref());
        let _ = config::save(
            self.user.as_deref(),
            &config::Config {
                roms_dir: saved.roms_dir,
                render_block: Some(self.render_mode == RenderMode::Block),
                sound: Some(self.sound.code().to_string()),
                filter: Some(self.filter.code().to_string()),
                screen: Some(self.screen.code().to_string()),
                last_game: saved.last_game,
            },
        );
    }

    /// Remember the game just launched, preserving the other saved prefs, so the
    /// list reopens on it next time the menu is shown.
    fn persist_last_game(&self, name: &str) {
        let mut cfg = config::load(self.user.as_deref());
        cfg.last_game = Some(name.to_string());
        let _ = config::save(self.user.as_deref(), &cfg);
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
    roms_override: Option<&str>,
    animate: bool,
    screen_orig: &mut Option<(u16, u16)>,
) -> io::Result<Option<MenuConfig>> {
    let mut state = MenuState::new(user, roms_override);

    // Play the GBC-style startup animation (first entry only). The alternate
    // screen / raw mode are owned by the caller and shared with the game.
    if animate {
        let mut w = Cp437Writer::new(&mut *term);
        play_startup_animation(&mut w)?;
    }

    let result = run_menu_loop(term, input, &mut state, screen_orig);
    // Drop the LCD background before leaving so it doesn't bleed into the game
    // render or the terminal after the door exits.
    let _ = emit!(term, ResetColor, Clear(ClearType::All));
    let _ = term.flush();
    result
}

/// Sync the terminal to the chosen `ScreenSize` after it changes. "Best" captures
/// the current size (into `screen_orig`) and resizes to the ideal; "Auto"
/// restores the captured size. Idempotent, and persists the preference.
fn apply_screen(
    state: &MenuState,
    term: &mut dyn Term,
    screen_orig: &mut Option<(u16, u16)>,
    cur_rows: u16,
    cur_cols: u16,
) {
    match state.screen {
        ScreenSize::Best => {
            if screen_orig.is_none() {
                *screen_orig = Some((cur_rows, cur_cols));
                let _ = crate::resize_terminal(term, BEST_ROWS, BEST_COLS);
            }
        }
        ScreenSize::Auto => {
            if let Some((rows, cols)) = screen_orig.take() {
                let _ = crate::resize_terminal(term, rows, cols);
            }
        }
    }
    state.persist_prefs();
}

fn run_menu_loop(
    term: &mut dyn Term,
    input: &mut Input,
    state: &mut MenuState,
    screen_orig: &mut Option<(u16, u16)>,
) -> io::Result<Option<MenuConfig>> {
    // Tracks the last keystroke time so the type-ahead buffer can reset after
    // a pause. Starts far enough in the past that the first keypress is fresh.
    let mut last_key = Instant::now() - TYPEAHEAD_RESET * 2;

    // Track the caller's terminal size so the game list fills the screen. It
    // starts at the 80x24 fallback and self-corrects from the first probe reply.
    let mut term_rows: u16 = 24;
    let mut term_cols: u16 = 80;

    'redraw: loop {
        state.visible_rows = visible_for(term_rows);

        // Draw through a CP437 adapter over the shared terminal, then release the
        // borrow so we can read input from the same terminal.
        {
            let mut stdout = Cp437Writer::new(&mut *term);
            draw_menu(&mut stdout, state)?;
        }
        // Ask the terminal its size; the reply comes back as a Resize event below
        // and re-lays-out the list — no keystroke needed. Capability probes ride
        // along (folded in) until keyboard detection resolves.
        let _ = crate::send_size_probe(&mut *term, !input.caps_resolved());
        let _ = term.flush();

        // Wait for a key, applying any size reports in between (redraw only when
        // the height actually changes, so a stable size doesn't busy-loop).
        let key = loop {
            match input.wait_event(term)? {
                MenuEvent::Key(k) => break k,
                MenuEvent::Resize(rows, cols) => {
                    // Persisted "Best" not yet applied: the reported size is the
                    // pre-resize original — capture it, then resize and re-probe.
                    if state.screen == ScreenSize::Best && screen_orig.is_none() {
                        *screen_orig = Some((rows, cols));
                        let _ = crate::resize_terminal(term, BEST_ROWS, BEST_COLS);
                        let _ = term.flush();
                        continue 'redraw;
                    }
                    term_cols = cols;
                    if rows >= 10 && rows != term_rows {
                        term_rows = rows;
                        continue 'redraw;
                    }
                }
                // Idle: re-probe so a resize that happened while we sat still is
                // noticed (its reply arrives as a Resize on the next iteration).
                MenuEvent::Idle => {
                    let _ = crate::send_size_probe(&mut *term, !input.caps_resolved());
                    let _ = term.flush();
                }
            }
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
            // Page Up / Page Down scroll the game list a full page, keeping the
            // cursor at the same relative row in the viewport (SyncTERM sends
            // CSI V/U; xterm-family sends CSI 5~/6~ — both decode to these keys).
            Key::PageUp | Key::PageDown => {
                if state.current_section == MenuSection::GameList {
                    state.page(key == Key::PageUp);
                }
            }
            // Tab jumps between the Settings zone and the game list, keeping the
            // game-list selection where it was (so you can pop up, change a
            // setting, and come right back without scrolling).
            Key::Tab | Key::BackTab => {
                state.typeahead.clear();
                state.current_section = if state.current_section == MenuSection::GameList {
                    state.last_settings
                } else {
                    MenuSection::GameList
                };
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
                    MenuSection::Screen => {
                        state.screen = if key == Key::Left {
                            state.screen.prev()
                        } else {
                            state.screen.next()
                        };
                        apply_screen(state, term, screen_orig, term_rows, term_cols);
                    }
                    _ => {}
                }
            }
            // Z (A button) or Enter (Start): Select/Confirm
            Key::Char('z') | Key::Char('Z') | Key::Enter => {
                match state.current_section {
                    MenuSection::GameList => {
                        if let Some(rom_path) = state.selected_rom().cloned() {
                            // Remember this game so the list reopens on it after
                            // the player exits (this session and future ones).
                            if let Some(name) = rom_path.file_name() {
                                state.persist_last_game(&name.to_string_lossy());
                            }
                            return Ok(Some(MenuConfig {
                                rom_path,
                                render_mode: state.render_mode,
                                sound: state.sound,
                            }));
                        }
                    }
                    MenuSection::RenderMode => state.toggle_render(),
                    MenuSection::Audio => state.cycle_sound(),
                    MenuSection::Filter => state.cycle_filter(),
                    MenuSection::Screen => {
                        state.screen = state.screen.next();
                        apply_screen(state, term, screen_orig, term_rows, term_cols);
                    }
                }
            }
            _ => {}
        }

        // Remember the focused settings row so Tab can return to it.
        if state.current_section != MenuSection::GameList {
            state.last_settings = state.current_section;
        }
    }
}

fn draw_menu(stdout: &mut impl Write, state: &MenuState) -> io::Result<()> {
    // Paint the whole screen the Game Boy LCD background, then draw dark ink on it.
    emit!(stdout, SetBackgroundColor(SCREEN_BG), Clear(ClearType::All))?;

    // Hints at the very top so they stay in view on tall terminals.
    emit!(
        stdout,
        MoveTo(3, 0),
        SetForegroundColor(KEY_COLOR), Print("↑↓"), SetForegroundColor(DIM_COLOR), Print(" move   "),
        SetForegroundColor(KEY_COLOR), Print("Tab"), SetForegroundColor(DIM_COLOR), Print(" settings/list   "),
        SetForegroundColor(KEY_COLOR), Print("◄►"), SetForegroundColor(DIM_COLOR), Print(" change   "),
        SetForegroundColor(KEY_COLOR), Print("Z/Enter"), SetForegroundColor(DIM_COLOR), Print(" play   "),
        SetForegroundColor(KEY_COLOR), Print("Esc"), SetForegroundColor(DIM_COLOR), Print(" quit"),
        NORMAL
    )?;
    emit!(
        stdout,
        MoveTo(4, 1),
        SetForegroundColor(KEY_COLOR), Print("type"), SetForegroundColor(DIM_COLOR), Print(" any letters to jump to a game"),
        NORMAL
    )?;

    // Wordmark on the left (rows 3-7); Settings menu to its right. The settings
    // caret tracks last_settings so it stays put (dimmed) while the list is focused.
    draw_logo(stdout, 2, 3)?;

    let settings_x: u16 = 54;
    let zone_settings = state.current_section != MenuSection::GameList;
    draw_zone_header(stdout, settings_x, 3, "Settings", zone_settings)?;
    let render_label = match state.render_mode {
        RenderMode::Ascii => "ASCII",
        RenderMode::Block => "Block",
    };
    draw_option(stdout, settings_x, 4, "Render", render_label, state.last_settings == MenuSection::RenderMode, zone_settings)?;
    draw_option(stdout, settings_x, 5, "Sound", state.sound.label().trim(), state.last_settings == MenuSection::Audio, zone_settings)?;
    draw_option(stdout, settings_x, 6, "Filter", state.filter.label().trim(), state.last_settings == MenuSection::Filter, zone_settings)?;
    draw_option(stdout, settings_x, 7, "Size", state.screen.label(), state.last_settings == MenuSection::Screen, zone_settings)?;

    // Optional, low-key nudge about terminal size for the sharpest game picture.
    // Semi-dark ink on a semi-light olive bar so it reads as a tip, not a warning;
    // "any size still works" keeps it from scaring off smaller terminals. Hidden
    // when "Best" is selected (the door is already managing the size).
    if state.screen != ScreenSize::Best {
        emit!(
            stdout,
            MoveTo(2, 8),
            SetBackgroundColor(LIME),
            SetForegroundColor(INK_MID),
            Print(" Optional: set Size to Best (or use a 162x74 terminal) for the sharpest picture "),
            NORMAL
        )?;
    }

    // Games header + type-ahead indicator.
    let games_label = format!("Games: {} ({})", state.filter.label().trim(), state.filtered.len());
    draw_zone_header(
        stdout,
        4,
        GAMES_HEADER_Y,
        &games_label,
        state.current_section == MenuSection::GameList,
    )?;
    if state.current_section == MenuSection::GameList && !state.typeahead.is_empty() {
        emit!(
            stdout,
            MoveTo(44, GAMES_HEADER_Y),
            SetForegroundColor(KEY_COLOR),
            Print("Find: "),
            SetForegroundColor(TEXT_COLOR),
            Print(truncate_str(&state.typeahead, 20)),
            NORMAL
        )?;
    }

    let games_start_y = GAMES_START_Y;
    if state.filtered.is_empty() {
        let msg = if state.rom_files.is_empty() {
            "No ROMs found — add .gb/.gbc files beside the binary"
        } else {
            "No games match this filter"
        };
        emit!(stdout, MoveTo(4, games_start_y), SetForegroundColor(DIM_COLOR), Print(msg), NORMAL)?;
    } else {
        let visible_count = state.visible_rows.min(state.filtered.len());
        let is_game_list_selected = state.current_section == MenuSection::GameList;

        for (i, &rom_idx) in state.filtered.iter()
            .skip(state.scroll_offset)
            .take(visible_count)
            .enumerate()
        {
            let rom = &state.rom_files[rom_idx];
            let actual_index = state.scroll_offset + i;
            let is_selected = actual_index == state.selected_rom_index;

            let title = rom.file_stem()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            let is_gbc = rom.extension()
                .map(|e| e.to_string_lossy().eq_ignore_ascii_case("gbc"))
                .unwrap_or(false);

            draw_rom_row(
                stdout,
                games_start_y + i as u16,
                &title,
                is_gbc,
                is_selected,
                is_game_list_selected,
            )?;
        }

        // Scroll indicators
        let arrow_x: u16 = 65;
        if state.scroll_offset > 0 {
            emit!(stdout, MoveTo(arrow_x, games_start_y), SetForegroundColor(KEY_COLOR), Print("▲"), NORMAL)?;
        }
        if state.scroll_offset + visible_count < state.filtered.len() {
            emit!(stdout, MoveTo(arrow_x, games_start_y + visible_count as u16 - 1), SetForegroundColor(KEY_COLOR), Print("▼"), NORMAL)?;
        }
    }

    stdout.flush()?;
    Ok(())
}

/// Draw a single ROM row: " ▶ <title>   <TAG> ". The GB/GBC tag color flips with
/// the row's background so it stays legible on both the light list and the dark
/// selection bar. `selected`: this row is the game-list cursor. `zone_active`:
/// the game list is the focused zone — draws the dark selection bar and blinks
/// the caret; when inactive the caret is dimmed but the row stays marked.
fn draw_rom_row(
    stdout: &mut impl Write,
    y: u16,
    title: &str,
    is_gbc: bool,
    selected: bool,
    zone_active: bool,
) -> io::Result<()> {
    let tag = if is_gbc { "GBC" } else { "GB" };
    let name = format!("{:<width$} ", truncate_str(title, NAME_WIDTH), width = NAME_WIDTH);
    let bar = selected && zone_active;
    emit!(stdout, MoveTo(4, y))?;
    if bar {
        emit!(stdout, SetBackgroundColor(HIGHLIGHT_BG))?;
    }
    // Caret cell (" ▶ " or "   "), on the bar background when active.
    if selected {
        emit!(stdout, Print(" "))?;
        draw_caret(stdout, zone_active, HIGHLIGHT_FG)?;
        emit!(stdout, Print(" "))?;
    } else {
        emit!(stdout, Print("   "))?;
    }
    // Name + tag.
    let (name_color, tag_color) = if bar {
        (HIGHLIGHT_FG, if is_gbc { LIME } else { SCREEN_BG })
    } else if selected {
        (KEY_COLOR, if is_gbc { INK } else { INK_MID })
    } else {
        (TEXT_COLOR, if is_gbc { INK } else { INK_MID })
    };
    emit!(
        stdout,
        SetForegroundColor(name_color),
        Print(name),
        SetForegroundColor(tag_color),
        Print(format!("{:<3} ", tag)),
        NORMAL
    )?;
    Ok(())
}

/// `caret`: this row holds the settings cursor. `zone_active`: the settings menu
/// (not the game list) is the focused zone — drives blink vs dim on the caret.
fn draw_option(stdout: &mut impl Write, x: u16, y: u16, label: &str, value: &str, caret: bool, zone_active: bool) -> io::Result<()> {
    let label_color = if caret && zone_active { KEY_COLOR } else { TEXT_COLOR };
    let value_color = if caret { KEY_COLOR } else { DIM_COLOR };
    let arrow_color = if caret { KEY_COLOR } else { DIM_COLOR };
    emit!(stdout, MoveTo(x, y))?;
    if caret {
        draw_caret(stdout, zone_active, KEY_COLOR)?;
    } else {
        emit!(stdout, Print(" "))?;
    }
    emit!(
        stdout,
        SetForegroundColor(label_color),
        Print(format!(" {:<7}", label)),
        SetForegroundColor(arrow_color),
        Print("◄ "),
        SetForegroundColor(value_color),
        Print(format!("{:<5}", value)),
        SetForegroundColor(arrow_color),
        Print(" ►"),
        NORMAL
    )?;
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

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn menu_paints_the_gameboy_lcd_palette() {
        // Render the menu to a buffer (no terminal needed) and check the Game Boy
        // colors actually go out: the LCD background fill and the dark ink.
        let state = MenuState::new(None, None);
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = Cp437Writer::new(&mut buf);
            draw_menu(&mut w, &state).unwrap();
        }
        assert!(contains(&buf, b"\x1b[48;2;202;220;159m"), "LCD background not emitted");
        assert!(contains(&buf, b"\x1b[38;2;48;98;48m"), "ink text not emitted");
        // The block wordmark renders as CP437 full-block bytes (0xDB).
        assert!(buf.contains(&0xDB), "block wordmark not emitted");
        // The old blue/yellow scheme must be gone.
        assert!(!contains(&buf, b"\x1b[38;2;255;210;70m"), "stale yellow accent still present");
    }

    #[test]
    fn page_keeps_cursor_at_same_viewport_row() {
        let mut state = MenuState::new(None, None);
        // A controlled 100-item list in a 10-row viewport.
        state.filtered = (0..100).collect();
        state.visible_rows = 10;
        state.scroll_offset = 40;
        state.selected_rom_index = 45; // relative row 5 within the viewport

        state.page(false); // page down: scroll a page, keep the cursor on row 5
        assert_eq!((state.scroll_offset, state.selected_rom_index), (50, 55));
        state.page(true); // page up: back to exactly where we were
        assert_eq!((state.scroll_offset, state.selected_rom_index), (40, 45));

        // Already on the last page -> page down lands on the very last game.
        state.scroll_offset = 90; // max offset (100 items, 10 rows)
        state.selected_rom_index = 93;
        state.page(false);
        assert_eq!(state.selected_rom_index, 99);

        // Already on the first page -> page up lands on the first game.
        state.scroll_offset = 0;
        state.selected_rom_index = 4;
        state.page(true);
        assert_eq!(state.selected_rom_index, 0);
    }

    #[test]
    fn zone_header_blinks_only_when_focused() {
        let render = |active: bool| -> Vec<u8> {
            let mut buf: Vec<u8> = Vec::new();
            {
                let mut w = Cp437Writer::new(&mut buf);
                draw_zone_header(&mut w, 0, 0, "Settings", active).unwrap();
            }
            buf
        };
        let blink = b"\x1b[5m"; // SGR SlowBlink wraps the ══ of the focused zone
        assert!(contains(&render(true), blink), "focused header should blink its ══");
        assert!(!contains(&render(false), blink), "unfocused header must not blink");
        assert!(contains(&render(false), b"Settings"), "label still rendered");
    }
}
