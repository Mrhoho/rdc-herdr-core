//! Marked viewers here on terminals there.

use super::*;

fn mark(m: Value) -> Vec<u8> {
    m.to_string().into_bytes()
}

/// A marked viewer on `away/t1` over a line whose core there paints marks.
async fn marked(there: u32) -> (Attached, Arc<Line>, Frames) {
    let mut f = Attached::start();
    let (line, mut rx) = f.up_with(json!({"features": ["paint-marks"]}));
    let params = json!({"terminal": "t1", "marks": true});
    f.attach(&line, &mut rx, params, json!({"channel": there})).await.1.unwrap();
    (f, line, rx)
}

/// Whether the stream there was told to close.
fn closed_there(rx: &mut Frames, there: u32) -> bool {
    std::iter::from_fn(|| rx.try_recv().ok())
        .any(|f| wire::decode_header(f[..wire::HEADER].try_into().unwrap()).unwrap() == (there, wire::CLOSE, 0))
}

#[tokio::test]
async fn a_marked_attach_is_refused_before_anything_is_asked_where_marks_are_unknown() {
    let f = Attached::start();
    let (_line, mut rx) = f.up();
    let e = attach(&f.core, &f.conn, &f.machine, json!({"terminal": "t1", "marks": true})).await.unwrap_err();
    assert_eq!(e.kind, "marks_local_only");
    assert!(rx.try_recv().is_err(), "nothing was asked there");
    assert!(f.core.lock().relayed.is_empty());
    assert!(f.conn.outbox().next_frame().is_none(), "no stream was opened here");
}

#[tokio::test]
async fn a_marked_stream_gets_its_marks_in_order_and_no_grids() {
    let mut f = Attached::start();
    let (line, mut rx) = f.up_with(json!({"features": ["paint-marks", "resize_notifications"]}));
    let params = json!({"terminal": "t1", "marks": true, "observe": true, "resize_notifications": true});
    let (request, reply) = f.attach(&line, &mut rx, params, json!({"channel": 9, "resize_notifications": true})).await;
    assert_eq!(request["params"]["resize_notifications"], false, "{request}");
    assert_eq!(reply.unwrap()["resize_notifications"], false);
    let begin = json!({"type": "paint_begin", "reason": "attach", "paintGen": 1, "cols": 80, "rows": 24});
    let end = json!({"type": "paint_end", "paintGen": 1});
    relay_out(&f.core, &line, 9, wire::MARK, &mark(json!({"type": "discontinuity", "droppedBytes": 0})));
    relay_out(&f.core, &line, 9, wire::MARK, &mark(begin.clone()));
    relay_out(&f.core, &line, 9, wire::DATA, b"paint");
    relay_out(&f.core, &line, 9, wire::RESIZE, &[90, 0, 30, 0]);
    relay_out(&f.core, &line, 9, wire::MARK, &mark(end.clone()));
    let sent = f.sent();
    assert_eq!(sent.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(), [wire::MARK, wire::DATA, wire::MARK]);
    let json = |bytes: &[u8]| serde_json::from_slice::<Value>(bytes).unwrap();
    assert_eq!((json(&sent[0].1), &sent[1].1[..], json(&sent[2].1)), (begin, &b"paint"[..], end));
    // An unmarked viewer never asked for them.
    let (f, line, _rx) = Attached::new(5).await;
    relay_out(&f.core, &line, 5, wire::MARK, &mark(json!({"type": "paint_end", "paintGen": 1})));
    assert_eq!(f.sent(), []);
}

#[tokio::test]
async fn a_marked_stream_too_far_behind_or_with_a_mark_it_cant_follow_ends_here_and_there() {
    let too_many = (0..=wire::MAX_MARKS).map(|_| (wire::MARK, mark(json!({"type": "later_kind"})))).collect();
    let behind = vec![(wire::DATA, vec![b'x'; wire::MAX_PENDING + 1])];
    let wrong = vec![(wire::MARK, br#"{"type":"paint_end"}"#.to_vec())];
    for (frames, reason) in [(behind, wire::PAINT_LIMIT), (too_many, wire::PAINT_LIMIT), (wrong, b"")] {
        let (f, line, mut rx) = marked(9).await;
        for (kind, payload) in frames {
            relay_out(&f.core, &line, 9, kind, &payload);
        }
        let close = f.conn.outbox().next_frame().unwrap();
        assert_eq!((close[4], &close[wire::HEADER..]), (wire::CLOSE, reason));
        assert!(f.core.lock().relayed.is_empty());
        assert!(!lock(&line.streams).contains_key(&9));
        assert!(closed_there(&mut rx, 9));
    }
}

#[tokio::test]
async fn a_marked_viewer_ends_rather_than_reattach_where_marks_are_unknown() {
    let (f, old, _old_rx) = marked(9).await;
    f.drop_line(&old);
    let (line, mut rx) = f.up();
    reattach(f.core.clone(), f.machine.clone(), line.clone()).await;
    assert!(rx.try_recv().is_err(), "nothing was asked there");
    assert!(f.core.lock().relayed.is_empty());
    assert_eq!(f.sent().last().unwrap().0, wire::CLOSE);
}
