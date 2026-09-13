//! Sprite sheet loading and frame extraction.
//!
//! The sheet is a grid of fixed-size tiles. How it is divided up is declared by
//! the pet file rather than assumed here, so this module only decodes pixels.

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
