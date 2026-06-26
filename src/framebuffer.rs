use gameboy_core::{CGBColor, Color as GBColor, PixelMapper};

/// GameBoy screen dimensions
pub const GB_WIDTH: usize = 160;
pub const GB_HEIGHT: usize = 144;

#[derive(Clone, Copy, PartialEq)]
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
        match c {
            GBColor::White => Self { r: 255, g: 255, b: 255 },
            GBColor::LightGray => Self { r: 170, g: 170, b: 170 },
            GBColor::DarkGray => Self { r: 85, g: 85, b: 85 },
            GBColor::Black => Self { r: 0, g: 0, b: 0 },
        }
    }
}

pub struct FrameBuffer {
    pub width: usize,
    #[allow(dead_code)] // Kept for structural completeness
    pub height: usize,
    pub pixels: Vec<Rgb>,
}

impl FrameBuffer {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![Rgb { r: 0, g: 0, b: 0 }; width * height],
        }
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

impl PixelMapper for FrameBuffer {
    fn map_pixel(&mut self, pixel: usize, color: GBColor) {
        self.set_pixel(pixel, color.into());
    }

    fn cgb_map_pixel(&mut self, pixel: usize, color: CGBColor) {
        self.set_pixel(pixel, color.into());
    }
}

