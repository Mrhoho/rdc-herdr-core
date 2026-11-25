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

fn begin(reason: &str, generation: u64) -> Value {
    json!({"type": "paint_begin", "reason": reason, "paintGen": generation, "cols": 80, "rows": 24})
}

fn end(generation: u64) -> Value {
    json!({"type": "paint_end", "paintGen": generation})
}

/// What the viewer is sent, marks as JSON and bytes as text.
fn seen(f: &Attached) -> Vec<Value> {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    f.sent()
        .into_iter()
        .map(|(kind, bytes)| match kind {
            wire::MARK => serde_json::from_slice(&bytes).unwrap(),
            wire::DATA => text(&bytes).into(),
            _ => json!({"kind": kind, "payload": text(&bytes)}),
        })
        .collect()
}

/// A paint from the stream `there` on `line`.
fn paint(f: &Attached, line: &Line, there: u32, reason: &str, generation: u64, bytes: &[u8]) {
    relay_out(&f.core, line, there, wire::MARK, &mark(begin(reason, generation)));
    relay_out(&f.core, line, there, wire::DATA, bytes);
    relay_out(&f.core, line, there, wire::MARK, &mark(end(generation)));
}

/// The marked viewer reattached over a new line up to its reply, whose stream there is `there`.
async fn back(f: &Attached, there: u32) -> (tokio::task::JoinHandle<()>, Arc<Line>, Frames, Value) {
    let (line, mut rx) = f.up_with(json!({"features": ["paint-marks"]}));
    let task = tokio::spawn(reattach(f.core.clone(), f.machine.clone(), line.clone()));
    let request: Value = serde_json::from_slice(&rx.recv().await.unwrap()[wire::HEADER..]).unwrap();
    line.take_reply(json!({"id": request["id"], "result": {"channel": there}}));
    (task, line, rx, request["params"].clone())
}

#[tokio::test]
async fn a_marked_viewer_back_on_a_new_line_is_told_of_the_gap_then_stitched() {
    // The line's cleanup ran, or never did (its task was stopped): the reply tells it then.
    for cleaned in [true, false] {
        let mut f = Attached::start();
        let (old, mut old_rx) = f.up_with(json!({"features": ["paint-marks"]}));
        let params = json!({"terminal": "t1", "marks": true, "history": 50});
        f.attach(&old, &mut old_rx, params, json!({"channel": 9})).await.1.unwrap();
        paint(&f, &old, 9, "attach", 1, b"first");
        assert_eq!(seen(&f), [begin("attach", 1), "first".into(), end(1)]);
        relay_out(&f.core, &old, 9, wire::DATA, b"unread");
        let mut told = vec![json!({"type": "discontinuity", "droppedBytes": 6})];
        if cleaned {
            f.drop_line(&old);
            assert_eq!(seen(&f), told.split_off(0), "told while the line is down");
        } else {
            f.core.lock().lines.insert(f.machine.id.clone(), LineState::Connecting);
            old.close();
        }
        let (task, line, _rx, params) = back(&f, 17).await;
        assert_eq!((&params["history"], &params["resumed"]), (&json!(50), &json!(true)), "every paint holds it");
        // Read by the line right after the reply, before the reattach runs.
        paint(&f, &line, 17, "attach", 1, b"again");
        told.extend([begin("stitch", 2), "again".into(), end(2)]);
        assert_eq!(seen(&f), told, "cleaned: {cleaned}");
        task.await.unwrap();
        // Another line drop is another gap, told once.
        relay_out(&f.core, &line, 17, wire::DATA, b"live");
        f.drop_line(&line);
        let (task, line, _rx, _) = back(&f, 18).await;
        task.await.unwrap();
        paint(&f, &line, 18, "attach", 1, b"third");
        let gap = json!({"type": "discontinuity", "droppedBytes": 4});
        assert_eq!(seen(&f), [gap, begin("stitch", 3), "third".into(), end(3)]);
    }
}

#[tokio::test]
async fn a_line_dropped_before_the_first_paint_was_read_leaves_the_next_its_attach() {
    let (f, old, _old_rx) = marked(9).await;
    relay_out(&f.core, &old, 9, wire::MARK, &mark(begin("attach", 1)));
    relay_out(&f.core, &old, 9, wire::DATA, b"unread");
    f.drop_line(&old);
    let (task, line, _rx, _) = back(&f, 17).await;
    paint(&f, &line, 17, "attach", 1, b"paint");
    task.await.unwrap();
    assert_eq!(seen(&f), [begin("attach", 2), "paint".into(), end(2)], "no gap in nothing painted");
}

#[tokio::test]
async fn a_gap_there_drops_what_the_viewer_has_not_read_and_gives_its_credit_back() {
    let (f, line, mut rx) = marked(9).await;
    paint(&f, &line, 9, "attach", 1, b"first");
    seen(&f);
    relay_out(&f.core, &line, 9, wire::DATA, b"live");
    paint(&f, &line, 9, "resync", 2, b"p2");
    credits(&mut rx);
    relay_out(&f.core, &line, 9, wire::MARK, &mark(json!({"type": "discontinuity", "droppedBytes": 3})));
    assert_eq!(credits(&mut rx), [(9, 6)], "the viewer will never read them");
    paint(&f, &line, 9, "resync", 3, b"p3");
    let gap = json!({"type": "discontinuity", "droppedBytes": 9});
    assert_eq!(seen(&f), [gap, begin("resync", 3), "p3".into(), end(3)]);
    // Nothing painted left to cut short: no gap, and the next paint is the viewer's attach.
    let (f, line, _rx) = marked(9).await;
    paint(&f, &line, 9, "attach", 1, b"first");
    relay_out(&f.core, &line, 9, wire::MARK, &mark(json!({"type": "discontinuity", "droppedBytes": 3})));
    paint(&f, &line, 9, "resync", 2, b"p2");
    assert_eq!(seen(&f), [begin("attach", 2), "p2".into(), end(2)]);
}

#[tokio::test]
async fn a_viewer_that_does_not_read_holds_one_paint_however_often_the_core_there_resets() {
    let (f, line, _rx) = marked(9).await;
    paint(&f, &line, 9, "attach", 1, b"first");
    seen(&f);
    for generation in 2..2 + 2 * wire::MAX_MARKS as u64 {
        relay_out(&f.core, &line, 9, wire::MARK, &mark(json!({"type": "discontinuity", "droppedBytes": 0})));
        paint(&f, &line, 9, "resync", generation, &[b'x'; 1000]);
    }
    let last = 1 + 2 * wire::MAX_MARKS as u64;
    let sent = seen(&f);
    assert_eq!(sent.len(), 4, "still open, with one paint: {:?}", &sent[..4.min(sent.len())]);
    assert_eq!((&sent[0]["type"], &sent[1], &sent[3]), (&json!("discontinuity"), &begin("resync", last), &end(last)));
}

#[tokio::test]
async fn an_unmarked_viewer_keeps_what_it_had_and_is_told_in_words() {
    let mut f = Attached::start();
    let (old, mut old_rx) = f.up();
    f.attach(&old, &mut old_rx, json!({"terminal": "t1", "history": 50}), json!({"channel": 9})).await.1.unwrap();
    relay_out(&f.core, &old, 9, wire::DATA, b"unread");
    relay_out(&f.core, &old, 9, wire::RESIZE, &[90, 0, 30, 0]);
    relay_out(&f.core, &old, 9, wire::DATA, b"paint");
    f.drop_line(&old);
    let (task, _line, _rx, params) = back(&f, 17).await;
    task.await.unwrap();
    assert_eq!((&params["history"], &params["resumed"]), (&json!(0), &json!(true)), "it has its history");
    let sent = f.sent();
    let kinds: Vec<u8> = sent.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, [wire::DATA, wire::RESIZE, wire::DATA]);
    assert_eq!(sent[2].1, [&b"paint"[..], lost_notice("away").as_bytes()].concat());
}
