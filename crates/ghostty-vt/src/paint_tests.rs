use super::*;
use std::io::Write;
#[test]
fn streaming_paint_matches_legacy_bytes_and_frozen_state() {
    let mut screen = Terminal::new(12, 4, 1 << 20);
    screen.write(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n\x1b[31mred\x1b]2;title\x07");
    for alt in [false, true] {
        if alt {
            screen.write(b"\x1b[?1049h\x1b[?1000hALT");
        }
        let want = screen.paint(100);
        let budget = budget::Budget::new(32 << 20);
        let snapshot = screen.snapshot_budget(&budget).unwrap();
        let frozen = Terminal::restore_budget(snapshot.as_slice(), &budget).unwrap();
        let mut actual = Vec::new();
        frozen.paint_to(100, &mut actual).unwrap();
        assert_eq!(actual, want);
        drop(frozen);
        drop(snapshot);
        assert_eq!(budget.used(), 0);
    }
}

#[test]
fn colored_alternate_paint_preserves_primary_history_and_program_overrides() {
    let mut source = Terminal::new(12, 4, 1 << 20);
    source.set_palette(Some(&[(1, [1, 2, 3])])).unwrap();
    source.write(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n\x1b]4;1;#123456\x07\x1b[31mPRIMARY");
    source.write(b"\x1b[?1049hALT");
    let budget = budget::Budget::new(32 << 20);
    let snapshot = source.snapshot_budget(&budget).unwrap();
    let mut frozen = Terminal::restore_budget(snapshot.as_slice(), &budget).unwrap();
    let mut bytes = Vec::new();
    frozen.paint_with_colors_to(100, &mut bytes).unwrap();
    let mut viewer = Terminal::new(12, 4, 1 << 20);
    viewer.set_color_defaults(&source.default_colors()).unwrap();
    viewer.write(&bytes);
    assert_eq!(viewer.screen_rows(), source.screen_rows());
    assert_eq!(viewer.colors(), source.colors());
    for terminal in [&mut source, &mut viewer] {
        terminal.write(b"\x1b[?1049l");
    }
    assert_eq!(viewer.screen_rows(), source.screen_rows());
    assert!(viewer.plain_text().starts_with("one\ntwo\n"), "hidden primary history is restored");
    assert_eq!(viewer.colors(), source.colors());
    drop(frozen);
    drop(snapshot);
    assert_eq!(budget.used(), 0);
}
#[test]
fn frozen_paint_can_restore_an_unfinished_utf8_or_escape_sequence() {
    for (prefix, suffix) in [
        (b"\xf0\x9f".as_slice(), b"\x98\x80".as_slice()),
        (b"\x1b]2;unfinished", b" title\x07Z"),
        (b"\x1b[38;2;1;", b"2;3mZ"),
    ] {
        let mut source = Terminal::new(20, 4, 0);
        source.write(b"BASE");
        source.write(prefix);
        let budget = budget::Budget::new(32 << 20);
        let snapshot = source.snapshot_budget(&budget).unwrap();
        let frozen = Terminal::restore_budget(snapshot.as_slice(), &budget).unwrap();
        let mut bytes = Vec::new();
        frozen.paint_to(0, &mut bytes).unwrap();
        frozen.continuation_to(&mut bytes).unwrap();
        let mut viewer = Terminal::new(20, 4, 0);
        viewer.write(&bytes);
        viewer.write(suffix);
        source.write(suffix);
        assert_eq!(viewer.screen_rows(), source.screen_rows());
        assert_eq!(viewer.title(), source.title());
    }
}
#[test]
fn snapshot_and_decoder_fail_closed_at_the_shared_budget() {
    let screen = Terminal::new(80, 24, 0);
    assert!(screen.snapshot_budget(&budget::Budget::new(1)).is_err());
    let snapshot = screen.snapshot().unwrap();
    let budget = budget::Budget::new(64);
    assert!(Terminal::restore_budget(&snapshot, &budget).is_none());
    assert_eq!(budget.used(), 0);
    struct Refuse;
    impl Write for Refuse {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("cancelled"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(screen.paint_to(0, &mut Refuse).is_err());
}
