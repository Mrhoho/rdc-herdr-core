//! Streaming paint over a frozen terminal. Formatting may block in Write;
//! callers must use an owned frozen terminal, never hold the live core lock.
use super::*;
use std::io::{self, Write};
use std::sync::Arc;

fn callback<W: Write>(writer: &mut W, f: impl FnOnce(ffi::Writer) -> i32) -> io::Result<()> {
    struct State<'a, W> {
        writer: &'a mut W,
        error: Option<io::Error>,
    }
    unsafe extern "C" fn write<W: Write>(ud: *mut c_void, bytes: *const u8, len: usize) -> bool {
        let state = unsafe { &mut *ud.cast::<State<'_, W>>() };
        // No unwind is allowed through the C ABI.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.writer.write_all(unsafe { std::slice::from_raw_parts(bytes, len) })
        })) {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                state.error = Some(e);
                false
            }
            Err(_) => {
                state.error = Some(io::Error::other("paint_writer"));
                false
            }
        }
    }
    let mut state = State { writer, error: None };
    let rc = f(ffi::Writer { write: write::<W>, userdata: (&raw mut state).cast() });
    if rc == ffi::SUCCESS { Ok(()) } else { Err(state.error.unwrap_or_else(|| io::Error::other("paint_source"))) }
}

impl Terminal {
    pub fn snapshot_budget(&self, budget: &Arc<budget::Budget>) -> io::Result<budget::Bytes> {
        let mut bytes = budget::Bytes::new(budget);
        callback(&mut bytes, |writer| unsafe { ffi::ghostty_snapshot_encode(self.raw, writer) })?;
        Ok(bytes)
    }
    pub fn restore_budget(bytes: &[u8], budget: &Arc<budget::Budget>) -> Option<Self> {
        let allocator = budget::Allocator::new(budget);
        let mut d = ptr::null_mut();
        if unsafe { ffi::ghostty_snapshot_decoder_new_buf(allocator.as_ptr(), &mut d, bytes.as_ptr(), bytes.len()) }
            != ffi::SUCCESS
        {
            return None;
        }
        let retain = true;
        let maximum = CONTINUATION_MAX_BYTES;
        unsafe {
            ffi::ghostty_snapshot_decoder_set(d, 0, (&raw const maximum).cast());
            ffi::ghostty_snapshot_decoder_set(d, 1, (&raw const retain).cast());
            ffi::ghostty_snapshot_decoder_set(d, 2, (&raw const retain).cast());
        }
        let mut raw = ptr::null_mut();
        let rc = unsafe { ffi::ghostty_snapshot_decoder_decode(d, &mut raw) };
        unsafe { ffi::ghostty_snapshot_decoder_free(d) };
        if rc != ffi::SUCCESS || raw.is_null() {
            return None;
        }
        let mut term = Self::wrap(raw, budget.limit());
        term.allocator = Some(allocator);
        Some(term)
    }
    pub fn continuation_to(&self, writer: &mut impl Write) -> io::Result<()> {
        callback(writer, |writer| unsafe { ffi::ghostty_terminal_continuation_write(self.raw, writer) })
    }
    fn format_to(&self, selection: Option<&ffi::Selection>, extras: bool, writer: &mut impl Write) -> io::Result<()> {
        let emit = ffi::FORMAT_VT;
        let joined = false;
        let screen = ffi::ScreenExtra {
            size: size_of::<ffi::ScreenExtra>(),
            cursor: extras,
            style: extras,
            hyperlink: extras,
            protection: extras,
            kitty_keyboard: extras,
            charsets: extras,
        };
        let opts = ffi::FormatterOptions {
            size: size_of::<ffi::FormatterOptions>(),
            emit,
            unwrap: joined,
            trim: joined,
            extra: ffi::TerminalExtra {
                size: size_of::<ffi::TerminalExtra>(),
                // The full palette would paint over the viewer's own theme.
                palette: false,
                modes: extras,
                scrolling_region: extras,
                tabstops: extras,
                pwd: extras,
                keyboard: extras,
                screen,
            },
            selection: selection.map_or(ptr::null(), |s| s as *const _),
        };
        let allocator = self.allocator.as_ref().map_or(ptr::null(), |a| a.as_ptr());
        let mut f = ptr::null_mut();
        if unsafe { ffi::ghostty_formatter_terminal_new(allocator, &mut f, self.raw, opts) } != ffi::SUCCESS {
            return Err(io::Error::other("paint_limit"));
        }
        let result = callback(writer, |writer| unsafe { ffi::ghostty_formatter_format(f, writer) });
        unsafe { ffi::ghostty_formatter_free(f) };
        result
    }
    /// Same bytes as paint(), without allocating the complete VT string.
    /// Runtime metadata and continuation are deliberately separate: metadata
    /// must precede the unfinished parser suffix, never be inserted inside it.
    pub fn paint_to(&self, history: usize, writer: &mut impl Write) -> io::Result<()> {
        let (cols, rows) = self.size();
        let last = (cols.saturating_sub(1), rows.saturating_sub(1) as u32);
        writer.write_all(
            b"\x1b[!p\x1b[?2026l\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\
\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?1l\x1b[?66l\x1b[?2031l\x1b[?2048l\x1b[=0;1u\
\x1b[?1049l\x1b[?1047l\x1b[?47l\x1b[H\x1b[2J",
        )?;
        let available = self.history_rows();
        if history > 0 && available > 0 && !self.alt_screen() {
            let from = available.saturating_sub(history.min(u32::MAX as usize) as u32);
            if let Some(sel) = self.select(ffi::POINT_HISTORY, (0, from), (last.0, available - 1)) {
                self.format_to(Some(&sel), false, writer)?;
                writer.write_all(b"\x1b[0m\r\n")?;
                for _ in 0..rows {
                    writer.write_all(b"\n")?;
                }
                writer.write_all(b"\x1b[H\x1b[2J")?;
            }
        }
        if self.alt_screen() {
            writer.write_all(b"\x1b[?1049h")?;
        }
        if let Some(sel) = self.select(ffi::POINT_ACTIVE, (0, 0), last) {
            let mut filter = Strip { writer, held: Vec::new() };
            self.format_to(Some(&sel), true, &mut filter)?;
            filter.finish()?;
        }
        let mut title = ffi::Str { ptr: ptr::null(), len: 0 };
        if unsafe { ffi::ghostty_terminal_get(self.raw, ffi::DATA_TITLE, (&raw mut title).cast()) } == ffi::SUCCESS
            && !title.ptr.is_null()
            && title.len > 0
        {
            writer.write_all(b"\x1b]2;")?;
            writer.write_all(unsafe { std::slice::from_raw_parts(title.ptr, title.len) })?;
            writer.write_all(b"\x1b\\")?;
        }
        Ok(())
    }
}

/// Remove only these complete mode sequences, even across FFI callback boundaries.
struct Strip<'a, W> {
    writer: &'a mut W,
    held: Vec<u8>,
}
const STRIP: &[&[u8]] = &[b"\x1b[?2026h", b"\x1b[?47h", b"\x1b[?1047h", b"\x1b[?1049h"];
impl<W: Write> Strip<'_, W> {
    fn finish(&mut self) -> io::Result<()> {
        self.writer.write_all(&self.held)?;
        self.held.clear();
        Ok(())
    }
}
impl<W: Write> Write for Strip<'_, W> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len();
        while !bytes.is_empty() {
            if self.held.is_empty() {
                let count = bytes.iter().position(|b| *b == 0x1b).unwrap_or(bytes.len());
                self.writer.write_all(&bytes[..count])?;
                bytes = &bytes[count..];
                if bytes.is_empty() {
                    break;
                }
            }
            self.held.push(bytes[0]);
            bytes = &bytes[1..];
            if STRIP.contains(&self.held.as_slice()) {
                self.held.clear();
            } else if !STRIP.iter().any(|s| s.starts_with(&self.held)) {
                self.finish()?;
            }
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
