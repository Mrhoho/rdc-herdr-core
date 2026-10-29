//! Color defaults and program overrides remain distinct. Only the frozen paint
//! clone is probed for override bits; the live terminal is never modified by a paint.
use super::*;
use std::io::{self, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub palette: [Rgb; 256],
    pub dark: bool,
}
impl Terminal {
    fn rgb_value(&self, key: i32) -> Option<Rgb> {
        let mut rgb = [0; 3];
        (unsafe { ffi::ghostty_terminal_get(self.raw, key, rgb.as_mut_ptr().cast()) } == ffi::SUCCESS).then_some(rgb)
    }
    fn palette_value(&self, key: i32) -> [Rgb; 256] {
        let mut palette = [[0; 3]; 256];
        let rc = unsafe { ffi::ghostty_terminal_get(self.raw, key, palette.as_mut_ptr().cast()) };
        assert_eq!(rc, ffi::SUCCESS, "palette getter");
        palette
    }
    pub fn colors(&self) -> Colors {
        Colors {
            fg: self.rgb_value(ffi::DATA_COLOR_FOREGROUND),
            bg: self.rgb_value(ffi::DATA_COLOR_BACKGROUND),
            cursor: self.rgb_value(ffi::DATA_COLOR_CURSOR),
            palette: self.palette_value(ffi::DATA_COLOR_PALETTE),
            dark: self.hooks.dark_override.unwrap_or(self.hooks.dark),
        }
    }
    pub fn default_colors(&self) -> Colors {
        Colors {
            fg: self.rgb_value(ffi::DATA_COLOR_FOREGROUND_DEFAULT),
            bg: self.rgb_value(ffi::DATA_COLOR_BACKGROUND_DEFAULT),
            cursor: self.rgb_value(ffi::DATA_COLOR_CURSOR_DEFAULT),
            palette: self.palette_value(ffi::DATA_COLOR_PALETTE_DEFAULT),
            dark: self.hooks.dark_override.unwrap_or(self.hooks.dark),
        }
    }
    fn palette_set(&mut self, palette: Option<&[Rgb; 256]>) -> io::Result<()> {
        let rc = unsafe {
            ffi::ghostty_terminal_set(
                self.raw,
                ffi::OPT_COLOR_PALETTE,
                palette.map_or(ptr::null(), |p| p.as_ptr().cast()),
            )
        };
        if rc == ffi::SUCCESS { Ok(()) } else { Err(io::Error::other("color_palette")) }
    }
    /// None preserves the known palette; Some replaces the sparse theme over
    /// the built-in palette. Empty explicitly restores the built-in defaults.
    /// Ghostty preserves every OSC override, even one equal to its old default.
    pub fn set_palette(&mut self, palette: Option<&[(u8, Rgb)]>) -> io::Result<()> {
        let Some(entries) = palette else { return Ok(()) };
        self.palette_set(None)?;
        let mut base = self.palette_value(ffi::DATA_COLOR_PALETTE_DEFAULT);
        for &(i, c) in entries {
            base[i as usize] = c;
        }
        self.palette_set(Some(&base))
    }
    pub fn set_dark(&mut self, dark: Option<bool>) {
        self.hooks.dark_override = dark;
    }
    /// Apply a MARK's default color layer before parsing its VT paint. This
    /// does not promote defaults to OSC overrides, so later resets stay correct.
    pub fn set_color_defaults(&mut self, colors: &Colors) -> io::Result<()> {
        self.palette_set(Some(&colors.palette))?;
        for (opt, rgb) in [
            (ffi::OPT_COLOR_FOREGROUND, colors.fg),
            (ffi::OPT_COLOR_BACKGROUND, colors.bg),
            (ffi::OPT_COLOR_CURSOR, colors.cursor),
        ] {
            unsafe {
                ffi::ghostty_terminal_set(self.raw, opt, rgb.as_ref().map_or(ptr::null(), |c| c.as_ptr().cast()))
            };
        }
        self.hooks.dark = colors.dark;
        self.hooks.dark_override = None;
        Ok(())
    }
    /// Existing FFI exposes effective/default values but not override masks.
    /// On an owned frozen clone, temporarily choose defaults unequal to every
    /// effective value; unchanged effective values identify overrides exactly,
    /// including an override that originally equalled the default. Restore the
    /// original defaults before formatting. No snapshot-layout parsing needed.
    pub(crate) fn color_overrides(&mut self) -> io::Result<Overrides> {
        let defaults = self.default_colors();
        let current = self.colors();
        let mut probe = current.palette;
        for c in &mut probe {
            c[0] ^= 1;
        }
        self.palette_set(Some(&probe))?;
        let probed = self.palette_value(ffi::DATA_COLOR_PALETTE);
        self.palette_set(Some(&defaults.palette))?;
        let mut palette = [None; 256];
        for i in 0..256 {
            if probed[i] == current.palette[i] {
                palette[i] = Some(current.palette[i]);
            }
        }
        let mut dynamic = [None; 3];
        for (i, (opt, key, default, effective)) in [
            (ffi::OPT_COLOR_FOREGROUND, ffi::DATA_COLOR_FOREGROUND, defaults.fg, current.fg),
            (ffi::OPT_COLOR_BACKGROUND, ffi::DATA_COLOR_BACKGROUND, defaults.bg, current.bg),
            (ffi::OPT_COLOR_CURSOR, ffi::DATA_COLOR_CURSOR, defaults.cursor, current.cursor),
        ]
        .into_iter()
        .enumerate()
        {
            let mut probe = effective.unwrap_or([0; 3]);
            probe[0] ^= 1;
            unsafe { ffi::ghostty_terminal_set(self.raw, opt, probe.as_ptr().cast()) };
            let value = self.rgb_value(key);
            unsafe {
                ffi::ghostty_terminal_set(self.raw, opt, default.as_ref().map_or(ptr::null(), |c| c.as_ptr().cast()))
            };
            if value == effective {
                dynamic[i] = effective;
            }
        }
        Ok(Overrides { palette, dynamic })
    }
}
pub(crate) struct Overrides {
    palette: [Option<Rgb>; 256],
    dynamic: [Option<Rgb>; 3],
}
impl Overrides {
    pub(crate) fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        // DECSTR in the paint prefix resets palette overrides, but not 10-12.
        writer.write_all(b"\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\")?;
        for (i, rgb) in self.palette.iter().enumerate() {
            if let Some([r, g, b]) = rgb {
                write!(writer, "\x1b]4;{i};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\")?;
            }
        }
        for (i, rgb) in self.dynamic.iter().enumerate() {
            if let Some([r, g, b]) = rgb {
                write!(writer, "\x1b]{};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\", i + 10)?;
            }
        }
        Ok(())
    }
}

/// Same color grammar as OSC setters (including X11 names).
pub fn parse_color(bytes: &[u8]) -> Option<Rgb> {
    unsafe extern "C" {
        fn ghostty_color_parse(data: *const u8, len: usize, out: *mut u8) -> i32;
    }
    let mut rgb = [0; 3];
    (unsafe { ghostty_color_parse(bytes.as_ptr(), bytes.len(), rgb.as_mut_ptr()) } == ffi::SUCCESS).then_some(rgb)
}
