"""What a person meets in scripts/e2e-remote.py, and what the App hears: the
terminal flows of `machine add`/`login`/`update` (asked, shown each step,
held with --hold) driven through a pseudo-terminal, the event stream followed
across the whole round, the host-key and other-core states, an ssh agent only
the terminal has, and the checks of design section 8 against odd accounts.
Its functions use the e2e module given to `bind`."""
import json
import pathlib
import re
import subprocess
import sys
import time

E = None


def bind(module):
    global E
    E = module


# A command in an 80-column pseudo-terminal, answered as a person would:
# argv[1] is a JSON list of [regex, text]; when what came since the last
# answer matches the first, its text is typed. It ends when the command does.
SCRIPTED = r"""
import fcntl, json, os, pty, re, select, signal, struct, sys, termios, time
steps, argv = json.loads(sys.argv[1]), sys.argv[2:]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 80, 0, 0))
out, seen, start, status = b"", 0, time.time(), None
def read(wait):
    global out
    if not select.select([fd], [], [], wait)[0]:
        return False
    try:
        data = os.read(fd, 65536)
    except OSError:
        data = b""
    out += data
    return bool(data)
while time.time() - start < 900:
    found = steps and re.search(steps[0][0].encode(), out[seen:])
    if found:
        seen += found.end()
        os.write(fd, steps.pop(0)[1].encode())
    read(0.2)
    done, code = os.waitpid(pid, os.WNOHANG)
    if done:
        status = os.waitstatus_to_exitcode(code)
        while read(0.5):
            pass
        break
if status is None:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
print(json.dumps({"status": status, "output": out.decode("utf-8", "replace"), "unanswered": [s[0] for s in steps]}))
"""

# The App's event stream: a plain `events.subscribe {}` on the hub's socket,
# one JSON line per message (with `kind` msg), dialled again whenever the hub
# is gone (`lost`, then `connected`). It ends when its stdin closes.
EVENTS = r"""
import json, select, socket, sys, time
def out(**m):
    m["t"] = time.time()
    sys.stdout.write(json.dumps(m) + "\n")
    sys.stdout.flush()
def stdin_closed(wait):
    return sys.stdin in select.select([sys.stdin], [], [], wait)[0] and not sys.stdin.readline()
while True:
    try:
        s = socket.socket(socket.AF_UNIX)
        s.connect(sys.argv[1])
    except OSError:
        if stdin_closed(0.3):
            sys.exit(0)
        continue
    s.sendall(b'{"jsonrpc":"2.0","id":1,"method":"events.subscribe","params":{}}\n')
    out(kind="connected")
    buf = b""
    while True:
        ready = select.select([s, sys.stdin], [], [], 1)[0]
        if sys.stdin in ready and not sys.stdin.readline():
            sys.exit(0)
        if s not in ready:
            continue
        chunk = s.recv(65536)
        if not chunk:
            out(kind="lost")
            break
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            out(kind="msg", msg=json.loads(line))
"""


def screen(output):
    """The text as a terminal ends up showing it: a line rewritten in place
    (`\r`) shows its last version; no escapes."""
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", output.replace("\r\n", "\n"))
    return "\n".join(line.split("\r")[-1].rstrip() for line in text.split("\n"))


def flow(hub, what, steps, *args, env=None):
    """`nhc <args>` in a terminal where the hub is, answered by `steps`; kept for the summary."""
    argv = [str(a) for a in args]
    if hasattr(hub, "box"):
        extra = ["env", *(f"{k}={v}" for k, v in (env or {}).items())] if env else []
        raw = hub.python(SCRIPTED, json.dumps(steps), *extra, "nhc", *argv)
    else:
        raw = E.run([sys.executable, "-I", "-c", SCRIPTED, json.dumps(steps), hub.binary, *argv],
                    env={**hub.env, **(env or {})}, timeout=1000)[1]
    out = json.loads(raw)
    out["screen"] = screen(out["output"])
    E.LOG.shown(f"{what} (exit {out['status']})", out["screen"])
    E.check(not out["unanswered"], f"{what}: every prompt came ({out['unanswered']})")
    return out


def has(out, *lines):
    for line in lines:
        E.check(line in out["screen"], f"the terminal shows {line!r}")


HOLD = ["press Enter to close", "\r"]


# --- the event stream -----------------------------------------------------------------------

class Follower:
    """The App's stream for a whole round, read back by `stop`."""

    def __init__(self, hub, path):
        self.path = path
        self.file = path.open("w")
        E.LOG.raw(f"$ (event stream on {hub.socket}) > {path}")
        self.proc = subprocess.Popen(hub.spawn(EVENTS, hub.socket), stdin=subprocess.PIPE, stdout=self.file,
                                     stderr=subprocess.STDOUT)

    def caught_up(self):
        """Waits until, since its last reconnect, it heard far connected and
        synced: a hub just stopped may still answer one more subscribe."""
        def since():
            lines = [json.loads(line) for line in self.path.read_text().splitlines() if line.startswith("{")]
            story = far_story(lines)
            last = max((i for i, s in enumerate(story) if s == ("hub", "connected")), default=0)
            return any(s[1:3] == ("connected", True) for s in story[last:])
        E.wait_for("the App's stream caught up with the hub", since, timeout=60)

    def stop(self):
        if self.proc.poll() is None:
            self.proc.stdin.close()
            try:
                self.proc.wait(30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.file.close()
        lines = []
        for line in self.path.read_text().splitlines():
            try:
                lines.append(json.loads(line))
            except ValueError:
                E.LOG.raw(f"  event stream: {line}")
        return lines


def far_story(lines, name="far"):
    """What the stream told of `name`, in order: its states, its terminals' kinds."""
    story, ids = [], set()
    for line in lines:
        if line["kind"] != "msg":
            story.append(("hub", line["kind"]))
            continue
        msg = line["msg"]
        if msg.get("id") == 1:
            for m in msg.get("result", {}).get("machines", []):
                if m["name"] == name:
                    ids.add(m["id"])
                    story.append(("snapshot", m["state"], m.get("synced"), (m.get("build") or {}).get("releaseSeq")))
            continue
        p = msg.get("params", {})
        if msg.get("method") != "event":
            continue
        if p.get("type") == "machine":
            d = p["data"]
            if d.get("name") == name or d.get("id") in ids:
                ids.add(d["id"])
                story.append(("removed",) if d.get("removed") else
                             ("machine", d["state"], d.get("synced"), (d.get("build") or {}).get("releaseSeq")))
        elif str(p.get("terminal", "")).startswith(f"{name}/"):
            story.append(("terminal", p["type"], p["terminal"]))
    return story


def rm_scenario(hub, far, pid, label, stream):
    """`machine rm` once the App's stream holds all before it: far's core and
    terminal run on there, and its files stay."""
    E.LOG.say(f"### {label}: machine rm")
    stream.caught_up()
    core = far.show(next(u for u in far.units() if u.startswith("nearshell-serve-")), "MainPID")
    hub.nhc("machine", "rm", "far")
    E.check(hub.machine("far") is None, "the hub forgot far")
    time.sleep(1)
    E.check(far.alive(core) and far.alive(pid), f"far's core ({core}) and its terminal ({pid}) still run")
    E.check(far.build_info()["releaseSeq"] == 2, "far's files are where they were")


def check_stream(lines, label):
    """(a): the App heard the add, the update, the lost line and the rm."""
    story = far_story(lines)
    E.LOG.raw(f"  {label}: far in the event stream: {json.dumps(story)}")
    def at(pick, after=0):
        return next((i for i in range(after, len(story)) if pick(story[i])), None)
    up = lambda s, seq=None: s[0] in ("machine", "snapshot") and s[1] == "connected" and s[2] is True and (
        seq is None or s[3] == seq)
    added = at(lambda s: up(s, 1))
    E.check(added is not None and at(lambda s: s[0] == "machine" and s[2] is False) < added,
            "the stream told far's add: first not synced, then connected and synced on release 1")
    created = at(lambda s: s[:2] == ("terminal", "created"), added)
    E.check(created is not None, f"then its terminals as they were made ({story[created][2]})")
    # A hub restarted meanwhile tells it in its snapshot instead.
    held = at(lambda s: s[0] in ("machine", "snapshot") and s[1] == "needs_update", created)
    updated = held is not None and at(lambda s: up(s, 2), held)
    E.check(bool(updated), "the update: needs_update, then connected and synced on release 2")
    lost = at(lambda s: s[0] == "machine" and s[1] != "connected", updated)
    back = lost is not None and at(lambda s: up(s), lost)
    E.check(bool(back) and ("hub", "lost") not in story[lost:back],
            f"a lost line, the hub running on: {story[lost][1]}, then connected and synced again")
    gone = at(lambda s: s[0] == "removed", back)
    after = story[gone + 1:] if gone is not None else []
    E.check(gone is not None and any(s[:2] == ("terminal", "removed") for s in after)
            and not any(s[0] == "terminal" and s[1] != "removed" for s in after),
            "the rm: far removed, then its terminals removed, nothing else of them")
    states = [s[1] for s in story if s[0] == "machine"]
    E.LOG.say(f"  far's states as the App heard them: {' > '.join(dict.fromkeys(states))}")


# --- (b) a person adds a machine ---------------------------------------------------------

def interactive_add(hub, far, label):
    """From an empty account and a host key nobody knows yet to connected:
    only the target is typed, the name is the offered one, the key accepted."""
    E.LOG.say(f"### {label}: a person adds far in a terminal (`machine add --install --hold`)")
    e = hub.fails("host_key_unknown", "machine", "add", "far", "--name", "far")
    E.LOG.shown("machine add, the host key unknown (--json, nobody to ask)", json.dumps(e))
    out = flow(hub, "machine add --install --hold, answered", [
        [r"ssh target \(", "fa r\r"], [r"bad ssh target", ""], [r"ssh target \(", "far\r"],
        [r"name for it here \[far\]: ", "\r"], [r"\(yes/no(/\[fingerprint\])?\)\? ", "yes\r"], HOLD,
    ], "machine", "add", "--install", "--hold")
    E.check(out["status"] == 0, "it exits 0 after Enter")
    has(out, "logged in\n", "looking for the remote core there... not installed\n",
        f"finding out what far runs... linux {far.box.arch}\n", ", arrived\nstarting it there... done\n",
        "adding it here... done\n", "waiting for the line... connected\n",
        "added far: installed its remote core (release 1); connected")
    sending = [v for line in out["output"].split("\n") for v in line.split("\r") if v.startswith("sending the remote")]
    E.check(len(sending) > 2 and all(len(v) < 80 for v in sending),
            "the progress line, rewritten in place, fits 80 columns")
    return out


# --- (d) whose host, whose core ------------------------------------------------------------

def trust_scenario(hub, far, label, forget, restore, end_logins, ask):
    """After `machine rm`: added again; its host key forgotten here, then
    another host key there, then another core there. `ask`: ssh here asks
    about an unknown key (else it refuses it and `restore` puts it back)."""
    LOG = E.LOG
    LOG.say(f"### {label}: far's host key unknown here")
    hub.json("machine", "add", "far", "--name", "far")
    E.wait_for("far is connected", lambda: hub.state("far") == "connected")
    forget()
    end_logins()
    far.restart()
    E.wait_for("far is host_key_unknown", lambda: hub.state("far") == "host_key_unknown", timeout=90)
    hub.human("machine ls, host key unknown", "machine", "ls")
    if ask:
        out = flow(hub, "machine login --hold, accepting the key", [
            [r"\(yes/no(/\[fingerprint\])?\)\? ", "yes\r"], HOLD], "machine", "login", "far", "--hold")
    else:
        restore()
        out = flow(hub, "machine login --hold", [HOLD], "machine", "login", "far", "--hold")
    E.check(out["status"] == 0 and "logged in to far: connected" in out["screen"], "logging in brings it back")

    LOG.say(f"### {label}: another host key there")
    box = far.box
    box.sh("cp -p /etc/ssh/ssh_host_ed25519_key /root/e2e-key && cp -p /etc/ssh/ssh_host_ed25519_key.pub "
           "/root/e2e-key.pub && rm -f /etc/ssh/ssh_host_ed25519_key* && "
           "ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key && systemctl restart ssh")
    end_logins()
    far.restart()
    E.wait_for("far is host_key_changed", lambda: hub.state("far") == "host_key_changed", timeout=90)
    hub.human("machine ls, host key changed", "machine", "ls")
    out = flow(hub, "machine login --hold, the key changed", [HOLD], "machine", "login", "far", "--hold")
    E.check(out["status"] != 0 and "error: " in out["screen"], f"logging in refuses it (exit {out['status']})")
    box.sh("mv -f /root/e2e-key /etc/ssh/ssh_host_ed25519_key && mv -f /root/e2e-key.pub "
           "/etc/ssh/ssh_host_ed25519_key.pub && systemctl restart ssh")
    out = flow(hub, "machine login --hold, the key back", [HOLD], "machine", "login", "far", "--hold")
    E.check(out["status"] == 0 and "logged in to far: connected" in out["screen"], "the old key back: connected")

    LOG.say(f"### {label}: another core there (wiped, installed again)")
    # Moved away first, so a line coming back finds nothing to start; killed by
    # process name, as a pattern on the command line would match this very shell.
    far.user_sh(f"{far.env}{far.root}/bin/nhc server stop; mv {far.root} {far.root}.gone; "
                f"pkill -u {far.user} -x nhc; sleep 1; pkill -u {far.user} -x nhc; rm -rf {far.root}.gone",
                check_rc=False)
    E.check(far.box.sh(f"test -e {far.root}", check_rc=False)[0] != 0, "far's remote core and its state are gone")
    E.wait_for("the line to far is down", lambda: hub.state("far") != "connected", timeout=60)
    out = flow(hub, "machine update --hold, another core there", [HOLD], "machine", "update", "far", "--hold")
    E.check(hub.state("far") == "different_core", "the hub holds far as different_core")
    E.check(out["status"] not in (0, None) and "but another core answers at far" in out["screen"]
            and "machine rm far" in out["screen"], f"the update says what it did, what the hub reached and what "
            f"to do (exit {out['status']}, kept through --hold)")
    hub.nhc("machine", "rm", "far")


# --- an ssh agent only the terminal has ---------------------------------------------------

def agent_scenario(hub, box, b, port, keys, label):
    """far2 lets in only a key held by an ssh agent (a scratch one) that the
    terminal has and the hub does not (SSH_AUTH_SOCK)."""
    LOG = E.LOG
    LOG.say(f"### {label}: a key only in the terminal's ssh agent")
    key = E.keygen(b / "agent-key", "nhc-e2e-agent")
    box.account("far2", key.with_suffix(".pub").read_bytes())
    root = f"/home/far2/.local/share/nearshell/{E.BUNDLE}"
    with (b / "ssh_config").open("a") as conf:
        conf.write(f"Host far2\n  HostName 127.0.0.1\n  Port {port}\n  User far2\n  IdentityFile {b}/no-key\n"
                   f"  IdentityAgent SSH_AUTH_SOCK\n  UserKnownHostsFile {b}/known_hosts\n"
                   "  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n  UpdateHostKeys no\n"
                   "  PasswordAuthentication no\n  KbdInteractiveAuthentication no\n")
    sock = b / "agent.sock"
    started = E.run(["/usr/bin/ssh-agent", "-a", sock], env={"PATH": "/usr/bin:/bin"})[1]
    pid = re.search(r"SSH_AGENT_PID=(\d+)", started).group(1)
    agent = {"SSH_AUTH_SOCK": str(sock)}
    try:
        E.run(["/usr/bin/ssh-add", "-q", key], env={"PATH": "/usr/bin:/bin", **agent})
        conf = dict(line.split(" ", 1) for line in E.run(["/usr/bin/ssh", "-G", "-F", b / "ssh_config", "far2"],
                                                          env={"PATH": "/usr/bin:/bin", **agent})[1].splitlines())
        E.check(conf["identityagent"] == "SSH_AUTH_SOCK" and conf["identityfile"] == f"{b}/no-key",
                "far2: ssh here takes keys from $SSH_AUTH_SOCK only (the scratch agent)")
        e = hub.fails("needs_login", "machine", "add", "far2", "--name", "far2")
        LOG.shown("machine add far2, the hub without the agent (--json)", json.dumps(e))
        out = flow(hub, "machine add far2 --install --hold, the agent in the terminal only", [HOLD],
                   "machine", "add", "far2", "--install", "--hold", env=agent)
        E.check(out["status"] == 0 and "; connected" in out["screen"],
                "added: the hub's line rides the terminal's shared login (its ControlMaster)")
        far2 = E.Far(box, "far2", root, "test", f"NEARSHELL_HC_TEST_ROOT={root} ")
        for m in (hub.root / "run").glob("ssh-*"):
            E.run(["/usr/bin/ssh", "-F", b / "ssh_config", "-S", m, "-O", "exit", "far2"], check_rc=False)
        far2.restart()
        E.wait_for("without that login the hub cannot log in: far2 is needs_login",
                   lambda: hub.state("far2") == "needs_login", timeout=90)
        hub.human("machine ls, far2 needs a login", "machine", "ls")
        out = flow(hub, "machine login far2 --hold, no agent in the terminal either", [HOLD],
                   "machine", "login", "far2", "--hold")
        E.check(out["status"] != 0 and "error: " in out["screen"], "it fails, and says so")
        out = flow(hub, "machine login far2 --hold, the agent in the terminal", [HOLD],
                   "machine", "login", "far2", "--hold", env=agent)
        E.check(out["status"] == 0 and "logged in to far2: connected" in out["screen"], "logged in: connected")
        hub.nhc("machine", "rm", "far2")
        far2.user_sh(f"{far2.env}{root}/bin/nhc server stop", check_rc=False)
    finally:
        E.run(["kill", pid], check_rc=False)


# --- design section 8 ----------------------------------------------------------------------

def logout(hub, far):
    """Ends the hub's logins there: the shared ssh login (`machine add` in a
    terminal opened it) and the core's line (the hub core stops)."""
    hub.box.sh(f'for m in /tmp/nearshell-1000-{E.SHA8}/ssh-*; do [ -S "$m" ] && ssh -S "$m" -O exit far; done; true',
               user="hub")
    hub.nhc("server", "stop")
    E.wait_for("far has no login session", lambda: not far.box.sh(
        f"loginctl list-sessions --no-legend | awk '$3 == \"{far.user}\"'")[1].strip(), timeout=30)


def side_checks(hub, box, far, far_socket, label):
    """§8 of the design: $HOME unlike the account's, a socket directory taken
    by another account, and /tmp cleaners against the live socket."""
    LOG, check, BUNDLE = E.LOG, E.check, E.BUNDLE
    LOG.say(f"### {label}: an ssh session whose $HOME is not the account's")
    e = hub.fails("hc_missing", "machine", "add", "odd", "--name", "odd")
    LOG.shown("machine add, $HOME moved (--json)", json.dumps(e))
    hub.fails("invalid_params", "machine", "add", "odd", "--name", "odd", "--install",
              human="add --install, $HOME moved")
    left = box.sh(f"ls -A /home/odd/elsewhere/.local/share/nearshell/{BUNDLE}/bin; "
                  "ls -A /home/odd/.local 2>&1 || true")[1]
    LOG.say(f"  odd: {left.strip()}")
    check(".nhc.new" not in left, "no temp copy is left where the session's $HOME pointed")
    LOG.say(f"### {label}: another account took the socket directory first")
    taken_uid = box.uid("taken")
    box.sh(f"install -d -m 700 -o mallory /tmp/nearshell-{taken_uid}-{E.SHA8}")
    err = hub.fails("nearshell_socket_dir", "machine", "add", "taken", "--name", "taken", "--install",
                    human="add --install, socket directory taken")
    LOG.raw(json.dumps(err))
    LOG.say("  taken: " + box.sh(f"ls -A /home/taken/.local/share/nearshell/{BUNDLE}/bin 2>&1 || true")[1].strip())
    LOG.say(f"### {label}: /tmp cleaners and the live socket")
    sockdir = str(pathlib.PurePosixPath(far_socket).parent)
    LOG.say("  " + box.sh(f"ls -la --time-style=+ {sockdir}")[1].replace("\n", "\n  "))
    # Decoys: an old file and an old socket nothing listens on.
    decoys = ("echo old > /tmp/e2e-stale && "
              "python3 -c 'import socket; socket.socket(socket.AF_UNIX).bind(\"/tmp/e2e-dead.sock\")'"
              f" && find {sockdir} /tmp/e2e-stale /tmp/e2e-dead.sock -exec touch -h -d '30 days ago' {{}} +")
    stale = f"for f in /tmp/e2e-stale /tmp/e2e-dead.sock; do test -e $f && echo left:$f; done; ls -A {sockdir}"
    box.sh(decoys)
    # Fedora's and Arch's rule (Debian's tmp.conf has no age). touch cannot age
    # ctime, so the rule ages by atime and mtime only (systemd >= 250).
    box.sh("printf 'q /tmp 1777 root root am:10d\\n' > /etc/tmpfiles.d/tmp.conf && systemd-tmpfiles --clean; "
           "rm /etc/tmpfiles.d/tmp.conf")
    left = box.sh(stale)[1]
    check("left:" not in left and "c.sock" in left, "systemd-tmpfiles with a 10d age on /tmp removes an old file and "
          f"an old dead socket, keeps the live sockets and their directory ({left.split()})")
    check(far.user_sh(f"{far.root}/bin/nhc --json server status", check_rc=False)[0] == 0, "far's core still answers")
    box.sh(f"rm -f /tmp/e2e-stale /tmp/e2e-dead.sock; {decoys} && tmpreaper 7d /tmp")
    left = box.sh(stale)[1]
    check("left:/tmp/e2e-stale" not in left and "c.sock" in left,
          f"tmpreaper 7d /tmp (Debian's default file types) removes the old file, keeps sockets ({left.split()})")
    box.sh(f"rm -f /tmp/e2e-stale /tmp/e2e-dead.sock; {decoys} && tmpreaper --all 7d /tmp")
    left = box.sh(f"ls -A {sockdir} 2>&1 || true")[1]
    LOG.say(f"  after tmpreaper --all: {left.strip() or 'empty'}")
    if "c.sock" not in left:
        rc, _, err = far.user_sh(f"{far.root}/bin/nhc --json server status", check_rc=False)
        check(rc != 0 and E.error_kind(err) == "nearshell_socket_lost", "the core there reports nearshell_socket_lost")
        LOG.shown("nhc on the far end after its socket was removed", far.user_sh(
            f"{far.root}/bin/nhc server status", check_rc=False)[2])
        box.sh("pkill -u far -f 'nhc serve' || true")
