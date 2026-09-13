//! Sprite sheet loading and frame extraction.
//!
//! The classic eSheep sheet is a grid of fixed-size tiles. Frames are addressed
//! by pixel offset into the sheet, matching the convention the original
//! `animations.xml` uses.

pub const TILE: u32 = 40;

pub struct Sheet {
    pub width: u32,
    pub height: u32,
    /// Straight (non-premultiplied) RGBA8.
    pixels: Vec<u8>,
}

impl Sheet {
    pub fn load(bytes: &[u8]) -> Result<Self, String> {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().map_err(|e| format!("png header: {e}"))?;
        let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buf).map_err(|e| format!("png data: {e}"))?;

        if info.bit_depth != png::BitDepth::Eight {
            return Err(format!("expected 8-bit png, got {:?}", info.bit_depth));
        }

        // Normalise whatever colour type we got into straight RGBA8.
        let px = &buf[..info.buffer_size()];
        let pixels = match info.color_type {
            png::ColorType::Rgba => px.to_vec(),
            png::ColorType::Rgb => px
                .chunks_exact(3)
                .flat_map(|c| [c[0], c[1], c[2], 0xFF])
                .collect(),
            other => return Err(format!("unsupported png colour type {other:?}")),
        };

        Ok(Sheet { width: info.width, height: info.height, pixels })
    }

    pub fn cols(&self) -> u32 {
        self.width / TILE
    }

    pub fn rows(&self) -> u32 {
        self.height / TILE
    }

    /// Straight-alpha RGBA for one pixel of the sheet.
    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 0];
        }
        let i = ((y * self.width + x) * 4) as usize;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2], self.pixels[i + 3]]
    }
}

/// A single animation frame: a TILE-sized window into the sheet, addressed by
/// its top-left pixel offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub x: u32,
    pub y: u32,
}

impl Frame {
    pub const fn at(x: u32, y: u32) -> Self {
        Frame { x, y }
    }

    /// Convenience for addressing by grid cell rather than pixel offset.
    pub const fn cell(col: u32, row: u32) -> Self {
        Frame { x: col * TILE, y: row * TILE }
    }

    /// The original addresses frames as linear, row-major tile indices.
    pub const fn index(i: u32, cols: u32) -> Self {
        Frame { x: (i % cols) * TILE, y: (i / cols) * TILE }
    }
}
