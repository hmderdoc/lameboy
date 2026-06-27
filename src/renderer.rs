use crate::framebuffer::{FrameBuffer, GB_HEIGHT, GB_WIDTH};
use std::io::{self, Write};

/// Native Game Boy terminal rows (144 pixels / 2 for half-block = 72)
const NATIVE_COLS: usize = GB_WIDTH;      // 160
const NATIVE_ROWS: usize = GB_HEIGHT / 2; // 72

/// ASCII character palette (ordered by brightness, dark to light)
const ASCII_CHARS: &[u8] = b" .'`^\",:;Il!i><~+_-?][}{1)(|\\/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@$";

#[derive(Clone, Copy, PartialEq)]
pub enum RenderMode {
    Ascii,
    Block,
}

/// One terminal cell of the last frame we transmitted. Block mode keys on
/// (fg, bg) with `ch` fixed to the half-block; ASCII mode keys on (ch, fg) with
/// `bg` fixed to black. Storing all three lets both modes share one cache.
#[derive(Clone, Copy, PartialEq)]
struct Cell {
    ch: u8,
    fg: u8,
    bg: u8,
}

impl Cell {
    /// A value no real cell can take (`rgb_to_256` only yields 16..=231 and a
    /// drawn glyph is 0xDF or printable ASCII), so an invalidated cache forces
    /// every cell to repaint.
    const SENTINEL: Cell = Cell { ch: 0, fg: 255, bg: 255 };
}

/// CP437 byte for ▀ (upper half block). Synchronet treats door output as CP437:
/// this byte passes through to CP437 clients and is mapped to U+2580 for UTF-8
/// clients. Emitting the raw UTF-8 ▀ instead would garble on both.
/// (Local patch -- see PATCH-NOTES.md)
const HALF_BLOCK: u8 = 0xDF;

pub struct RenderConfig {
    pub mode: RenderMode,
}

pub struct Renderer {
    config: RenderConfig,
    output_buffer: Vec<u8>,
    last_fg: u8,  // 256-color index
    last_bg: u8,  // 256-color index
    needs_clear: bool,  // Set when terminal needs full clear (e.g., resize)

    // Scaled output dimensions (terminal cells)
    out_cols: u16,
    out_rows: u16,
    // Centering offsets (terminal cells)
    left_pad: u16,
    top_pad: u16,
    // Nearest-neighbor lookup tables (rebuilt on each dimension update)
    col_map: Vec<usize>,  // col_map[j] = source GB x for output column j
    row_map: Vec<usize>,  // row_map[s] = source GB y for output subpixel row s

    // Delta encoding: the last frame we actually transmitted, one entry per
    // output cell (row-major, out_cols * out_rows). We repaint only the cells
    // that changed and jump the cursor over unchanged runs. Sized lazily and
    // invalidated (all SENTINEL) on a full clear/resize so the next frame
    // repaints in full and re-syncs the terminal.
    prev_cells: Vec<Cell>,

    // When set, the next frame repaints every cell (without a screen clear) so
    // any cell corrupted by line noise self-heals. Driven by a periodic
    // keyframe timer in the main loop.
    force_repaint: bool,
}

impl Renderer {
    pub fn new(config: RenderConfig) -> Self {
        // Start at native size (updated before first frame via update_dimensions)
        let out_cols = NATIVE_COLS as u16;
        let out_rows = NATIVE_ROWS as u16;
        let col_map: Vec<usize> = (0..NATIVE_COLS).collect();
        let row_map: Vec<usize> = (0..GB_HEIGHT).collect();

        // Estimate ~12 bytes per cell worst case with 256-color mode + cursor escapes
        let buffer_size = GB_WIDTH * (GB_HEIGHT / 2) * 16;
        Self {
            config,
            output_buffer: Vec::with_capacity(buffer_size),
            last_fg: 255, // Invalid sentinel
            last_bg: 255,
            needs_clear: true, // Clear on first frame
            out_cols,
            out_rows,
            left_pad: 0,
            top_pad: 0,
            col_map,
            row_map,
            prev_cells: Vec::new(),
            force_repaint: false,
        }
    }

    /// Ensure the delta cache matches the current output dimensions. A size
    /// change (first frame or resize) reallocates and invalidates it, so every
    /// cell repaints.
    fn ensure_cache(&mut self) {
        let needed = self.out_cols as usize * self.out_rows as usize;
        if self.prev_cells.len() != needed {
            self.prev_cells = vec![Cell::SENTINEL; needed];
        }
    }

    /// Force the next frame to repaint every cell (used after a screen clear).
    fn invalidate_cache(&mut self) {
        for c in self.prev_cells.iter_mut() {
            *c = Cell::SENTINEL;
        }
    }

    /// Update output dimensions based on the current terminal size.
    /// Call on startup and on every resize event.
    pub fn update_dimensions(&mut self, cols: u16, rows: u16) {
        // Guard against degenerate terminal sizes
        if cols == 0 || rows == 0 {
            return;
        }

        // Reserve 1 row for the FPS display below the game
        let usable_rows = rows.saturating_sub(1).max(1);

        // Scale factor: fit both axes, preserve aspect ratio.
        // f < 1 → downscale; f > 1 → upscale.
        let f = (cols as f64 / NATIVE_COLS as f64)
            .min(usable_rows as f64 / NATIVE_ROWS as f64);

        let out_cols = ((NATIVE_COLS as f64 * f).round() as u16).max(1);
        let out_rows = ((NATIVE_ROWS as f64 * f).round() as u16).max(1);
        let out_subrows = (out_rows as usize) * 2;

        // Center horizontally and vertically in the available space
        let left_pad = cols.saturating_sub(out_cols) / 2;
        let top_pad = usable_rows.saturating_sub(out_rows) / 2;

        // Build nearest-neighbor lookup tables
        let col_map: Vec<usize> = (0..out_cols as usize)
            .map(|j| j * GB_WIDTH / out_cols as usize)
            .collect();
        let row_map: Vec<usize> = (0..out_subrows)
            .map(|s| s * GB_HEIGHT / out_subrows)
            .collect();

        self.out_cols = out_cols;
        self.out_rows = out_rows;
        self.left_pad = left_pad;
        self.top_pad = top_pad;
        self.col_map = col_map;
        self.row_map = row_map;

        self.request_clear();
    }

    /// Terminal row for the FPS overlay (just below the game image, 0-based).
    pub fn fps_row(&self) -> u16 {
        self.top_pad + self.out_rows
    }

    /// Mark that the terminal needs a full clear (e.g., after resize)
    pub fn request_clear(&mut self) {
        self.needs_clear = true;
    }

    /// Repaint every cell on the next frame (no screen clear). Used as a periodic
    /// keyframe so a cell corrupted by line noise self-heals on a lossy link.
    pub fn request_repaint(&mut self) {
        self.force_repaint = true;
    }

    pub fn render<W: Write + ?Sized>(&mut self, fb: &FrameBuffer, out: &mut W) -> io::Result<()> {
        match self.config.mode {
            RenderMode::Block => self.render_block(fb, out),
            RenderMode::Ascii => self.render_ascii(fb, out),
        }
    }

    /// Convert RGB to 256-color palette index (6x6x6 color cube: indices 16-231)
    #[inline]
    fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
        // Map 0-255 to 0-5 for each channel, then to color cube index
        let r6 = (r as u16 * 6 / 256) as u8;
        let g6 = (g as u16 * 6 / 256) as u8;
        let b6 = (b as u16 * 6 / 256) as u8;
        16 + 36 * r6 + 6 * g6 + b6
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        if n >= 100 {
            self.output_buffer.push(b'0' + n / 100);
            self.output_buffer.push(b'0' + (n / 10) % 10);
            self.output_buffer.push(b'0' + n % 10);
        } else if n >= 10 {
            self.output_buffer.push(b'0' + n / 10);
            self.output_buffer.push(b'0' + n % 10);
        } else {
            self.output_buffer.push(b'0' + n);
        }
    }

    /// Write a u16 as decimal bytes into the output buffer
    #[inline]
    fn write_u16(&mut self, n: u16) {
        if n >= 10000 {
            self.output_buffer.push(b'0' + (n / 10000) as u8);
            self.output_buffer.push(b'0' + ((n / 1000) % 10) as u8);
            self.output_buffer.push(b'0' + ((n / 100) % 10) as u8);
            self.output_buffer.push(b'0' + ((n / 10) % 10) as u8);
            self.output_buffer.push(b'0' + (n % 10) as u8);
        } else if n >= 1000 {
            self.output_buffer.push(b'0' + (n / 1000) as u8);
            self.output_buffer.push(b'0' + ((n / 100) % 10) as u8);
            self.output_buffer.push(b'0' + ((n / 10) % 10) as u8);
            self.output_buffer.push(b'0' + (n % 10) as u8);
        } else if n >= 100 {
            self.output_buffer.push(b'0' + (n / 100) as u8);
            self.output_buffer.push(b'0' + ((n / 10) % 10) as u8);
            self.output_buffer.push(b'0' + (n % 10) as u8);
        } else if n >= 10 {
            self.output_buffer.push(b'0' + (n / 10) as u8);
            self.output_buffer.push(b'0' + (n % 10) as u8);
        } else {
            self.output_buffer.push(b'0' + n as u8);
        }
    }

    /// Emit absolute cursor-position escape: ESC[row;colH (1-based)
    #[inline]
    fn move_to(&mut self, row: u16, col: u16) {
        self.output_buffer.push(b'\x1b');
        self.output_buffer.push(b'[');
        self.write_u16(row);
        self.output_buffer.push(b';');
        self.write_u16(col);
        self.output_buffer.push(b'H');
    }

    /// Set foreground using 256-color mode (much shorter escape sequences)
    #[inline]
    fn set_fg_256(&mut self, color: u8) {
        if color != self.last_fg {
            self.output_buffer.extend_from_slice(b"\x1b[38;5;");
            self.write_u8(color);
            self.output_buffer.push(b'm');
            self.last_fg = color;
        }
    }

    /// Set background using 256-color mode
    #[inline]
    fn set_bg_256(&mut self, color: u8) {
        if color != self.last_bg {
            self.output_buffer.extend_from_slice(b"\x1b[48;5;");
            self.write_u8(color);
            self.output_buffer.push(b'm');
            self.last_bg = color;
        }
    }

    #[inline]
    fn brightness_to_ascii(brightness: u8) -> u8 {
        // Map 0-255 brightness to ASCII char index using integer math
        let index = (brightness as usize * (ASCII_CHARS.len() - 1)) / 255;
        ASCII_CHARS[index]
    }

    fn render_block<W: Write + ?Sized>(&mut self, fb: &FrameBuffer, out: &mut W) -> io::Result<()> {
        self.output_buffer.clear();
        self.ensure_cache();

        // Clear entire screen if needed (resize, first frame, etc.) and force a
        // full repaint so the delta cache re-syncs with a blank terminal.
        if self.needs_clear {
            self.output_buffer.extend_from_slice(b"\x1b[2J");
            self.invalidate_cache();
            self.needs_clear = false;
        } else if self.force_repaint {
            // Periodic keyframe: repaint everything (no clear) to heal corruption.
            self.invalidate_cache();
        }
        self.force_repaint = false;

        // We reset colors at the end of every frame (\x1b[0m), so the terminal
        // starts each frame at default; sentinels force the first emitted cell
        // to (re)assert its colors.
        self.last_fg = 255;
        self.last_bg = 255;

        let out_cols = self.out_cols as usize;
        let out_rows = self.out_rows as usize;

        for i in 0..out_rows {
            let row = self.top_pad + i as u16 + 1; // 1-based
            let sy_top = self.row_map[2 * i];
            let sy_bot = self.row_map[2 * i + 1];

            // `drawing` tracks whether the terminal cursor is positioned to emit
            // the current cell (i.e. we're mid-run). An unchanged cell ends the
            // run; the next changed cell re-homes the cursor with move_to.
            let mut drawing = false;

            for j in 0..out_cols {
                let sx = self.col_map[j];
                let top = fb.get_pixel(sx, sy_top);
                let bottom = fb.get_pixel(sx, sy_bot);

                // Use 256-color palette for shorter escape sequences
                let fg_color = Self::rgb_to_256(top.r, top.g, top.b);
                let bg_color = Self::rgb_to_256(bottom.r, bottom.g, bottom.b);

                let idx = i * out_cols + j;
                let cell = Cell { ch: HALF_BLOCK, fg: fg_color, bg: bg_color };
                if self.prev_cells[idx] == cell {
                    drawing = false; // unchanged: skip, breaking any run
                    continue;
                }

                if !drawing {
                    self.move_to(row, self.left_pad + 1 + j as u16);
                    drawing = true;
                }
                self.set_fg_256(fg_color);
                self.set_bg_256(bg_color);
                self.output_buffer.push(HALF_BLOCK);
                self.prev_cells[idx] = cell;
            }
        }

        // Reset colors at the end
        self.output_buffer.extend_from_slice(b"\x1b[0m");

        out.write_all(&self.output_buffer)?;
        out.flush()?;

        Ok(())
    }

    fn render_ascii<W: Write + ?Sized>(&mut self, fb: &FrameBuffer, out: &mut W) -> io::Result<()> {
        self.output_buffer.clear();
        self.ensure_cache();

        // Clear entire screen if needed (resize, first frame, etc.) and force a
        // full repaint so the delta cache re-syncs with a blank terminal.
        if self.needs_clear {
            self.output_buffer.extend_from_slice(b"\x1b[2J");
            self.invalidate_cache();
            self.needs_clear = false;
        } else if self.force_repaint {
            // Periodic keyframe: repaint everything (no clear) to heal corruption.
            self.invalidate_cache();
        }
        self.force_repaint = false;

        // The end-of-frame reset clears the background, so re-assert black bg
        // (256-color index 16) once per frame; it stays active for every cell we
        // emit below. last_fg sentinel forces the first changed cell to set fg.
        self.last_fg = 255;
        self.output_buffer.extend_from_slice(b"\x1b[48;5;16m");

        let out_cols = self.out_cols as usize;
        let out_rows = self.out_rows as usize;

        for i in 0..out_rows {
            let row = self.top_pad + i as u16 + 1; // 1-based
            let sy_top = self.row_map[2 * i];
            let sy_bot = self.row_map[2 * i + 1];

            // See render_block: a run of changed cells shares one move_to; an
            // unchanged cell ends the run.
            let mut drawing = false;

            for j in 0..out_cols {
                let sx = self.col_map[j];
                let top = fb.get_pixel(sx, sy_top);
                let bottom = fb.get_pixel(sx, sy_bot);

                // Average two u8 greys using integer math (avoids overflow)
                let avg_grey = ((top.to_grey() as u16 + bottom.to_grey() as u16) >> 1) as u8;
                let ascii_char = Self::brightness_to_ascii(avg_grey);

                let fg_r = ((top.r as u16 + bottom.r as u16) >> 1) as u8;
                let fg_g = ((top.g as u16 + bottom.g as u16) >> 1) as u8;
                let fg_b = ((top.b as u16 + bottom.b as u16) >> 1) as u8;

                let fg_color = Self::rgb_to_256(fg_r, fg_g, fg_b);

                let idx = i * out_cols + j;
                let cell = Cell { ch: ascii_char, fg: fg_color, bg: 16 };
                if self.prev_cells[idx] == cell {
                    drawing = false; // unchanged: skip, breaking any run
                    continue;
                }

                if !drawing {
                    self.move_to(row, self.left_pad + 1 + j as u16);
                    drawing = true;
                }
                self.set_fg_256(fg_color);
                self.output_buffer.push(ascii_char);
                self.prev_cells[idx] = cell;
            }
        }

        // Reset colors at the end
        self.output_buffer.extend_from_slice(b"\x1b[0m");

        out.write_all(&self.output_buffer)?;
        out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::{FrameBuffer, Rgb, GB_HEIGHT, GB_WIDTH};

    fn count(hay: &[u8], needle: u8) -> usize {
        hay.iter().filter(|&&b| b == needle).count()
    }
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn block_delta_skips_unchanged_and_repaints_only_changed() {
        let mut r = Renderer::new(RenderConfig { mode: RenderMode::Block });
        r.update_dimensions(20, 11); // -> 20x9 output grid
        let cells = r.out_cols as usize * r.out_rows as usize;
        assert_eq!(cells, 180);

        let fb = FrameBuffer::new(GB_WIDTH, GB_HEIGHT);

        // Frame 1: first frame clears and paints every cell.
        let mut b1 = Vec::new();
        r.render(&fb, &mut b1).unwrap();
        assert!(contains(&b1, b"\x1b[2J"), "first frame must clear");
        assert_eq!(count(&b1, HALF_BLOCK), cells, "first frame paints every cell");

        // Frame 2: identical frame -> nothing changed -> only the trailing reset.
        let mut b2 = Vec::new();
        r.render(&fb, &mut b2).unwrap();
        assert_eq!(b2, b"\x1b[0m", "unchanged frame emits no cells");

        // Frame 3: change exactly one source pixel mapping to a single cell (0,0).
        let mut fb2 = FrameBuffer::new(GB_WIDTH, GB_HEIGHT);
        fb2.pixels[0] = Rgb { r: 255, g: 255, b: 255 };
        let mut b3 = Vec::new();
        r.render(&fb2, &mut b3).unwrap();
        assert!(!contains(&b3, b"\x1b[2J"), "no clear on a steady frame");
        assert_eq!(count(&b3, HALF_BLOCK), 1, "exactly one changed cell repainted");
        assert!(contains(&b3, b"\x1b[1;1H"), "cursor homed to the changed cell");
        assert!(b3.len() < b1.len(), "delta frame smaller than a full repaint");
    }

    #[test]
    fn resize_forces_full_repaint() {
        let mut r = Renderer::new(RenderConfig { mode: RenderMode::Block });
        r.update_dimensions(20, 11);
        let fb = FrameBuffer::new(GB_WIDTH, GB_HEIGHT);
        let mut sink = Vec::new();
        r.render(&fb, &mut sink).unwrap(); // first frame
        sink.clear();
        r.render(&fb, &mut sink).unwrap(); // steady -> empty
        assert_eq!(sink, b"\x1b[0m");

        // A resize re-requests a clear; the next frame must repaint everything.
        r.update_dimensions(20, 11);
        let cells = r.out_cols as usize * r.out_rows as usize;
        let mut b = Vec::new();
        r.render(&fb, &mut b).unwrap();
        assert!(contains(&b, b"\x1b[2J"));
        assert_eq!(count(&b, HALF_BLOCK), cells);
    }

    #[test]
    fn ascii_delta_steady_frame_is_minimal() {
        let mut r = Renderer::new(RenderConfig { mode: RenderMode::Ascii });
        r.update_dimensions(20, 11);
        let fb = FrameBuffer::new(GB_WIDTH, GB_HEIGHT);
        let mut b1 = Vec::new();
        r.render(&fb, &mut b1).unwrap();
        assert!(contains(&b1, b"\x1b[2J"));
        // Steady frame: black-bg assert + reset, but no cursor moves or glyphs.
        let mut b2 = Vec::new();
        r.render(&fb, &mut b2).unwrap();
        assert_eq!(b2, b"\x1b[48;5;16m\x1b[0m", "unchanged ASCII frame draws no cells");
    }
}
