use super::*;
use crate::paint::{Mark, Reason};
use ghostty_vt::budget::Budget;

#[test]
fn marks_cannot_overtake_data_or_be_left_behind_by_overflow() {
    let mut out = Outbox::default();
    out.open_viewer(1, true, Budget::new(16 << 20));
    assert!(out.data(1, &vec![b'x'; WINDOW + 1]));
    out.viewer(1).unwrap().mark(Mark::PaintEnd { generation: 1 }).unwrap();
    while let Some(f) = out.next_frame() {
        assert_eq!(f[4], DATA);
    }
    assert!(!out.data(1, &vec![b'x'; MAX_PENDING]));
    assert!(out.next_frame().is_none(), "overflow discarded the end mark too");
    out.viewer(1).unwrap().mark(Mark::Discontinuity { dropped: 1 }).unwrap();
    out.viewer(1)
        .unwrap()
        .mark(Mark::PaintBegin { reason: Reason::Resync, generation: 2, cols: 80, rows: 24, colors: None })
        .unwrap();
    assert_eq!(out.next_frame().unwrap()[4], MARK, "marks need no byte credit");
    assert_eq!(out.next_frame().unwrap()[4], MARK);
    out.viewer(1).unwrap().needs = false;
    assert!(out.data(1, b"y"));
    out.viewer(1).unwrap().mark(Mark::PaintEnd { generation: 2 }).unwrap();
    assert!(out.next_frame().is_none());
    out.credit(1, 1);
    assert_eq!(out.next_frame().unwrap()[4], DATA);
    assert_eq!(out.next_frame().unwrap()[4], MARK);
}

#[test]
fn shared_budget_covers_tiny_items_and_releases_on_detach() {
    let budget = Budget::new(1 << 20);
    let mut out = Outbox::default();
    out.open_viewer(1, true, budget.clone());
    for _ in 0..10_000 {
        if !out.data(1, b"x") {
            break;
        }
    }
    assert!(budget.peak() <= budget.limit());
    out.close(1, b"{}");
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn transport_splitting_and_coalescing_preserves_mark_data_order() {
    use tokio::io::AsyncWriteExt;
    let begin = Mark::PaintBegin { reason: Reason::Attach, generation: 1, cols: 80, rows: 24, colors: None }.bytes();
    let end = Mark::PaintEnd { generation: 1 }.bytes();
    let frames =
        [encode(3, MARK, &begin), encode(3, DATA, b"\x1b["), encode(3, DATA, b"31mRED"), encode(3, MARK, &end)];
    let bytes = frames.concat();
    for width in [1, 7, 65536] {
        let (mut sender, mut receiver) = tokio::io::duplex(width);
        let bytes = bytes.clone();
        let task = tokio::spawn(async move {
            for chunk in bytes.chunks(width) {
                sender.write_all(chunk).await.unwrap();
            }
        });
        for expected in &frames {
            let (ch, kind, data) = read_frame(&mut receiver).await.unwrap().unwrap();
            assert_eq!(encode(ch, kind, &data), *expected);
        }
        assert!(read_frame(&mut receiver).await.unwrap().is_none());
        task.await.unwrap();
    }
}

#[test]
fn a_second_overflow_aborts_the_repaint_and_counts_its_discarded_data() {
    use std::sync::Arc;
    let conn = Arc::new(Conn::default());
    let budget = Budget::new(32 << 20);
    let metrics = Arc::new(crate::paint::Metrics::default());
    let mut st = crate::runtime::State::default();
    let id = st
        .reg
        .create(
            crate::terminals::NewTerminal { argv: vec!["test".into()], cols: 80, rows: 24, ..Default::default() },
            0,
        )
        .unwrap();
    let mut screen = ghostty_vt::Terminal::new(80, 24, 0);
    screen.write(b"BASE");
    st.screens.insert(id, screen);
    let source = || crate::paint::Source::capture(&st, id, 0, true, &budget, &metrics).unwrap();
    let mut out = Outbox::default();
    out.open_viewer(1, true, budget.clone());
    out.streams.get_mut(&1).unwrap().credit = 0;
    assert!(!out.data(1, &vec![b'x'; MAX_PENDING + 1]));
    out.enqueue_paint(1, source(), Arc::downgrade(&conn), Reason::Resync, false, 0);
    let mark = |out: &mut Outbox| -> serde_json::Value {
        let f = out.next_frame().unwrap();
        assert_eq!(f[4], MARK);
        serde_json::from_slice(&f[HEADER..]).unwrap()
    };
    assert_eq!(mark(&mut out)["type"], "discontinuity");
    assert_eq!(mark(&mut out)["paintGen"], 1);
    // The first recovery transaction is incomplete, and another actual queue
    // overflow must discard its end and start generation 2, even with no credit.
    assert!(!out.data(1, &vec![b'y'; MAX_PENDING + 1]));
    out.enqueue_paint(1, source(), Arc::downgrade(&conn), Reason::Resync, false, 0);
    let d = mark(&mut out);
    assert_eq!(d["droppedBytes"], MAX_PENDING + 1);
    assert_eq!(mark(&mut out)["paintGen"], 2);
    assert!(out.next_frame().is_none());
    // This protected recovery records one deferred request, with no tail
    // allocation and no additional abort, despite repeated resync pressure.
    assert!(out.data(1, &vec![b'z'; MAX_PENDING + 1]));
    assert!(out.data(1, b"last"));
    assert!(out.next_frame().is_none());
    out.credit(1, WINDOW);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(f) = out.next_frame() {
            if f[4] == MARK {
                let end: serde_json::Value = serde_json::from_slice(&f[HEADER..]).unwrap();
                assert_eq!(end["type"], "paint_end");
                assert_eq!(end["paintGen"], 2);
                break;
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(std::time::Instant::now() < until);
    }
    assert!(out.paint_ready(1));
    out.close(1, b"{}");
    while budget.used() != 0 && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(budget.used(), 0);
}
