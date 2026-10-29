use super::*;
fn ask(t: &mut Terminal, q: &[u8]) -> String {
    t.write(q);
    String::from_utf8(t.take_responses()).unwrap()
}
#[test]
fn display_palette_keeps_dynamic_overrides_including_equal_to_default_and_resets() {
    let mut t = Terminal::new(20, 4, 0);
    t.set_palette(Some(&[(1, [1, 2, 3]), (255, [4, 5, 6])])).unwrap();
    assert_eq!(
        ask(&mut t, b"\x1b]4;1;?;255;?\x07"),
        "\x1b]4;1;rgb:0101/0202/0303\x07\x1b]4;255;rgb:0404/0505/0606\x07"
    );
    t.write(b"\x1b]4;1;#010203\x07\x1b]10;#010203\x07");
    t.set_palette(Some(&[(1, [9, 8, 7])])).unwrap();
    assert!(ask(&mut t, b"\x1b]4;1;?\x07").contains("0101/0202/0303"));
    let snap = t.snapshot().unwrap();
    let mut frozen = Terminal::restore(&snap, 0).unwrap();
    let mut replay = Vec::new();
    frozen.paint_with_colors_to(0, &mut replay).unwrap();
    let mut v = Terminal::new(20, 4, 0);
    v.set_palette(Some(&[(1, [9, 8, 7])])).unwrap();
    v.write(&replay);
    v.set_palette(Some(&[(1, [12, 13, 14])])).unwrap();
    assert!(ask(&mut v, b"\x1b]4;1;?\x07").contains("0101/0202/0303"));
    assert!(ask(&mut v, b"\x1b]104;1\x07\x1b]4;1;?\x07").contains("0c0c/0d0d/0e0e"));
    t.write(b"\x1b]104\x07\x1b]110\x07");
    assert_eq!(t.colors().palette, t.default_colors().palette);
}
#[test]
fn color_defaults_survive_snapshot_and_dark_is_an_explicit_override() {
    let mut t = Terminal::new(20, 4, 0);
    t.set_display(Some([1, 2, 3]), Some([255, 255, 255]), Some([7, 8, 9]), None);
    t.set_dark(Some(true));
    assert_eq!(ask(&mut t, b"\x1b[?996n"), "\x1b[?997;1n");
    t.set_dark(None);
    assert_eq!(ask(&mut t, b"\x1b[?996n"), "\x1b[?997;2n");
    t.write(b"\x1b]10;#112233\x07\x1b]11;#223344\x07\x1b]12;#334455\x07");
    let snap = t.snapshot().unwrap();
    let mut restored = Terminal::restore(&snap, 0).unwrap();
    assert_eq!(restored.default_colors(), t.default_colors());
    let mut bytes = Vec::new();
    restored.paint_with_colors_to(0, &mut bytes).unwrap();
    let mut v = Terminal::new(20, 4, 0);
    v.set_display(Some([1, 2, 3]), Some([255, 255, 255]), Some([7, 8, 9]), None);
    v.write(&bytes);
    assert_eq!(v.colors(), t.colors());
    assert_eq!(
        ask(&mut v, b"\x1b]110\x07\x1b]111\x07\x1b]112\x07\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07"),
        ask(&mut restored, b"\x1b]110\x07\x1b]111\x07\x1b]112\x07\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07")
    );
}
