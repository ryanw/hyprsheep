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
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        // Pet sheets come in whatever the author saved: palette-indexed,
        // greyscale, RGB or RGBA. Ask the decoder to expand the lot and give
        // us an alpha channel, so the blit only ever sees one layout.
        decoder.set_transformations(
            png::Transformations::EXPAND | png::Transformations::ALPHA,
        );
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
            png::ColorType::Rgb => {
                px.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 0xFF]).collect()
            }
            png::ColorType::GrayscaleAlpha => {
                px.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect()
            }
            png::ColorType::Grayscale => {
                px.iter().flat_map(|&g| [g, g, g, 0xFF]).collect()
            }
            other => return Err(format!("unsupported png colour type {other:?}")),
        };

        Ok(Sheet { width: info.width, height: info.height, pixels })
    }

    /// One row of the sheet, as straight-alpha RGBA.
    ///
    /// The blit walks a row at a time, so it pays for the bounds check once
    /// per row rather than once per pixel. A row off the end of the sheet
    /// comes back empty, which reads as transparent.
    #[inline]
    pub fn row(&self, y: u32) -> &[u8] {
        if y >= self.height {
            return &[];
        }
        let i = (y * self.width * 4) as usize;
        &self.pixels[i..i + (self.width * 4) as usize]
    }
}
