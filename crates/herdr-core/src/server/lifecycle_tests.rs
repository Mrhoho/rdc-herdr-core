use super::*;
use serde_json::json;

fn fixture() -> (Arc<Core>, std::path::PathBuf, u32) {
    let root = std::path::PathBuf::from(format!("/var/tmp/hc-life-{}", platform::random_hex()));
    let paths = Paths {
        socket: root.join("c.sock"),
        holder_socket: root.join("h.sock"),
        state: root.join("state"),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let (cols, rows) = core.headless();
    let id = {
        let mut st = core.lock();
        let id = st
            .reg
            .create(crate::terminals::NewTerminal { argv: vec!["test".into()], cols, rows, ..Default::default() }, 0)
            .unwrap();
        st.screens.insert(id, ghostty_vt::Terminal::new(cols, rows, 1024));
        id
    };
    (core, root, id)
}

// A current-thread executor plus an always-ready byte slice makes the old
// probe deterministic: every RPC is queued, then EOF cleanup, then handlers.
#[tokio::test]
async fn queued_attach_and_subscribe_cannot_outlive_eof() {
    let (core, root, id) = fixture();
    let conn = Arc::new(Conn::default());
    let mut bytes = Vec::new();
    for (method, params) in [
        ("terminal.attach", json!({"terminal":"t1", "cols":43, "rows":11})),
        ("events.subscribe", json!({})),
        ("doc.set", json!({"name":"accepted", "version":0, "value":true})),
    ] {
        bytes.extend(wire::encode(
            0,
            wire::DATA,
            json!({"jsonrpc":"2.0", "id":method,
            "method":method, "params":params})
            .to_string()
            .as_bytes(),
        ));
    }
    read_frames(&core, &conn, &mut bytes.as_slice()).await.unwrap();
    core.detach(&conn, None);
    core.detach(&conn, None); // repeated cancellation is harmless
    tokio::task::yield_now().await;
    assert!(core.lock().viewers.is_empty(), "late attach resurrected a viewer");
    assert!(!core.subscribed(&core.lock(), &conn, 1));
    assert!(!core.subscribed(&core.lock(), &conn, 2));
    assert!(conn.outbox().next_frame().is_none(), "dead connection kept queued frames");
    assert_eq!(core.lock().docs["accepted"].version, 1, "accepted effects must still finish");
    let call = api::Call { core: &core, conn: Some(&conn), plain: None };
    for (method, params) in
        [("terminal.attach", json!({"terminal":"t1", "cols":81, "rows":25})), ("events.subscribe", json!({}))]
    {
        assert_eq!(api::dispatch(&call, method, params).await.unwrap_err().kind, "connection_closed");
    }
    // A subsequent healthy viewer must not uncover the abandoned 43 x 11 size.
    let healthy = Arc::new(Conn::default());
    let req = crate::viewers::AttachRequest {
        observe: false,
        history: 0,
        size: (81, 25),
        display: Default::default(),
        resumed: false,
    };
    core.attach(&healthy, id, req).unwrap();
    assert_eq!(core.lock().reg.terminals[&id].cols, 81);
    core.detach(&healthy, None);
    {
        let st = core.lock();
        assert_eq!((st.reg.terminals[&id].cols, st.reg.terminals[&id].rows), core.headless());
    }
    assert!(core.lock().reg.terminals[&id].is_live());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn registered_streams_and_unread_first_paint_are_reclaimed() {
    let (core, root, id) = fixture();
    let conn = Arc::new(Conn::default());
    let call = api::Call { core: &core, conn: Some(&conn), plain: None };
    let attach = api::dispatch(&call, "terminal.attach", json!({"terminal":"t1","cols":43,"rows":11})).await.unwrap();
    let sub = api::dispatch(&call, "events.subscribe", json!({})).await.unwrap();
    let channel = sub["channel"].as_u64().unwrap() as u32;
    assert!(core.subscribed(&core.lock(), &conn, channel));
    // Per-channel close does not revoke connection admission.
    core.detach(&conn, Some(attach["channel"].as_u64().unwrap() as u32));
    api::dispatch(&call, "terminal.attach", json!({"terminal":"t1", "cols":81, "rows":25})).await.unwrap();
    // No paint/reply has been read yet.
    core.detach(&conn, None);
    assert!(core.lock().viewers.is_empty());
    assert!(!core.subscribed(&core.lock(), &conn, channel));
    assert!(conn.outbox().next_frame().is_none());
    assert!(core.lock().reg.terminals[&id].is_live());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
