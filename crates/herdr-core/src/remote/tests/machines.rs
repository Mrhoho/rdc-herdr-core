//! Machines here as an unfiltered subscriber hears of them.

use super::*;

#[tokio::test]
async fn a_core_there_that_adds_machines_of_its_own_tells_nothing_of_them_here() {
    let f = Attached::start();
    let (tx, mut rx) = mpsc::channel(8);
    {
        let mut st = f.core.lock();
        st.machines.push(f.machine.clone());
        tell(&f.core, &mut st);
        f.core.subscribe(&mut st, Filter::default(), Sink::Plain { tx, behind: Arc::new(AtomicBool::new(false)) });
    }
    let (line, _rx) = f.up();
    let theirs = json!({"type": "machine", "data": {"id": "0f", "name": "gondor", "state": "connected"}});
    mirror_events(&f.core, &f.machine, &line, format!("{}\n", json!({"method": "event", "params": theirs})).as_bytes());
    assert!(rx.try_recv().is_err(), "only the machines added here are told");
    // While what this core tells of its own does reach it.
    tell(&f.core, &mut f.core.lock());
    let told: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!((&told["params"]["type"], &told["params"]["data"]["state"]), (&json!("machine"), &json!("connected")));
}

#[tokio::test]
async fn a_missed_there_unsyncs_the_machine_until_its_changes_are_out() {
    let f = Attached::start();
    f.core.lock().machines.push(f.machine.clone());
    let (line, _rx) = f.up();
    mirror_replace(&f.core, &f.machine, &line, Vec::new()); // as its first snapshot leaves it
    assert!(view(&f.core.lock(), &f.machine).synced);
    let (tx, mut rx) = mpsc::channel(8);
    f.core.subscribe(
        &mut f.core.lock(),
        Filter::default(),
        Sink::Plain { tx, behind: Arc::new(AtomicBool::new(false)) },
    );
    let missed = json!({"method": "missed", "params": {"seq": 9, "terminals": [{"address": "t1", "state": "live"}]}});
    mirror_events(&f.core, &f.machine, &line, format!("{missed}\n").as_bytes());
    let mut heard = Vec::new();
    while let Ok(line) = rx.try_recv() {
        let msg: Value = serde_json::from_str(&line).unwrap();
        let p = &msg["params"];
        heard.push(match (msg["method"].as_str(), p["type"].as_str()) {
            (Some("event"), Some("machine")) => format!("machine synced {}", p["data"]["synced"]),
            (Some("event"), Some(kind)) => format!("{kind} {}", p["terminal"].as_str().unwrap()),
            (method, _) => format!("{} documents {}", method.unwrap(), p["documents"]),
        });
    }
    assert_eq!(
        heard,
        ["machine synced false", "created away/t1", "machine synced true", "missed documents true"],
        "while its changes go out, a terminal missing here may still be there"
    );
}
