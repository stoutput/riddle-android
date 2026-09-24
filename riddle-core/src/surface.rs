//! Drawing surface abstraction: same drawing code renders into either the
//! qtfb RGB565 shared memory (in-xochitl backend) or the vendor engine's
//! RGB32 aux framebuffer (takeover backend). Colors are RGB565 u16 at the
//! API; the surface converts on write.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PixFmt {
    /// 2 bytes/px, little-endian RGB565 (qtfb FBFMT_RMPP_RGB565).
    Rgb565,
    /// 4 bytes/px, QImage Format_RGB32: bytes B,G,R,0xFF. Unused on Android
    /// (the bitmap is RGB_565) but retained so the ported drawing code stays
    /// directly comparable with upstream.
    #[allow(dead_code)]
    Rgb32,
}

pub struct Surface {
    ptr: *mut u8,
    len: usize,
    pub w: usize,
    pub h: usize,
    pub stride: usize,
    pub fmt: PixFmt,
}

// Single-threaded writer over a long-lived mapping.
unsafe impl Send for Surface {}

pub const WHITE: u16 = 0xFFFF;
pub const BLACK: u16 = 0x0000;
/// Old ink: how the diary writes its memories (a readable e-ink gray).
pub const FADED: u16 = 0x7BCF;

#[inline]
fn expand565(c: u16) -> (u8, u8, u8) {
    let r = ((c >> 11) & 0x1f) as u32;
    let g = ((c >> 5) & 0x3f) as u32;
    let b = (c & 0x1f) as u32;
    (
        ((r * 255 + 15) / 31) as u8,
        ((g * 255 + 31) / 63) as u8,
        ((b * 255 + 15) / 31) as u8,
    )
}

impl Surface {
    /// Wrap a buffer someone else owns — the reMarkable backends' shared
    /// memory, and the host unit tests.
    ///
    /// # Safety
    /// `ptr` must point to `len` writable bytes that outlive this `Surface`.
    #[allow(dead_code)] // used by the host tests, which wrap a Vec
    pub fn new(ptr: *mut u8, len: usize, w: usize, h: usize, stride: usize, fmt: PixFmt) -> Self {
        Self {
            ptr,
            len,
            w,
            h,
            stride,
            fmt,
        }
    }

    /// A page the engine owns, as an RGB565 buffer. This is the Android path:
    /// the view copies the pixels out after each dirty frame.
    pub fn new_owned(w: usize, h: usize) -> Self {
        let len = w * h * 2;
        // Leaked deliberately: the surface lives for the whole process, and a
        // raw pointer with a stable address is what the drawing code wants.
        // 1620x2160x2 is 7 MB, once.
        let buf: &'static mut [u8] = Vec::leak(vec![0u8; len]);
        Self {
            ptr: buf.as_mut_ptr(),
            len,
            w,
            h,
            stride: w * 2,
            fmt: PixFmt::Rgb565,
        }
    }

    #[inline]
    fn buf(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    #[inline]
    fn buf_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The whole surface as RGB565 pixels, row by row.
    ///
    /// This is how the Android UI gets the page: the engine owns a plain
    /// `Vec<u16>` and the view copies it into a `Bitmap` after each dirty
    /// frame, rather than the engine drawing straight into the bitmap's
    /// buffer. Reading a `Bitmap`'s pixels from native code needs either the
    /// NDK's `libjnigraphics` (a C header and stub) or its hidden `mBuffer`
    /// field, which JNI cannot see on current Android. An owned buffer has
    /// none of that coupling and is testable on the host.
    ///
    /// Only correct for `PixFmt::Rgb565`, where the stride is exactly
    /// `w * 2`; asserts so in debug builds.
    pub fn pixels(&self) -> &[u16] {
        debug_assert!(matches!(self.fmt, PixFmt::Rgb565));
        debug_assert_eq!(self.stride, self.w * 2);
        let bytes = self.buf_ref();
        // Safety: `put_px`/`fill_rect` maintain little-endian RGB565 pairs, and
        // the buffer is `stride * h` bytes with stride == w * 2. Alignment is
        // guaranteed because the buffer is allocated as a `Vec<u16>`.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u16, self.w * self.h) }
    }

    #[inline]
    pub fn put_px(&mut self, x: i32, y: i32, c: u16) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let (stride, fmt) = (self.stride, self.fmt);
        match fmt {
            PixFmt::Rgb565 => {
                let i = y as usize * stride + x as usize * 2;
                let b = self.buf();
                b[i] = (c & 0xff) as u8;
                b[i + 1] = (c >> 8) as u8;
            }
            PixFmt::Rgb32 => {
                let (r, g, bl) = expand565(c);
                let i = y as usize * stride + x as usize * 4;
                let b = self.buf();
                b[i] = bl;
                b[i + 1] = g;
                b[i + 2] = r;
                b[i + 3] = 0xFF;
            }
        }
    }

    /// Luminance 0..255 — used by the PNG rasterizer and dissolve inkness test.
    #[inline]
    pub fn luma(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return 255;
        }
        let b = self.buf_ref();
        match self.fmt {
            PixFmt::Rgb565 => {
                let i = y as usize * self.stride + x as usize * 2;
                let px = (b[i] as u16) | ((b[i + 1] as u16) << 8);
                (((px >> 5) & 0x3f) as u32 * 255 / 63) as u8
            }
            PixFmt::Rgb32 => {
                let i = y as usize * self.stride + x as usize * 4;
                // Green approximates luma well enough for mono ink.
                b[i + 1]
            }
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: u16) {
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        for row in y..y1 {
            for col in x..x1 {
                self.put_px(col as i32, row as i32, c);
            }
        }
    }

    /// Invert the RGB of a rect (cursor/pressed-key feedback). Upstream UI
    /// affordance; the Android view uses normal Android widgets for chrome.
    #[allow(dead_code)]
    pub fn invert_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        let (stride, fmt) = (self.stride, self.fmt);
        let buf = self.buf();
        for row in y..y1 {
            match fmt {
                PixFmt::Rgb565 => {
                    let s = row * stride + x * 2;
                    let e = row * stride + x1 * 2;
                    for b in &mut buf[s..e] {
                        *b = !*b;
                    }
                }
                PixFmt::Rgb32 => {
                    for col in x..x1 {
                        let i = row * stride + col * 4;
                        buf[i] = !buf[i];
                        buf[i + 1] = !buf[i + 1];
                        buf[i + 2] = !buf[i + 2];
                    }
                }
            }
        }
    }

    #[inline]
    fn bpp(&self) -> usize {
        match self.fmt {
            PixFmt::Rgb565 => 2,
            PixFmt::Rgb32 => 4,
        }
    }

    /// Snapshot a rect's raw bytes (for save-under panels).
    pub fn copy_rect(&self, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
        let (x1, y1) = ((x + w).min(self.w), (y + h).min(self.h));
        let bpp = self.bpp();
        let b = self.buf_ref();
        let mut out = Vec::with_capacity((x1 - x) * (y1 - y) * bpp);
        for row in y..y1 {
            let s = row * self.stride + x * bpp;
            out.extend_from_slice(&b[s..s + (x1 - x) * bpp]);
        }
        out
    }

    /// Put back bytes captured by `copy_rect` with the same geometry.
    pub fn paste_rect(&mut self, x: usize, y: usize, w: usize, h: usize, data: &[u8]) {
        let (x1, y1) = ((x + w).min(self.w), (y + h).min(self.h));
        let (bpp, stride) = (self.bpp(), self.stride);
        let row_len = (x1 - x) * bpp;
        let b = self.buf();
        for (i, row) in (y..y1).enumerate() {
            let s = row * stride + x * bpp;
            b[s..s + row_len].copy_from_slice(&data[i * row_len..(i + 1) * row_len]);
        }
    }

    pub fn stamp(&mut self, cx: i32, cy: i32, r: i32, c: u16) {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    self.put_px(cx + dx, cy + dy, c);
                }
            }
        }
    }

    pub fn brush_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, r: i32, c: u16) {
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let steps = dx.max(dy).max(1);
        for i in 0..=steps {
            let x = x0 + (x1 - x0) * i / steps;
            let y = y0 + (y1 - y0) * i / steps;
            self.stamp(x, y, r, c);
        }
    }
}
