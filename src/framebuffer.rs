use crate::color::ColorDepth;
use gameboy_core::{CGBColor, Color as GBColor, PixelMapper};

/// GameBoy screen dimensions
pub const GB_WIDTH: usize = 160;
pub const GB_HEIGHT: usize = 144;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Convert to greyscale using integer math (faster than floating point).
    /// Uses coefficients: R*77 + G*150 + B*29 ≈ R*0.299 + G*0.587 + B*0.114 (scaled by 256)
    #[inline]
    pub fn to_grey(self) -> u8 {
        ((self.r as u32 * 77 + self.g as u32 * 150 + self.b as u32 * 29) >> 8) as u8
    }
}

impl From<CGBColor> for Rgb {
    fn from(c: CGBColor) -> Self {
        Self {
            r: c.red,
            g: c.green,
            b: c.blue,
        }
    }
}

impl From<GBColor> for Rgb {
    fn from(c: GBColor) -> Self {
        // Default (grayscale) mapping. DMG games can instead request the classic
        // green LCD palette via `DmgPalette` (see `FrameBuffer::dmg_rgb`).
        DmgPalette::Gray.rgb(c)
    }
}

/// Which palette a non-color (DMG) game's four shades map to. Only affects games
/// that render through the 4-shade path (`map_pixel`); Game Boy Color titles
/// render through `cgb_map_pixel` and are unaffected.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DmgPalette {
    /// Neutral grayscale — the safe default; degrades cleanly on any terminal.
    #[default]
    Gray,
    /// The original Game Boy pea-green LCD palette (matches the splash/menu).
    Green,
}

impl DmgPalette {
    pub fn slug(self) -> &'static str {
        match self {
            DmgPalette::Gray => "gray",
            DmgPalette::Green => "green",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DmgPalette::Gray => "Gray",
            DmgPalette::Green => "Green",
        }
    }

    pub fn parse(s: &str) -> Option<DmgPalette> {
        match s.trim().to_lowercase().as_str() {
            "gray" | "grey" | "grayscale" | "bw" | "mono" => Some(DmgPalette::Gray),
            "green" | "dmg" | "lcd" => Some(DmgPalette::Green),
            _ => None,
        }
    }

    pub fn next(self) -> DmgPalette {
        match self {
            DmgPalette::Gray => DmgPalette::Green,
            DmgPalette::Green => DmgPalette::Gray,
        }
    }

    /// The palette to actually render with at a given color depth. The green LCD
    /// only reads well with extended color: in 16-color its greens quantize to
    /// muddy near-greens, whereas grayscale maps cleanly onto the ANSI grays. So
    /// green falls back to gray when the depth is 16 (the menu still shows the
    /// user's Green choice; it just applies once they're on a color terminal).
    pub fn resolved_for(self, depth: ColorDepth) -> DmgPalette {
        if depth == ColorDepth::C16 {
            DmgPalette::Gray
        } else {
            self
        }
    }

    /// Map one of the four DMG shades to display RGB for this palette. The green
    /// values are the canonical Game Boy LCD palette (#9bbc0f / #8bac0f / #306230
    /// / #0f380f), the same greens the menu and splash use.
    #[inline]
    pub fn rgb(self, c: GBColor) -> Rgb {
        match self {
            DmgPalette::Gray => match c {
                GBColor::White => Rgb { r: 255, g: 255, b: 255 },
                GBColor::LightGray => Rgb { r: 170, g: 170, b: 170 },
                GBColor::DarkGray => Rgb { r: 85, g: 85, b: 85 },
                GBColor::Black => Rgb { r: 0, g: 0, b: 0 },
            },
            DmgPalette::Green => match c {
                GBColor::White => Rgb { r: 155, g: 188, b: 15 },
                GBColor::LightGray => Rgb { r: 139, g: 172, b: 15 },
                GBColor::DarkGray => Rgb { r: 48, g: 98, b: 48 },
                GBColor::Black => Rgb { r: 15, g: 56, b: 15 },
            },
        }
    }
}

pub struct FrameBuffer {
    pub width: usize,
    #[allow(dead_code)] // Kept for structural completeness
    pub height: usize,
    pub pixels: Vec<Rgb>,
    /// Palette applied to DMG (4-shade) games; ignored by CGB games.
    dmg_palette: DmgPalette,
}

impl FrameBuffer {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![Rgb { r: 0, g: 0, b: 0 }; width * height],
            dmg_palette: DmgPalette::default(),
        }
    }

    /// Choose the palette DMG (4-shade) games render in. CGB games are unaffected
    /// (they render through `cgb_map_pixel`, which ignores this).
    pub fn set_dmg_palette(&mut self, palette: DmgPalette) {
        self.dmg_palette = palette;
    }

    #[inline]
    pub fn set_pixel(&mut self, index: usize, color: Rgb) {
        // Safety: GameBoy emulator always writes valid pixel indices (0..160*144)
        // Removing bounds check eliminates 23,040 branches per frame
        unsafe {
            *self.pixels.get_unchecked_mut(index) = color;
        }
    }

    #[inline]
    pub fn get_pixel(&self, x: usize, y: usize) -> Rgb {
        self.pixels[y * self.width + x]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmg_palettes_map_the_four_shades() {
        // Gray is the neutral default; Green is the canonical DMG LCD palette
        // (matches the menu/splash greens: 9bbc0f / 8bac0f / 306230 / 0f380f).
        assert_eq!(DmgPalette::default(), DmgPalette::Gray);
        assert_eq!(DmgPalette::Gray.rgb(GBColor::White), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(DmgPalette::Gray.rgb(GBColor::Black), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(DmgPalette::Green.rgb(GBColor::White), Rgb { r: 155, g: 188, b: 15 });
        assert_eq!(DmgPalette::Green.rgb(GBColor::LightGray), Rgb { r: 139, g: 172, b: 15 });
        assert_eq!(DmgPalette::Green.rgb(GBColor::DarkGray), Rgb { r: 48, g: 98, b: 48 });
        assert_eq!(DmgPalette::Green.rgb(GBColor::Black), Rgb { r: 15, g: 56, b: 15 });
        assert_eq!(DmgPalette::parse("green"), Some(DmgPalette::Green));
        assert_eq!(DmgPalette::parse("GRAY"), Some(DmgPalette::Gray));
    }

    #[test]
    fn green_falls_back_to_gray_only_in_16_color() {
        // Green renders in truecolor/256, but 16-color forces gray (clean ANSI
        // grays beat muddy quantized greens). Gray is unaffected everywhere.
        assert_eq!(DmgPalette::Green.resolved_for(ColorDepth::True), DmgPalette::Green);
        assert_eq!(DmgPalette::Green.resolved_for(ColorDepth::C256), DmgPalette::Green);
        assert_eq!(DmgPalette::Green.resolved_for(ColorDepth::C16), DmgPalette::Gray);
        assert_eq!(DmgPalette::Gray.resolved_for(ColorDepth::C16), DmgPalette::Gray);
    }
}

impl PixelMapper for FrameBuffer {
    fn map_pixel(&mut self, pixel: usize, color: GBColor) {
        let rgb = self.dmg_palette.rgb(color);
        self.set_pixel(pixel, rgb);
    }

    fn cgb_map_pixel(&mut self, pixel: usize, color: CGBColor) {
        self.set_pixel(pixel, color.into());
    }
}

