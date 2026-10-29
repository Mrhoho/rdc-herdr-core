use super::*;
use crate::events::{Filter, Sink};
use crate::paths::Paths;
use std::sync::atomic::AtomicBool;

#[test]
fn a_remote_missed_invalidates_unfiltered_document_readers_with_the_local_sequence() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-doc-missed-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "rohan".into(),
        target: "rohan".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "rohan".into(),
        version: "test".into(),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let (filtered_tx, mut filtered_rx) = mpsc::channel(4);
    {
        let mut st = core.lock();
        st.events.seq = 7;
        st.machines.push(machine.clone());
        core.subscribe(&mut st, Filter::default(), Sink::Plain { tx, behind: Arc::new(AtomicBool::new(false)) });
        core.subscribe(
            &mut st,
            Filter { terminal: Some("t1".into()), ..Default::default() },
            Sink::Plain { tx: filtered_tx, behind: Arc::new(AtomicBool::new(false)) },
        );
    }
    let (frames, _rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    mirror_events(&core, &machine, &line, b"{\"method\":\"missed\",\"params\":{\"seq\":9999,\"terminals\":[]}}\n");
    let msg: Value = serde_json::from_str(&rx.try_recv().expect("local missed notification")).unwrap();
    assert_eq!(msg["method"], "missed");
    assert_eq!(msg["params"]["seq"], 7);
    assert!(filtered_rx.try_recv().is_err());
    core.lock().machines.clear();
    mirror_event(&core, &machine, json!({"type": "doc", "data": {"name": "app", "version": 1}}));
    assert!(rx.try_recv().is_err(), "removed machines must not emit doc events");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn late_remote_attach_and_reattach_close_the_remote_stream() {
    for resuming in [false, true] {
        let root = std::env::temp_dir().join(format!("hc-relay-{}", crate::platform::random_hex()));
        let paths = Paths {
            socket: root.join("c.sock"),
            holder_socket: root.join("h.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
        let machine = Machine {
            name: "there".into(),
            target: "unused".into(),
            via: false,
            temporary: true,
            session: None,
            id: "there".into(),
            host: "there".into(),
            version: "test".into(),
        };
        let conn = Arc::new(Conn::default());
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({}));
        core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone()));
        let params = json!({"terminal":"t1", "cols":43, "rows":11});
        if resuming {
            let (frames, _old_rx) = mpsc::unbounded_channel();
            let old = Line::new(frames, &json!({}));
            core.lock().relayed.insert(
                (conn_key(&conn), 1),
                Relayed {
                    line: old,
                    there: 7,
                    conn: conn.clone(),
                    machine: machine.id.clone(),
                    params: params.clone(),
                },
            );
        }
        let (c, viewer, m, l) = (core.clone(), conn.clone(), machine.clone(), line.clone());
        let task = tokio::spawn(async move {
            if resuming {
                reattach(c, m, l).await;
            } else {
                assert_eq!(attach(&c, &viewer, &m, params).await.unwrap_err().kind, "connection_closed");
            }
        });
        let request = rx.recv().await.unwrap(); // handler is now suspended, awaiting the peer
        let req: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        core.detach(&conn, None);
        conn_gone(&core, &conn);
        line.take_reply(json!({"id":req["id"], "result":{"channel":9}}));
        task.await.unwrap();
        assert!(core.lock().relayed.is_empty());
        assert!(lock(&line.streams).is_empty());
        let close = rx.recv().await.unwrap();
        assert_eq!(wire::decode_header(close[..wire::HEADER].try_into().unwrap()).unwrap(), (9, wire::CLOSE, 0));
        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn abandoned_remote_reply_never_retains_a_sink() {
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    // Timeout already removed the pending call, but the peer accepted attach.
    line.take_reply(json!({"id":10, "result":{"channel":7}}));
    assert!(lock(&line.streams).is_empty());
    assert_eq!(
        wire::decode_header(rx.try_recv().unwrap()[..wire::HEADER].try_into().unwrap()).unwrap(),
        (7, wire::CLOSE, 0)
    );
    // Reply arrived just as the waiter was dropped.
    let (tx, waiter) = oneshot::channel();
    drop(waiter);
    lock(&line.pending).insert(11, (tx, Some((Arc::new(Conn::default()), 1))));
    line.take_reply(json!({"id":11, "result":{"channel":8}}));
    assert!(lock(&line.streams).is_empty());
    assert_eq!(
        wire::decode_header(rx.try_recv().unwrap()[..wire::HEADER].try_into().unwrap()).unwrap(),
        (8, wire::CLOSE, 0)
    );
}

#[test]
fn duplicate_remote_reply_does_not_close_an_active_stream() {
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let (tx, _waiter) = oneshot::channel();
    lock(&line.pending).insert(1, (tx, Some((Arc::new(Conn::default()), 2))));
    let reply = json!({"id":1, "result":{"channel":7}});
    line.take_reply(reply.clone());
    line.take_reply(reply);
    assert!(lock(&line.streams).contains_key(&7));
    lock(&line.mirror).1 = Some(8);
    line.take_reply(json!({"id":2, "result":{"channel":8}}));
    assert!(rx.try_recv().is_err());
}
