use super::*;
use crate::events::{Filter, Sink};
use crate::paths::Paths;
use std::sync::atomic::AtomicBool;

mod machines;
mod marks;

#[tokio::test]
async fn removing_or_replacing_a_machine_drops_its_status_poll() {
    for replacement in [false, true] {
        let root = std::path::PathBuf::from(format!("/var/tmp/hc-poll-{}", crate::platform::random_hex()));
        let paths = Paths {
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
        let machine: Machine = serde_json::from_value(json!({
            "name": "away", "target": "away", "id": "away", "host": "away",
            "version": "test", "session": null
        }))
        .unwrap();
        core.lock().machines.push(machine.clone());
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let dialed = crate::machines::Dialed {
            stdin: child.stdin.take().unwrap(),
            stdout: tokio::io::BufReader::new(child.stdout.take().unwrap()),
            stderr: child.stderr.take().unwrap(),
            child,
            hello: json!({}),
        };
        let owner = core.clone();
        let task = tokio::spawn(async move { hold(&owner, &machine, dialed).await });
        core.lock().line_tasks.insert("away".into(), task.abort_handle());
        let line = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(LineState::Connected(line)) = core.lock().lines.get("away")
                    && !lock(&line.pending).is_empty()
                {
                    break line.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let weak = Arc::downgrade(&line);
        drop(line);
        {
            let mut st = core.lock();
            forget(&core, &mut st, "away");
            if replacement {
                st.lines.insert("away".into(), LineState::Connecting);
            } else {
                st.machines.clear();
            }
        }
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("removed line is still retained by a detached poll");
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn a_send_on_an_already_closed_line_fails_without_waiting() {
    for writer_gone in [false, true] {
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({}));
        if writer_gone {
            rx.close();
        } else {
            line.close(); // The writer may not have finished shutting down yet.
        }
        let sent = tokio::time::timeout(
            Duration::from_millis(200),
            line.call("away", "terminal.send", json!({"terminal": "t1", "text": "hi"}), Duration::from_secs(30)),
        )
        .await
        .expect("a closed transport must not hold a send");
        assert!(sent.unwrap_err().message.contains("lost the line"));
        assert!(lock(&line.pending).is_empty());
    }
}

#[test]
fn automatic_input_requires_support_at_every_hop() {
    for features in [json!([]), json!(["automatic_input"])] {
        let (frames, _rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": features}));
        for reply in [json!({}), json!({"automatic_input": false}), json!({"automatic_input": true})] {
            assert_eq!(
                automatic_input(&line, &reply),
                !features.as_array().unwrap().is_empty() && reply["automatic_input"] == true
            );
        }
    }
}

#[tokio::test]
async fn automatic_input_survives_relays_but_not_a_capability_downgrade() {
    for (feature, confirmed) in [(false, true), (true, false), (true, true)] {
        let root = crate::test_tmp().join(format!(
            "hc-automatic-relay-{}-{}",
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
            name: "away".into(),
            target: "away".into(),
            via: false,
            temporary: false,
            session: None,
            id: "remote".into(),
            host: "away".into(),
            version: "test".into(),
            build: None,
            keeps_running_after_logout: None,
            logout_linger_user: None,
            features: Vec::new(),
        };
        let conn = Arc::new(Conn::default());
        conn.with(|out| out.open(7));
        let (old_frames, mut old_rx) = mpsc::unbounded_channel();
        core.lock().relayed.insert(
            (conn_key(&conn), 7),
            Relayed {
                active_at: std::time::Instant::now(),
                automatic_input: true,
                line: Line::new(old_frames, &json!({"features": ["automatic_input"]})),
                there: 9,
                conn: conn.clone(),
                machine: machine.id.clone(),
                terminal: "t1".into(),
                params: json!({"terminal": "t1"}),
                size_pending: false,
                relay: Default::default(),
            },
        );
        assert!(relay_in(&core, &conn, 7, wire::AUTOMATIC_INPUT, b"\x1b"));
        let frame = old_rx.recv().await.unwrap();
        assert_eq!(frame, wire::encode(9, wire::AUTOMATIC_INPUT, b"\x1b"));
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": if feature { vec!["automatic_input"] } else { vec![] }}));
        let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        line.take_reply(json!({"id": request["id"], "result": {"channel": 10, "automatic_input": confirmed}}));
        task.await.unwrap();
        if feature && confirmed {
            assert!(core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(relay_in(&core, &conn, 7, wire::AUTOMATIC_INPUT, b"[48;24;80t"));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::AUTOMATIC_INPUT, b"[48;24;80t"));
        } else {
            assert!(!core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(!lock(&line.streams).contains_key(&10));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::CLOSE, b""));
            let close = conn.outbox().next_frame().unwrap();
            assert_eq!(close[4], wire::CLOSE);
        }
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn reattach_applies_missed_sizes_without_replaying_delivered_activity() {
    let root = crate::test_tmp().join(format!("hc-reattach-{}", crate::platform::random_hex()));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine: Machine = serde_json::from_value(json!({
        "name": "away", "target": "away", "id": "away", "host": "away", "version": "test"
    }))
    .unwrap();
    let conn = Arc::new(Conn::default());
    let (old_frames, _old_rx) = mpsc::unbounded_channel();
    let old = Line::new(old_frames, &json!({}));
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let now = std::time::Instant::now();
    for (ch, active_at, target, via) in
        [(7, now - Duration::from_secs(1), "t1", &old), (8, now, "t1", &old), (9, now, "t2", &line)]
    {
        core.lock().relayed.insert(
            (conn_key(&conn), ch),
            Relayed {
                active_at,
                automatic_input: false,
                line: via.clone(),
                there: ch,
                conn: conn.clone(),
                machine: machine.id.clone(),
                terminal: target.into(),
                params: json!({"terminal": target, "cols": 80, "rows": 24}),
                size_pending: false,
                relay: Default::default(),
            },
        );
    }
    let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
    let frame = rx.recv().await.unwrap();
    let first: Value = serde_json::from_slice(&frame[wire::HEADER..]).unwrap();
    let size = [132, 0, 45, 0];
    assert!(relay_in(&core, &conn, 7, wire::RESIZE, &size));
    line.take_reply(json!({"id": first["id"], "result": {"channel": 17}}));
    assert_eq!(rx.recv().await.unwrap(), wire::encode(17, wire::RESIZE, &size));
    let frame = rx.recv().await.unwrap();
    let second: Value = serde_json::from_slice(&frame[wire::HEADER..]).unwrap();
    let missed = [90, 0, 30, 0];
    assert!(relay_in(&core, &conn, 8, wire::RESIZE, &missed));
    assert!(relay_in(&core, &conn, 7, wire::FOCUS, &[1]));
    assert!(relay_in(&core, &conn, 9, wire::RESIZE, &size));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(17, wire::FOCUS, &[1]));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(9, wire::RESIZE, &size));
    line.take_reply(json!({"id": second["id"], "result": {"channel": 18}}));
    task.await.unwrap();
    assert!(rx.try_recv().is_err(), "a newer viewer is already in control and B was never disconnected");
    {
        let st = core.lock();
        assert_eq!(st.relayed[&(conn_key(&conn), 7)].params["cols"], 132);
        assert_eq!(st.relayed[&(conn_key(&conn), 8)].params["cols"], 90);
    }
    assert!(relay_in(&core, &conn, 8, wire::FOCUS, &[1]));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(18, wire::RESIZE, &missed));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(18, wire::FOCUS, &[1]));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn reconnect_rejects_downgrade_before_mapping_fallback_data() {
    for stalled in [false, true] {
        // The immediate relay is capable, but its deeper path has downgraded.
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": ["resize_notifications"]}));
        let conn = Arc::new(Conn::default());
        conn.with(|out| {
            out.open(7);
            if stalled {
                out.data(7, &vec![120; wire::WINDOW]);
                while out.next_frame().is_some() {}
            }
        });
        let calling = line.clone();
        let sink = conn.clone();
        let task = tokio::spawn(async move {
            calling
                .call_into(
                    "relay",
                    "terminal.attach",
                    json!({"observe": true, "resumed": true, "resize_notifications": true}),
                    Some(((sink, 7, Default::default()), true)),
                    Duration::from_secs(1),
                )
                .await
        });
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        line.take_reply(json!({"id": request["id"], "result": {"channel": 9, "resize_notifications": false}}));
        // Model buffered DATA consumed before the awakened caller can run.
        if let Some((sink, here, _)) = lock(&line.streams).get(&9) {
            sink.with(|out| {
                out.data(*here, b"unsupported fallback paint");
            });
        }
        assert!(task.await.unwrap().is_err(), "downgraded reattach must be rejected");
        conn.with(|out| {
            assert_eq!(out.queued(7), 0, "rejected fallback DATA must not enter the downstream queue");
            out.close(7, b"");
            let frame = out.next_frame().expect("CLOSE must not wait for credit");
            assert_eq!(wire::decode_header(frame[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::CLOSE);
            assert!(out.next_frame().is_none());
        });
        assert!(!lock(&line.streams).contains_key(&9), "rejected stream must never be mapped");
    }
}

#[tokio::test]
async fn failed_reattach_drains_pending_data_before_close() {
    let root =
        crate::test_tmp().join(format!("hc-reattach-drain-{}-{}", std::process::id(), crate::platform::random_hex()));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "away".into(),
        target: "away".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "away".into(),
        version: "test".into(),
        build: None,
        keeps_running_after_logout: None,
        logout_linger_user: None,
        features: Vec::new(),
    };
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(7);
        assert!(out.data(7, &vec![b'x'; wire::WINDOW]));
        while out.next_frame().is_some() {}
        assert!(out.data(7, b"last output before the line dropped"));
        assert!(out.next_frame().is_none(), "pending output must wait for credit");
    });
    let (old_frames, old_rx) = mpsc::unbounded_channel();
    let old_line = Line::new(old_frames, &json!({}));
    core.lock().relayed.insert(
        (conn_key(&conn), 7),
        Relayed {
            active_at: std::time::Instant::now(),
            automatic_input: true,
            line: old_line,
            there: 9,
            conn: conn.clone(),
            machine: machine.id.clone(),
            terminal: "t1".into(),
            params: json!({"terminal": "t1"}),
            size_pending: false,
            relay: Default::default(),
        },
    );
    drop(old_rx); // The viewer survives its old line.
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
    let request = rx.recv().await.unwrap();
    let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
    assert_eq!(request["method"], "terminal.attach");
    line.take_reply(crate::rpc::response(
        request["id"].clone(),
        Err(RpcError::new("finished", "terminal finished", "")),
    ));
    task.await.unwrap();
    assert!(core.lock().relayed.is_empty());
    conn.with(|out| {
        out.credit(7, wire::WINDOW);
        let data = out.next_frame().expect("queued output before CLOSE");
        assert_eq!(wire::decode_header(data[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::DATA);
        assert_eq!(&data[wire::HEADER..], b"last output before the line dropped");
        let close = out.next_frame().expect("CLOSE after queued output");
        assert_eq!(wire::decode_header(close[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::CLOSE);
        assert!(out.next_frame().is_none());
    });
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_misses_and_renames_keep_subscribers_on_the_current_machine() {
    let root =
        crate::test_tmp().join(format!("hc-doc-missed-{}-{}", std::process::id(), crate::platform::random_hex()));
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
        build: None,
        keeps_running_after_logout: None,
        logout_linger_user: None,
        features: Vec::new(),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let (filtered_tx, mut filtered_rx) = mpsc::channel(4);
    {
        let mut st = core.lock();
        st.machines.push(machine.clone());
        tell(&core, &mut st); // as when it was added
        st.events.seq = 7;
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
    assert_eq!(msg["params"]["documents"], true, "no terminal event was lost: {msg}");
    assert!(filtered_rx.try_recv().is_err());
    mirror_replace(&core, &machine, &line, vec![json!({"address":"t1", "state":"live", "status":{"state":"working"}})]);
    rx.try_recv().unwrap();
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(1);
        assert!(out.data_lines(1, &vec![b'\n'; wire::MAX_PENDING]));
    });
    {
        let mut st = core.lock();
        core.subscribe(
            &mut st,
            Filter { terminal: Some("renamed/t1".into()), ..Default::default() },
            Sink::Framed { conn: conn.clone(), channel: 1 },
        );
        st.machines[0].name = "renamed".into();
        rename_mirror(&core, &mut st, &machine.id, "rohan", "renamed");
    }
    let mut bytes = Vec::new();
    conn.with(|out| {
        while let Some(frame) = out.next_frame() {
            bytes.extend_from_slice(&frame[wire::HEADER..]);
        }
    });
    let missed = bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|line| line["method"] == "missed")
        .expect("backpressure snapshot");
    assert_eq!(missed["params"]["terminals"][0]["address"], "renamed/t1");
    assert!(missed["params"].get("machines").is_none(), "a filtered subscriber hears of no machines");
    let renamed: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(renamed["params"]["type"], "machine", "the machine first: {renamed}");
    assert_eq!(
        (&renamed["params"]["data"]["id"], &renamed["params"]["data"]["name"]),
        (&json!("remote"), &json!("renamed"))
    );
    let removed: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    let created: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(removed["params"]["terminal"], "rohan/t1");
    assert_eq!(removed["params"]["type"], "removed");
    assert_eq!(created["params"]["terminal"], "renamed/t1");
    assert_eq!(created["params"]["data"]["status"]["state"], "working");
    mirror_events(&core, &machine, &line, b"{\"method\":\"event\",\"params\":{\"type\":\"status\",\"terminal\":\"t1\",\"data\":{\"status\":{\"state\":\"blocked\"}}}}\n");
    let status: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(status["params"]["terminal"], "renamed/t1");
    assert_eq!(core.lock().mirrors[&machine.id]["renamed/t1"]["status"]["state"], "blocked");
    core.lock().machines.clear();
    mirror_event(&core, &machine, json!({"type": "doc", "data": {"name": "app", "version": 1}}));
    assert!(rx.try_recv().is_err(), "removed machines must not emit doc events");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_line_lost_notice_shows_even_if_the_line_dropped_mid_sequence() {
    let mut screen = ghostty_vt::Terminal::new(80, 24, 0);
    screen.write(b"\x1b_Gm=1;AAAA");
    screen.write(lost_notice("rohan").as_bytes());
    assert!(screen.plain_text().contains("[line to rohan lost"));
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
            build: None,
            keeps_running_after_logout: None,
            logout_linger_user: None,
            features: Vec::new(),
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
                    automatic_input: false,
                    line: old,
                    there: 7,
                    conn: conn.clone(),
                    machine: machine.id.clone(),
                    terminal: "t1".into(),
                    params: params.clone(),
                    active_at: std::time::Instant::now(),
                    size_pending: false,
                    relay: Default::default(),
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
    lock(&line.pending).insert(11, (tx, Some(((Arc::new(Conn::default()), 1, Default::default()), false))));
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
    lock(&line.pending).insert(1, (tx, Some(((Arc::new(Conn::default()), 2, Default::default()), false))));
    let reply = json!({"id":1, "result":{"channel":7}});
    line.take_reply(reply.clone());
    line.take_reply(reply);
    assert!(lock(&line.streams).contains_key(&7));
    lock(&line.mirror).1 = Some(8);
    line.take_reply(json!({"id":2, "result":{"channel":8}}));
    assert!(rx.try_recv().is_err());
}

/// A viewer here on `away/t1`, attached the way `attach` leaves it, and its machine's lines.
struct Attached {
    core: Arc<Core>,
    root: std::path::PathBuf,
    machine: Machine,
    conn: Arc<Conn>,
    here: u32,
}

type Frames = mpsc::UnboundedReceiver<Vec<u8>>;

impl Attached {
    /// Attached over a first line, whose stream there is `there`.
    async fn new(there: u32) -> (Attached, Arc<Line>, Frames) {
        let mut f = Attached::start();
        let (line, mut rx) = f.up();
        f.attach(&line, &mut rx, json!({"terminal": "t1"}), json!({"channel": there})).await.1.unwrap();
        (f, line, rx)
    }

    /// A core with `away` added and a viewer's connection, not attached yet.
    fn start() -> Attached {
        let root = crate::test_tmp().join(format!("hc-relay-{}-{}", std::process::id(), crate::platform::random_hex()));
        let paths = Paths {
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
        let machine: Machine = serde_json::from_value(json!({
            "name": "away", "target": "away", "id": "away", "host": "away", "version": "test"
        }))
        .unwrap();
        Attached { core, root, machine, conn: Arc::new(Conn::default()), here: 0 }
    }

    /// `params` attached over `line`, which answers with `result`: the call
    /// there, and what the viewer is answered.
    async fn attach(
        &mut self,
        line: &Arc<Line>,
        rx: &mut Frames,
        params: Value,
        result: Value,
    ) -> (Value, Result<Value, RpcError>) {
        let (c, conn, m) = (self.core.clone(), self.conn.clone(), self.machine.clone());
        let task = tokio::spawn(async move { attach(&c, &conn, &m, params).await });
        let request: Value = serde_json::from_slice(&rx.recv().await.unwrap()[wire::HEADER..]).unwrap();
        line.take_reply(json!({"id": request["id"], "result": result}));
        let reply = task.await.unwrap();
        if let Ok(reply) = &reply {
            self.here = reply["channel"].as_u64().unwrap() as u32;
            self.conn.with(|out| out.resume(self.here));
        }
        (request, reply)
    }

    /// A new line comes up: the machine's line from now on.
    fn up(&self) -> (Arc<Line>, Frames) {
        self.up_with(json!({}))
    }

    /// A new line whose core there said `hello`.
    fn up_with(&self, hello: Value) -> (Arc<Line>, Frames) {
        let (frames, rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &hello);
        self.core.lock().lines.insert(self.machine.id.clone(), LineState::Connected(line.clone()));
        (line, rx)
    }

    /// What `hold` does once a line has dropped.
    fn drop_line(&self, line: &Line) {
        self.core.lock().lines.insert(self.machine.id.clone(), LineState::Connecting);
        line.close();
        lost_line(line, &self.machine.name);
    }

    /// The viewers waiting for `line`, reattached up to the call that brings this one back.
    async fn reattaching(&self, line: &Arc<Line>, rx: &mut Frames) -> (tokio::task::JoinHandle<()>, Value) {
        let task = tokio::spawn(reattach(self.core.clone(), self.machine.clone(), line.clone()));
        let request: Value = serde_json::from_slice(&rx.recv().await.unwrap()[wire::HEADER..]).unwrap();
        assert_eq!(request["method"], "terminal.attach");
        (task, request["id"].clone())
    }

    fn credit(&self, bytes: usize) {
        assert!(relay_in(&self.core, &self.conn, self.here, wire::CREDIT, &(bytes as u32).to_le_bytes()));
    }

    /// What the viewer is sent from now on, in the order it gets it.
    fn sent(&self) -> Vec<(u8, Vec<u8>)> {
        let mut frames = Vec::new();
        self.conn.with(|out| {
            out.credit(self.here, 1 << 30);
            while let Some(frame) = out.next_frame() {
                let (_, kind, _) = wire::decode_header(frame[..wire::HEADER].try_into().unwrap()).unwrap();
                frames.push((kind, frame[wire::HEADER..].to_vec()));
            }
        });
        frames
    }
}

impl Drop for Attached {
    /// The core's registry writer may still land a snapshot while the root is
    /// removed (the removal then finds it not empty): try again for a while.
    fn drop(&mut self) {
        let start = std::time::Instant::now();
        while std::fs::remove_dir_all(&self.root).is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
            && start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The credits sent on a line so far: (channel there, bytes).
fn credits(rx: &mut Frames) -> Vec<(u32, u32)> {
    let mut credits = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        let (channel, kind, _) = wire::decode_header(frame[..wire::HEADER].try_into().unwrap()).unwrap();
        if kind == wire::CREDIT {
            credits.push((channel, u32::from_le_bytes(frame[wire::HEADER..].try_into().unwrap())));
        }
    }
    credits
}

#[tokio::test]
async fn a_reattached_stream_is_credited_only_for_its_own_bytes_from_its_first() {
    let (f, old, _old_rx) = Attached::new(9).await;
    relay_out(&f.core, &old, 9, wire::DATA, &[b'x'; 1000]);
    f.drop_line(&old);
    let (line, mut rx) = f.up();
    let (task, id) = f.reattaching(&line, &mut rx).await;
    line.take_reply(json!({"id": id, "result": {"channel": 17}}));
    // Read by the line right after the reply, before the reattach runs.
    relay_out(&f.core, &line, 17, wire::DATA, b"paint");
    task.await.unwrap();
    // The viewer reads it all: the old stream's bytes and the notice were paid for by the old stream.
    f.credit(1000 + lost_notice("away").len() + 5);
    assert_eq!(credits(&mut rx), [(17, 5)]);
}

#[tokio::test]
async fn a_credit_sent_before_the_reattach_runs_reaches_the_new_stream() {
    let (f, old, mut old_rx) = Attached::new(9).await;
    f.drop_line(&old);
    let (line, mut rx) = f.up();
    let (task, id) = f.reattaching(&line, &mut rx).await;
    line.take_reply(json!({"id": id, "result": {"channel": 17}}));
    relay_out(&f.core, &line, 17, wire::DATA, b"paint");
    f.credit(lost_notice("away").len() + 5);
    task.await.unwrap();
    assert_eq!(credits(&mut old_rx), [], "the old line is gone");
    assert_eq!(credits(&mut rx), [(17, 5)]);
}

#[tokio::test]
async fn a_line_lost_while_reattaching_leaves_the_viewer_for_the_next_line() {
    let (f, old, _old_rx) = Attached::new(9).await;
    f.drop_line(&old);
    let (line, mut rx) = f.up();
    let (task, _) = f.reattaching(&line, &mut rx).await;
    f.drop_line(&line); // before it answered
    task.await.unwrap();
    assert!(f.core.lock().relayed.contains_key(&(conn_key(&f.conn), f.here)), "the viewer waits");
    let (line, mut rx) = f.up();
    let (task, id) = f.reattaching(&line, &mut rx).await;
    line.take_reply(json!({"id": id, "result": {"channel": 21}}));
    task.await.unwrap();
    relay_out(&f.core, &line, 21, wire::DATA, b"back");
    let sent = f.sent();
    assert!(sent.iter().all(|(kind, _)| *kind == wire::DATA), "still open: {sent:?}");
    let data: Vec<u8> = sent.into_iter().flat_map(|(_, bytes)| bytes).collect();
    assert!(data.ends_with(b"back"));
}
