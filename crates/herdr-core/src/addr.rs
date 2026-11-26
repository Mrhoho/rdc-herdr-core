//! Terminal addresses: `t7` or `reviewer` is local, `box/t7` lives on core `box`.

use crate::rpc::RpcError;

/// Terminal, machine and session names: typed anywhere without quoting.
pub fn check_name(name: &str) -> Result<(), RpcError> {
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.');
    let starts = name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !starts || name.len() > 32 || !name.chars().all(allowed) || crate::terminals::id_shaped(name) {
        return Err(RpcError::invalid_params(format!(
            "bad name {name:?}: use 1-32 of a-z 0-9 - _ . starting with a letter or digit, not shaped like an id (t7)"
        )));
    }
    Ok(())
}

/// What a core id looks like: 32 hex digits. Routing takes a machine's name or
/// its id, so a machine name must never look like one.
pub fn id_like(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A machine's name: a name (above) that does not look like a core id.
pub fn check_machine_name(name: &str) -> Result<(), RpcError> {
    check_name(name)?;
    match id_like(name) {
        true => Err(RpcError::invalid_params(format!(
            "bad name {name:?}: 32 hex digits look like a machine's id; pick another"
        ))),
        false => Ok(()),
    }
}

/// Something a machine calls itself (its hostname) as a valid name.
pub fn clean_name(raw: &str) -> String {
    let lower: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let name: String = lower.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).chars().take(32).collect();
    if check_name(&name).is_ok() { name } else { "local".into() }
}

/// Returns the local part (id or name) of `address` if it names this core.
pub fn local<'a>(address: &'a str, core_id: &str, core_name: &str) -> Result<&'a str, RpcError> {
    let (machine, local) = match address.split_once('/') {
        None => (None, address),
        Some((machine, local)) => (Some(machine), local),
    };
    if local.is_empty() || local.contains('/') || machine == Some("") {
        return Err(RpcError::invalid_params(format!("bad terminal address {address:?}")));
    }
    match machine {
        Some(m) if m != core_id && m != core_name => Err(RpcError::new(
            "machine_unknown",
            format!("unknown machine {m:?}"),
            "this core only runs local terminals; use an address like t7",
        )),
        _ => Ok(local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_need_no_quoting_anywhere() {
        for good in ["rohan", "can-desktop", "w_2.b", "1.2.3.4", "tmp", "tty", "t3po", "t0", &"a".repeat(32)] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in
            ["", "Rohan", "a b", "a/b", "-x", ".x", "t7", "t1b", "t9zzzzzzzzzz", "caf\u{e9}", "$x", &"a".repeat(33)]
        {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert_eq!(clean_name("Can's MacBook Pro"), "can-s-macbook-pro");
        assert_eq!(clean_name("--"), "local");
    }

    #[test]
    fn a_machine_name_never_looks_like_a_core_id() {
        let id = "0123456789abcdef0123456789abcdef";
        assert!(id_like(id) && check_name(id).is_ok(), "a name otherwise");
        assert_eq!(check_machine_name(id).unwrap_err().kind, "invalid_params");
        for good in ["rohan", &"z".repeat(32), &id[..31], &format!("{}g", &id[..31]), "0123456789abcdef-2"] {
            assert!(check_machine_name(good).is_ok(), "{good}");
        }
        assert!(check_machine_name("t7").is_err() && check_machine_name(&"a".repeat(32)).is_err());
    }

    #[test]
    fn local_remote_and_self_addresses() {
        assert_eq!(local("t7", "abc", "box").unwrap(), "t7");
        assert_eq!(local("reviewer", "abc", "box").unwrap(), "reviewer");
        assert_eq!(local("box/t7", "abc", "box").unwrap(), "t7");
        assert_eq!(local("abc/t7", "abc", "box").unwrap(), "t7");
        assert_eq!(local("other/t7", "abc", "box").unwrap_err().kind, "machine_unknown");
        for bad in ["", "/t7", "box/", "a/b/c"] {
            assert_eq!(local(bad, "abc", "box").unwrap_err().kind, "invalid_params", "{bad}");
        }
    }
}
