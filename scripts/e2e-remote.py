#!/usr/bin/env python3
"""End-to-end check of the Linux remote core over real OpenSSH and a real
`systemd --user` (see docs/nearshell-remote-e2e.md). Not part of CI: it needs
Docker (privileged containers running systemd as PID 1) and builds what it
installs from HEAD of this checkout.

Round a: a product remote build as the hub, in one container, reaches a far
account in another with plain `ssh far` (the hub account's own ssh config,
key and known_hosts, all generated here).
Round b (macOS): a managed-test hub on this Mac, started by a scripted owner,
reaches a Linux container through /usr/bin/ssh with a config of its own; the
user's ~/.ssh, agent and keychain are never used.

Every container, network and core it starts is removed again (images stay,
`--remove-images` drops them)."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
import pathlib
import platform
import re
import secrets
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import traceback

HERE = pathlib.Path(__file__).resolve().parent


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


viewer = load("e2e_remote_viewer", "e2e-remote-viewer.py")
flows = load("e2e_remote_flows", "e2e-remote-flows.py")
Failed, ticks, PTY, flow, HOLD = viewer.Failed, viewer.ticks, viewer.PTY, flows.flow, flows.HOLD

BUNDLE = "com.nearshell.e2e"
PATCHSET = "e2e"
ARCHES = {"aarch64": ("aarch64-unknown-linux-musl", "linux/arm64"),
          "x86_64": ("x86_64-unknown-linux-musl", "linux/amd64")}
LABEL = "org.nearshell.e2e=1"
NETWORK = "nhc-e2e"
SHA8 = hashlib.sha256(BUNDLE.encode()).hexdigest()[:8]
# The hub container's architecture: this host's own (anything else is emulated).
HUB = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
TICKER = "i=0; while :; do i=$((i+1)); echo tick $i; sleep 1; done"

DOCKERFILE = """FROM debian:bookworm-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \\
      systemd systemd-sysv libpam-systemd dbus openssh-server procps python3 iproute2 tmpreaper \\
    && rm -rf /var/lib/apt/lists/* /etc/ssh/ssh_host_* \\
    && systemctl enable ssh
STOPSIGNAL SIGRTMIN+3
CMD ["/lib/systemd/systemd"]
"""

class Log:
    def __init__(self, path):
        self.file = path.open("w")
        self.visible = []

    def say(self, text):
        print(text, flush=True)
        self.file.write(text + "\n")
        self.file.flush()

    def raw(self, text):
        self.file.write(text + ("\n" if not text.endswith("\n") else ""))
        self.file.flush()

    def shown(self, what, text):
        """Text a person (or the Swift client) meets, kept for the summary."""
        self.visible.append((what, text.strip()))
        self.say(f"  [{what}]\n" + "\n".join("    " + line for line in text.strip().splitlines()))


LOG = None


def check(cond, what):
    if not cond:
        raise Failed(what)
    LOG.say(f"  ok: {what}")


def run(args, check_rc=True, input=None, env=None, timeout=900):
    shown = ["<token>" if i and str(args[i - 1]) == "--owner-token" else str(a) for i, a in enumerate(args)]
    LOG.raw("$ " + " ".join(shlex.quote(a) for a in shown))
    out = subprocess.run([str(a) for a in args], input=input, capture_output=True, env=env, timeout=timeout)
    stdout, stderr = out.stdout.decode(errors="replace"), out.stderr.decode(errors="replace")
    LOG.raw(f"  rc={out.returncode}\n" + "".join(f"  | {line}\n" for line in (stdout + stderr).splitlines()))
    if check_rc and out.returncode != 0:
        raise Failed(f"{shown[0]} {' '.join(shown[1:3])}...: exit {out.returncode}: {stderr.strip()[-600:]}")
    return out.returncode, stdout, stderr


def wait_for(what, probe, timeout=60, every=0.5):
    end = time.time() + timeout
    while True:
        value = probe()
        if value:
            LOG.say(f"  ok: {what}")
            return value
        if time.time() > end:
            raise Failed(f"timed out: {what}")
        time.sleep(every)


def error_kind(stderr):
    try:
        return json.loads(stderr)["error"]["kind"]
    except (ValueError, KeyError, TypeError):
        return None


# --- builds -----------------------------------------------------------------

def clean_clone(builds, commit):
    """A checkout of `commit` only (no working changes), with the pinned Ghostty."""
    src = builds / "src"
    if (src / ".git").exists() and run(["git", "-C", src, "rev-parse", "HEAD"])[1].strip() == commit:
        return src
    shutil.rmtree(src, ignore_errors=True)
    main = HERE.parent
    run(["git", "clone", "--quiet", "--shared", "--no-checkout", main, src])
    run(["git", "-C", src, "checkout", "--quiet", "--detach", commit])
    modules = pathlib.Path(run(["git", "-C", main, "rev-parse", "--git-common-dir"])[1].strip())
    if not modules.is_absolute():
        modules = main / modules
    run(["git", "-C", src, "config", "submodule.vendor/ghostty.url", modules / "modules/vendor/ghostty"])
    run(["git", "-C", src, "-c", "protocol.file.allow=always", "submodule", "update", "--quiet", "--init"])
    return src


def identity_env(commit, seq):
    env = {k: os.environ[k] for k in ("PATH", "HOME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "ZIG")
           if k in os.environ}
    env.update(NEARSHELL_CORE_COMMIT=commit, NEARSHELL_CORE_PATCHSET=PATCHSET, NEARSHELL_CORE_RELEASE_SEQ=str(seq),
               NEARSHELL_CORE_CONFIG="dev", NEARSHELL_CORE_BUNDLE_ID=BUNDLE)
    return env


def build(builds, commit, arches, rounds, cache):
    """remote-<n>: product payloads (build-remote.py). test-<n>: nearshell-test
    payloads (build-remote.py --test) and hub-<n>/nhc, a managed-test hub for
    this Mac (round b)."""
    triples = sorted({ARCHES[a][0] for a in arches} | ({ARCHES[HUB][0]} if "a" in rounds else set()))
    stamp = builds / "stamp.json"
    want = {"commit": commit, "triples": triples, "rounds": sorted(rounds)}
    have = json.loads(stamp.read_text()) if stamp.exists() else {}
    if have.get("commit") == commit and set(triples) <= set(have["triples"]) and set(rounds) <= set(have["rounds"]):
        LOG.say(f"builds: reusing {builds}")
        return
    for old in builds.glob("*-[12]"):
        shutil.rmtree(old)
    src = clean_clone(builds, commit)
    cache.mkdir(parents=True, exist_ok=True)
    def payloads(seq, kind, targets, *test):
        args = ["python3", "-B", src / "scripts/build-remote.py", "--source", src, "--commit", commit,
                "--patchset", PATCHSET, "--release-seq", seq, "--config", "dev", "--bundle-id", BUNDLE,
                "--output", builds / f"{kind}-{seq}", "--target-dir", cache / kind, *test]
        run(args + [a for t in targets for a in ("--target", t)], timeout=3600)
    for seq in (1, 2):
        if "a" in rounds:
            LOG.say(f"builds: remote payloads, release {seq} ({', '.join(triples)})")
            payloads(seq, "remote", triples)
        if "b" in rounds:
            LOG.say(f"builds: managed-test hub and nearshell-test payloads, release {seq}")
            env = identity_env(commit, seq)
            mac = cache / "mac"
            subprocess_run_in(src, ["cargo", "build", "--locked", "-p", "herdr-core", "--features", "managed-test",
                                    "--bin", "nhc", "--target-dir", mac], env)
            hub = builds / f"hub-{seq}"
            hub.mkdir()
            shutil.copy2(mac / "debug/nhc", hub / "nhc")
            payloads(seq, "test", sorted({ARCHES[a][0] for a in arches}), "--test")
    stamp.write_text(json.dumps(want))


def subprocess_run_in(cwd, args, env):
    LOG.raw(f"$ (cd {cwd}) " + " ".join(map(str, args)))
    out = subprocess.run([str(a) for a in args], cwd=cwd, env=env, capture_output=True)
    LOG.raw(out.stderr.decode(errors="replace")[-4000:])
    if out.returncode != 0:
        raise Failed(f"build failed: {out.stderr.decode(errors='replace')[-1500:]}")


# --- containers ---------------------------------------------------------------

def image(arch):
    tag = f"nhc-e2e:{arch}"
    if run(["docker", "image", "inspect", tag], check_rc=False)[0] != 0:
        LOG.say(f"image: building {tag}")
        with tempfile.TemporaryDirectory() as ctx:
            pathlib.Path(ctx, "Dockerfile").write_text(DOCKERFILE)
            run(["docker", "build", "--platform", ARCHES[arch][1], "--label", LABEL, "-t", tag, ctx], timeout=3600)
    return tag


def teardown_docker():
    ids = run(["docker", "ps", "-aq", "--filter", f"label={LABEL}"], check_rc=False)[1].split()
    if ids:
        run(["docker", "rm", "-f", *ids], check_rc=False)
    run(["docker", "network", "rm", NETWORK], check_rc=False)


class Box:
    """A disposable Linux machine: systemd as PID 1, sshd, nothing of this Mac mounted."""

    def __init__(self, name, arch, publish=False):
        self.name, self.arch, self.uids = name, arch, {}
        args = ["docker", "run", "-d", "--privileged", "--cgroupns=private", "--label", LABEL, "--name", name,
                "--hostname", name, "--network", NETWORK, "--platform", ARCHES[arch][1],
                "--tmpfs", "/run", "--tmpfs", "/run/lock"]
        if publish:
            args += ["-p", "127.0.0.1::22"]
        run(args + [image(arch)])
        wait_for(f"{name}: systemd is up", lambda: self.sh(
            "systemctl is-system-running", check_rc=False)[1].strip() in ("running", "degraded"), timeout=180)

    def exec(self, user=None):
        args = ["docker", "exec", "-i"]
        if user:
            uid = self.uid(user) if user != "root" else 0
            args += ["-u", user, "-w", f"/home/{user}", "-e", f"HOME=/home/{user}", "-e", f"USER={user}",
                     "-e", f"XDG_RUNTIME_DIR=/run/user/{uid}", "-e", "LANG=C.UTF-8",
                     "-e", f"PATH=/home/{user}/bin:/usr/local/bin:/usr/bin:/bin"]
        return args + [self.name]

    def sh(self, script, user=None, check_rc=True, input=None, timeout=300):
        return run(self.exec(user) + ["sh", "-c", script], check_rc=check_rc, input=input, timeout=timeout)

    def uid(self, user):
        if user not in self.uids:
            self.uids[user] = int(run(["docker", "exec", self.name, "id", "-u", user])[1])
        return self.uids[user]

    def put(self, data, path, owner="root", mode="600"):
        self.sh(f"mkdir -p {shlex.quote(str(pathlib.PurePosixPath(path).parent))} && cat > {shlex.quote(path)} && "
                f"chown {owner}: {shlex.quote(path)} && chmod {mode} {shlex.quote(path)}", input=data)

    def copy(self, src, dest):
        run(["docker", "cp", src, f"{self.name}:{dest}"])

    def port(self):
        return int(run(["docker", "port", self.name, "22/tcp"])[1].split(":")[-1])

    def account(self, user, client_pub, profile=""):
        """A login account with the hub's key, a login profile exporting the
        user's own hc session (the remote core must ignore it) and `profile`."""
        exports = f"export HC_SESSION=user-own HC_SOCKET=/home/{user}/own-hc.sock\n{profile}"
        self.sh(f"useradd -m -s /bin/bash {user} && install -d -m 700 -o {user} -g {user} /home/{user}/.ssh")
        self.put(client_pub, f"/home/{user}/.ssh/authorized_keys", user)
        self.put(exports.encode(), f"/home/{user}/.profile", user, "644")
        self.put(exports.encode() + b"case $- in *i*) ;; *) return ;; esac\n", f"/home/{user}/.bashrc", user, "644")

    def sshd(self, host_key, extra=""):
        self.put(host_key.read_bytes(), "/etc/ssh/ssh_host_ed25519_key")
        self.put(host_key.with_suffix(".pub").read_bytes(), "/etc/ssh/ssh_host_ed25519_key.pub", mode="644")
        conf = ("HostKey /etc/ssh/ssh_host_ed25519_key\nPasswordAuthentication no\n"
                f"KbdInteractiveAuthentication no\n{extra}")
        self.put(conf.encode(), "/etc/ssh/sshd_config.d/e2e.conf", mode="644")
        self.sh("systemctl reset-failed ssh; systemctl restart ssh && systemctl is-active ssh")


# --- the far end, inspected as root ---------------------------------------------

class Far:
    """`payloads`: the builds' payload directories for it (`remote-<n>` or
    `test-<n>`); `env`: what its own nhc needs on its command line."""

    def __init__(self, box, user, root, payloads="remote", env=""):
        self.box, self.user, self.root, self.payloads, self.env = box, user, root, payloads, env
        self.uid = box.uid(user)

    def user_sh(self, script, check_rc=True):
        return self.box.sh(f"runuser -u {self.user} -- env XDG_RUNTIME_DIR=/run/user/{self.uid} HOME=/home/{self.user} "
                           f"sh -c {shlex.quote(script)}", check_rc=check_rc)

    def units(self):
        out = self.user_sh("systemctl --user list-units --all --plain --no-legend --type=service", check_rc=False)[1]
        return {line.split()[0]: line.split()[2] for line in out.splitlines() if line.strip()}

    def show(self, unit, prop):
        return self.user_sh(f"systemctl --user show -p {prop} --value {unit}")[1].strip()

    def environ(self, pid):
        raw = self.box.sh(f"cat /proc/{pid}/environ")[1]
        return dict(item.split("=", 1) for item in raw.split("\0") if "=" in item)

    def alive(self, pid):
        return self.box.sh(f"kill -0 {pid}", check_rc=False)[0] == 0

    def build_info(self):
        return json.loads(self.box.sh(f"{self.root}/bin/nhc build-info --json")[1])

    def restart(self, builds=None, seq=None):
        """Its core restarts as an update restarts it (`server restart`; the
        holder keeps the terminals), onto release `seq` put in place by hand
        when given: an install never puts an older one there."""
        if seq:
            new = f"{self.root}/bin/.e2e-nhc"
            self.box.copy(builds / f"{self.payloads}-{seq}" / f"nhc-{ARCHES[self.box.arch][0]}", new)
            self.box.sh(f"chown {self.user}: {new} && chmod 700 {new} && mv -f {new} {self.root}/bin/nhc")
        self.user_sh(f"{self.env}{self.root}/bin/nhc server restart")

    def check_services(self, socket, expected_config):
        """The core and holder run as the user's systemd services under the
        Nearshell names, after the shutdown guard, without HC_SESSION/HC_SOCKET."""
        units = self.units()
        LOG.say("  far units: " + ", ".join(f"{u} {s}" for u, s in sorted(units.items())))
        def active(pattern):
            return [u for u in units if re.fullmatch(pattern, u) and units[u] == "active"]
        serve, hold = active(r"nearshell-serve-[0-9a-f]{8}\.service"), active(r"nearshell-hold-[0-9a-f]{8}\.service")
        guard = active(r"nearshell-shutdown-[0-9a-f]{16}\.service")
        check(len(serve) == 1 and hold and len(guard) == 1, "one nearshell-serve, a nearshell-hold and the "
              "nearshell-shutdown guard are active user services")
        check(not [u for u in units if u.startswith("hc-")], "no hc-* units")
        for unit in serve + hold:
            pid = self.show(unit, "MainPID")
            env = self.environ(pid)
            exe = self.box.sh(f"readlink /proc/{pid}/exe")[1].strip()
            if "qemu" in exe:  # an emulated far end: the emulator runs argv[0]
                exe = self.box.sh(f"tr '\\0' '\\n' < /proc/{pid}/cmdline | sed -n 2p")[1].strip()
            # A holder keeps the binary it started from (replaced: "(deleted)").
            check(exe in (f"{self.root}/bin/nhc", f"{self.root}/bin/nhc (deleted)"), f"{unit} runs {exe}")
            check("HC_SESSION" not in env and "HC_SOCKET" not in env,
                  f"{unit}: no HC_SESSION/HC_SOCKET in its environment (HC_* there: "
                  f"{sorted(k for k in env if k.startswith('HC_'))})")
            unset = self.show(unit, "UnsetEnvironment").split()
            check({"HC_SESSION", "HC_SOCKET"} <= set(unset), f"{unit}: UnsetEnvironment lists HC_SESSION HC_SOCKET")
        order = self.show(serve[0], "After")
        check(guard[0] in order.split(), f"{serve[0]} is ordered after {guard[0]}")
        st = self.box.sh(f"stat -c '%F %a %U' {socket} {pathlib.PurePosixPath(socket).parent}")[1].splitlines()
        check(st[0].startswith("socket") and st[1] == f"directory 700 {self.user}",
              f"the core listens at {socket} (dir 0700, {self.user}'s): {st}")
        listen = self.box.sh(f"ss -xlpn | grep -F {shlex.quote(socket + ' ')} || true")[1]
        check("nhc" in listen, "ss shows nhc listening on it")
        check(self.build_info()["config"] == expected_config, f"installed build is {expected_config}")


# --- hubs --------------------------------------------------------------------------

class Hub:
    """`nhc` as the hub account runs it; `json`/`fails` parse what it prints."""

    def nhc(self, *args, check_rc=True):
        raise NotImplementedError

    def json(self, *args):
        return json.loads(self.nhc("--json", *args)[1])

    def fails(self, kind, *args, human=None):
        rc, _, err = self.nhc("--json", *args, check_rc=False)
        check(rc != 0 and error_kind(err) == kind, f"`nhc {' '.join(args)}` fails with {kind} (got {error_kind(err)})")
        if human:
            self.human(human, *args)
        return json.loads(err)["error"]

    def tty(self, detach, *args):
        raise NotImplementedError

    def human(self, what, *args):
        """What a person sees in a terminal: kept for the summary."""
        out = json.loads(self.tty(0, *args))
        LOG.shown(what, re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", out["output"].replace("\r\n", "\n")))
        return out

    def attach(self, terminal):
        return json.loads(self.tty(3, "attach", terminal))

    def marked(self, terminal):
        """The App's attach, held open (see e2e-remote-viewer.py), at the
        terminal's size: a new one would repaint it at once."""
        t = self.json("get", terminal)
        params = {"terminal": terminal, "marks": True, "cols": t["cols"], "rows": t["rows"], "history": 1000}
        LOG.raw(f"$ (marked viewer on {self.socket}) {json.dumps(params)}")
        argv = self.spawn(viewer.PROGRAM, self.socket, json.dumps(params))
        return viewer.Viewer(argv, params, LOG.say, check, wait_for)

    def machine(self, name):
        found = [m for m in self.json("machine", "ls")["machines"] if m["name"] == name]
        return found[0] if found else None

    def state(self, name):
        m = self.machine(name)
        return m and m["state"]


class LinuxHub(Hub):
    def __init__(self, box, user="hub"):
        self.box, self.user = box, user
        self.socket = f"/tmp/nearshell-1000-{SHA8}/c.sock"

    def nhc(self, *args, check_rc=True):
        return self.box.sh(shlex.join(["nhc", *args]), user=self.user, check_rc=check_rc)

    def python(self, code, *args):
        return self.box.sh(shlex.join(["python3", "-c", code, *args]), user=self.user)[1]

    def tty(self, detach, *args):
        return self.python(PTY, str(detach), "nhc", *args)

    def spawn(self, code, *args):
        return self.box.exec(self.user) + ["python3", "-c", code, *args]

    def masters(self):
        return self.box.sh(f"ls /tmp/nearshell-1000-{SHA8}/ | grep ^ssh- || true", user=self.user)[1].split()


class MacHub(Hub):
    def __init__(self, root, shim, binary):
        self.root, self.binary, self.socket = root, binary, str(root / "run/c.sock")
        self.token = secrets.token_hex(32)
        self.env = {"PATH": f"{shim}:/usr/bin:/bin:/usr/sbin:/sbin", "NEARSHELL_HC_TEST_ROOT": str(root),
                    "HOME": str(root / "wrong-home"), "LANG": "en_US.UTF-8", "TMPDIR": os.environ.get("TMPDIR", "/tmp")}
        (root / "config/herdr-core").mkdir(parents=True, mode=0o700)
        os.chmod(root, 0o700)
        # The owner's lock, held by this process for the hub's lifetime.
        self.lock = (root / "owner.lock").open("a")
        fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def configure(self, payloads):
        verifier = hashlib.sha256(self.token.encode()).hexdigest()
        (self.root / "config/herdr-core/config.toml").write_text(
            f"socket = '{self.root}/run/c.sock'\nowner_token_sha256 = '{verifier}'\nremote_payloads = '{payloads}'\n")

    def nhc(self, *args, check_rc=True):
        return run([self.binary, *args], check_rc=check_rc, env=self.env)

    def owner(self, *args):
        return run([self.binary, "--owner-token", self.token, *args], env=self.env)

    def python(self, code, *args):
        return run([sys.executable, "-I", "-c", code, *args])[1]

    def tty(self, detach, *args):
        return run([sys.executable, "-I", "-c", PTY, str(detach), self.binary, *args], env=self.env)[1]

    def spawn(self, code, *args):
        return [sys.executable, "-I", "-c", code, *args]

    def masters(self):
        return [m.name for m in (self.root / "run").glob("ssh-*")]


# --- the shared scenario -------------------------------------------------------------

def terminal_scenario(hub, far, label):
    """run --on, read, plain attach, marked attach, and what the far end is."""
    LOG.say(f"### {label}: a terminal there")
    t = hub.json("run", "--bg", "--on", "far", "--name", "ticker", "--", "sh", "-c", TICKER)
    LOG.raw(json.dumps(t))
    info = hub.json("get", "far/ticker")
    pid = info["pid"]
    check(far.alive(pid), f"far/ticker is pid {pid} there")
    wait_for("read far/ticker shows its output", lambda: ticks(hub.nhc("read", "far/ticker")[1]))
    probe = "echo NEARSHELL_HC=$NEARSHELL_HC; echo PATH=$PATH; env | grep '^HC_' || true"
    env = hub.nhc("exec", "--on", "far", "--", "sh", "-c", probe)[1]
    check(f"NEARSHELL_HC={far.root}/bin/nhc" in env and f"PATH={far.root}/bin:" in env and "HC_SESSION" not in env,
          "exec there: NEARSHELL_HC and PATH point at the remote core, no HC_SESSION")
    LOG.say(f"### {label}: attach")
    attached = hub.attach("far/ticker")
    LOG.raw(json.dumps(attached))
    check(attached["status"] == 0 and ticks(attached["output"]), f"plain attach to far/ticker shows its output and "
          f"detaches with ctrl+\\ (exit {attached['status']})")
    with hub.marked("far/ticker") as v:
        v.paint(0, "attach", 0)
    return pid


def marked_scenario(hub, far, term, builds, label):
    """One marked stream while the line drops twice: the core there restarts
    and the line comes back by itself; then it restarts onto release 1, which
    holds the machine at needs_update until `machine update` puts release 2
    back. (The update above follows the hub's restart, which ends streams.)"""
    LOG.say(f"### {label}: a marked stream while the core there restarts")
    with hub.marked(term) as v:
        at = v.paint(0, "attach", 0)[1]
        # A tick a second: a ticker started just before needs as many seconds as the screen has rows.
        wait_for("it wrote more lines than the screen has", lambda: v.tick() > v.size[1] + 2, timeout=v.size[1] + 60)
        last = v.newest()
        far.restart()
        at = v.stitched(at, last)
        last = v.newest()
        far.restart(builds, 1)
        wait_for("far is needs_update (its core is release 1 now)", lambda: hub.state("far") == "needs_update",
                 timeout=90)
        time.sleep(3)
        held = [e["type"] for e in (v.gap(at) or (0, []))[1]]
        check(held == ["gap"], f"meanwhile the stream got one discontinuity and nothing after it ({held})")
        out = hub.human("machine update, a marked stream open", "machine", "update", "far")["output"]
        check("updated its remote core from release 1 to 2; its terminals kept running" in out,
              "machine update puts release 2 back")
        v.stitched(at, last)
        check(sum(e["type"] == "gap" for e in v.events()) == 2, "two discontinuities in all, one per lost line")


def install_scenario(hub, far, label, interactive=False):
    LOG.say(f"### {label}: add on an empty account")
    check(far.box.sh(f"test -e /home/{far.user}/.local", check_rc=False)[0] != 0, "the far account has no ~/.local")
    if interactive:
        out = flows.interactive_add(hub, far, label)
    else:
        e = hub.fails("hc_missing", "machine", "add", "far", "--name", "far")
        LOG.shown("machine add, nothing installed there (--json; in a terminal it asks to install)", json.dumps(e))
        check(hub.machine("far") is None, "the machine is not added")
        LOG.say(f"### {label}: add --install")
        out = hub.human("machine add --install (in a terminal: it also opens the shared ssh login)",
                        "machine", "add", "far", "--name", "far", "--install")
    check(out["status"] == 0 and "far: installed its remote core (release 1)" in out["output"],
          "it says it installed release 1")
    check(f"sudo loginctl enable-linger -- {far.user}" in out["output"],
          "it warns that programs stop at logout (no linger yet)")
    m = hub.machine("far")
    LOG.say(f"  right after add: {m['state']}; shared logins: {hub.masters()}")
    m = wait_for("far is connected", lambda: hub.state("far") == "connected" and hub.machine("far"), timeout=60)
    check(m["build"]["releaseSeq"] == 1, f"far is on release 1: {m['build']}")
    LOG.raw(json.dumps(m))
    ls = hub.human("machine ls", "machine", "ls")["output"]
    check(re.search(r"build 1 [0-9a-f]{12} e2e", ls), "machine ls shows the far build")
    mode = far.box.sh(f"stat -c '%a' {far.root} {far.root}/bin/nhc; ls -A {far.root}/bin; "
                      f"test -e /home/{far.user}/.local/bin && echo has-local-bin || true")[1].split()
    check(mode[:3] == ["700", "700", "nhc"] and "has-local-bin" not in mode,
          f"far: root and nhc are 0700, bin holds only nhc, no ~/.local/bin ({mode})")
    return m


def hub_onto(hub, seq, builds, label, restart):
    LOG.say(f"### {label}: the hub on release {seq}")
    restart(seq)
    check(hub.json("server", "status")["build"]["releaseSeq"] == seq, f"the hub core runs release {seq}")


def update_scenario(hub, far, pid, term, builds, label, restart):
    before = max(ticks(hub.nhc("read", term)[1]))
    hub_onto(hub, 2, builds, label, restart)
    wait_for("far is needs_update", lambda: hub.state("far") == "needs_update", timeout=60)
    hub.human("machine ls, needs_update", "machine", "ls")
    hub.fails("needs_update", "read", term, human="read on a needs_update machine")
    LOG.say(f"### {label}: machine update --hold")
    out = flow(hub, "machine update --hold", [HOLD], "machine", "update", "far", "--hold")
    flows.has(out, f"finding out what far runs... linux {far.box.arch}\n", "sending the remote core... ",
              ", arrived\nstarting it there... done\n", "waiting for the line... connected\n",
              "far: updated its remote core from release 1 to 2; its terminals kept running", "press Enter to close")
    check(out["status"] == 0, "it exits 0 after Enter")
    m = hub.machine("far")
    check(m["state"] == "connected" and m["build"]["releaseSeq"] == 2, "far is connected on release 2")
    check(far.build_info()["releaseSeq"] == 2, "far's installed nhc is release 2")
    check(hub.json("get", term)["pid"] == pid and far.alive(pid), f"{term} is still pid {pid}")
    first = max(ticks(hub.nhc("read", term)[1]))
    time.sleep(2.5)
    later = max(ticks(hub.nhc("read", term)[1]))
    check(later > first > before, f"and still writing (tick {first} -> {later})")
    out = hub.human("machine update, again", "machine", "update", "far")["output"]
    check("its remote core is already current (release 2)" in out, "a second update leaves it as it is")
    marked_scenario(hub, far, term, builds, label)


def downgrade_scenario(hub, far, builds, label, restart):
    hub_onto(hub, 1, builds, label, restart)
    wait_for("far is machine_newer", lambda: hub.state("far") == "machine_newer", timeout=60)
    hub.fails("machine_newer", "machine", "update", "far", human="machine update from an older hub")
    hub.fails("machine_newer", "machine", "add", "far", "--name", "far-again", "--install",
              human="machine add --install from an older hub")
    check(far.build_info()["releaseSeq"] == 2, "far still has release 2")
    hub_onto(hub, 2, builds, label, restart)
    wait_for("far is connected again", lambda: hub.state("far") == "connected", timeout=60)


# --- round a -------------------------------------------------------------------------------

def round_a(work, builds, arch, keys):
    label = f"a/{arch}"
    teardown_docker()
    run(["docker", "network", "create", "--label", LABEL, NETWORK])
    far_box = Box(f"nhc-e2e-far-{arch}", arch)
    hub_box = Box("nhc-e2e-hub", HUB)
    LOG.say(f"### {label}: machines")
    for box in (far_box, hub_box):
        about = box.sh("uname -sm; systemd --version | head -1; ssh -V 2>&1")[1]
        LOG.say(f"  {box.name}: " + about.replace("\n", "; "))
    pub = keys["client"].with_suffix(".pub").read_bytes()
    far_box.account("far", pub)
    far_box.account("odd", pub, profile="export HOME=/home/odd/elsewhere\n")
    far_box.account("taken", pub)
    far_box.sh("useradd -m mallory")
    far_box.sshd(keys[f"host-{arch}"])
    far = Far(far_box, "far", f"/home/far/.local/share/nearshell/{BUNDLE}")
    # The hub account: its own key, ssh config and known_hosts; a user manager (linger).
    hub_box.sh("useradd -m -s /bin/bash hub && loginctl enable-linger hub && install -d -m 700 -o hub -g hub "
               "/home/hub/.ssh /home/hub/bin")
    hub_box.put(keys["client"].read_bytes(), "/home/hub/.ssh/id_ed25519", "hub")
    host_pub = keys[f"host-{arch}"].with_suffix(".pub").read_text().split()
    hub_box.put(f"{far_box.name} {host_pub[0]} {host_pub[1]}\n".encode(), "/home/hub/.ssh/known_hosts", "hub")
    conf = "".join(f"Host {u}\n  HostName {far_box.name}\n  User {u}\n  StrictHostKeyChecking yes\n"
                   for u in ("far", "odd", "taken"))
    hub_box.put(conf.encode(), "/home/hub/.ssh/config", "hub")
    for seq in (1, 2):
        hub_box.copy(builds / f"remote-{seq}", f"/opt/remote-{seq}")
    hub_box.sh("chmod -R a+rX /opt/remote-1 /opt/remote-2")
    hub_root = f"/home/hub/.local/share/nearshell/{BUNDLE}"
    wait_for("the hub account's user manager runs", lambda: hub_box.sh(
        "test -S /run/user/1000/systemd/private", check_rc=False)[0] == 0)
    hub = LinuxHub(hub_box)

    def restart(seq):
        hub.nhc("server", "stop", check_rc=False)
        hub_box.sh(f"install -m 755 -o hub /opt/remote-{seq}/nhc-{ARCHES[HUB][0]} /home/hub/bin/nhc.new && "
                   f"mv /home/hub/bin/nhc.new /home/hub/bin/nhc && install -d -m 700 -o hub {hub_root} "
                   f"{hub_root}/config {hub_root}/config/herdr-core")
        config = f'remote_payloads = "/opt/remote-{seq}"\n'.encode()
        hub_box.put(config, f"{hub_root}/config/herdr-core/config.toml", "hub")
        hub.nhc("server", "start")

    hub_onto(hub, 1, builds, label, restart)
    status = hub.json("server", "status")
    check(status["build"]["config"] == f"dev:{BUNDLE}:remote", "the hub is a product remote build")
    home = far_box.sh("getent passwd far | cut -d: -f6")[1].strip()
    seen = hub_box.sh("ssh far 'echo $HOME'", user="hub")[1].strip()
    LOG.say(f"  far: account home {home}, an ssh session's $HOME {seen}")
    check(home == seen, "sshd gives the session the account database's home")

    stream = flows.Follower(hub, work / f"events-{label.replace('/', '-')}.jsonl")
    install_scenario(hub, far, label)
    m = hub.machine("far")
    LOG.say(f"  far keeps_running_after_logout={m.get('keeps_running_after_logout')} "
            f"logout_linger_user={m.get('logout_linger_user')}")
    far_socket = f"/tmp/nearshell-{far.uid}-{SHA8}/c.sock"
    pid = terminal_scenario(hub, far, label)
    far.check_services(far_socket, f"dev:{BUNDLE}:remote")

    LOG.say(f"### {label}: every login there ends, no linger")
    linger = far_box.sh("ls /var/lib/systemd/linger 2>/dev/null || true")[1].split()
    check("far" not in linger, "far has no linger (it cannot enable it itself: no polkit)")
    flows.logout(hub, far)
    gone = wait_for("far's user manager stops", lambda: far_box.sh(
        "loginctl show-user far", check_rc=False)[0] != 0, timeout=60)
    LOG.say(f"  far/ticker {'still runs' if far.alive(pid) else 'ended'}; far processes: "
            + far_box.sh("ps -o pid=,args= -u far || true")[1].strip().replace("\n", "; "))
    check(gone and not far.alive(pid), "without linger the terminal ends with the last login (as the warning says)")
    hub.nhc("server", "start")
    wait_for("the hub reaches far again", lambda: hub.state("far") == "connected")
    ended = hub.json("get", "far/ticker")
    LOG.say(f"  far/ticker there now: state {ended['state']}, reason {ended.get('reason')}")
    check(ended["state"] == "finished", "the hub lists far/ticker as finished")

    LOG.say(f"### {label}: linger on (the warning's command, as root)")
    far_box.sh("loginctl enable-linger far")
    hub.json("run", "--bg", "--on", "far", "--name", "ticker2", "--", "sh", "-c", TICKER)
    pid = hub.json("get", "far/ticker2")["pid"]
    wait_for("far/ticker2 writes", lambda: ticks(hub.nhc("read", "far/ticker2")[1]))
    update_scenario(hub, far, pid, "far/ticker2", builds, label, restart)
    far.check_services(far_socket, f"dev:{BUNDLE}:remote")

    LOG.say(f"### {label}: every login there ends, with linger")
    flows.logout(hub, far)
    time.sleep(15)  # logind's UserStopDelaySec is 10 s
    user = far_box.sh("loginctl show-user far -p State -p Sessions")[1].split()
    check("State=lingering" in user and "Sessions=" in user, f"far: {user}")
    check(far.alive(pid), f"far/ticker2 (pid {pid}) still runs")
    hub.nhc("server", "start")
    wait_for("the hub reaches far again", lambda: hub.state("far") == "connected")
    check(hub.json("get", "far/ticker2")["pid"] == pid, "the hub sees the same far/ticker")

    downgrade_scenario(hub, far, builds, label, restart)
    flows.rm_scenario(hub, far, pid, label, stream)
    flows.check_stream(stream.stop(), label)
    known = hub_box.sh("cat /home/hub/.ssh/known_hosts", user="hub")[1].encode()
    flows.trust_scenario(
        hub, far, label, ask=False,
        forget=lambda: hub_box.sh(f"ssh-keygen -q -R {far_box.name} -f /home/hub/.ssh/known_hosts", user="hub"),
        restore=lambda: hub_box.put(known, "/home/hub/.ssh/known_hosts", "hub"),
        end_logins=lambda: hub_box.sh(f'for m in /tmp/nearshell-1000-{SHA8}/ssh-*; do [ -S "$m" ] && '
                                      'ssh -S "$m" -O exit far; done; true', user="hub"))
    flows.side_checks(hub, far_box, far, far_socket, label)
    far.user_sh(f"{far.root}/bin/nhc server stop", check_rc=False)
    hub.nhc("server", "stop", check_rc=False)
    teardown_docker()


# --- round b -----------------------------------------------------------------------------

def round_b(work, builds, arch, keys):
    label = f"b/{arch}"
    if platform.system() != "Darwin":
        raise Failed("round b runs on macOS")
    teardown_docker()
    run(["docker", "network", "create", "--label", LABEL, NETWORK])
    box = Box(f"nhc-e2e-farb-{arch}", arch, publish=True)
    far_root = f"/home/far/.local/share/nearshell/{BUNDLE}"
    box.account("far", keys["client"].with_suffix(".pub").read_bytes())
    # A test build takes its root from NEARSHELL_HC_TEST_ROOT: sshd sets it to
    # the path the dial line uses.
    box.sshd(keys[f"host-{arch}"], "".join(f"Match User {u}\n    SetEnv NEARSHELL_HC_TEST_ROOT=/home/{u}/.local/share/"
                                           f"nearshell/{BUNDLE}\n" for u in ("far", "far2")))
    far = Far(box, "far", far_root, "test", f"NEARSHELL_HC_TEST_ROOT={far_root} ")
    port = box.port()
    # This Mac's side: a scratch ssh config, key and known_hosts; `ssh` on the
    # hub's PATH is /usr/bin/ssh reading only that config.
    b = pathlib.Path(tempfile.mkdtemp(prefix="ne2e-", dir="/var/tmp"))
    try:
        # No host key known yet: a person accepts it (ssh asks).
        (b / "known_hosts").write_text("")
        (b / "ssh_config").write_text(
            f"Host far\n  HostName 127.0.0.1\n  Port {port}\n  User far\n  IdentityFile {keys['client']}\n"
            f"  IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile {b}/known_hosts\n"
            "  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking ask\n  UpdateHostKeys no\n"
            "  PasswordAuthentication no\n  KbdInteractiveAuthentication no\n")
        shim = b / "shim"
        shim.mkdir()
        (shim / "ssh").write_text(f"#!/bin/sh\nexec /usr/bin/ssh -F {b}/ssh_config \"$@\"\n")
        (shim / "ssh").chmod(0o755)
        effective = run(["/usr/bin/ssh", "-G", "-F", b / "ssh_config", "far"])[1]
        conf = dict(line.split(" ", 1) for line in effective.splitlines() if " " in line)
        check(conf["identityfile"] == str(keys["client"]) and conf["identityagent"] == "none"
              and conf["userknownhostsfile"] == f"{b}/known_hosts",
              "ssh here uses only the scratch key and known_hosts, no agent")
        root = b / "hub"
        hub = MacHub(root, shim, builds / "hub-1/nhc")

        def restart(seq):
            if (root / "run/c.sock").exists():
                hub.owner("server", "stop")
            hub.binary = builds / f"hub-{seq}/nhc"
            hub.configure(builds / f"test-{seq}")
            hub.owner("server", "start")

        try:
            b_scenario(hub, far, root, builds, label, restart, work)

            def end_logins():
                for m in (root / "run").glob("ssh-*"):
                    run(["/usr/bin/ssh", "-F", b / "ssh_config", "-S", m, "-O", "exit", "far"], check_rc=False)
            flows.trust_scenario(hub, far, label, lambda: (b / "known_hosts").write_text(""), None, end_logins, True)
            flows.agent_scenario(hub, box, b, port, keys, label)
        finally:
            if (root / "run/c.sock").exists():
                run([hub.binary, "--owner-token", hub.token, "server", "stop"], env=hub.env, check_rc=False)
            for m in (root / "run").glob("ssh-*"):
                run(["/usr/bin/ssh", "-F", b / "ssh_config", "-S", m, "-O", "exit", "far"], check_rc=False)
            hubs = str(builds / "hub-")
            # The hub core and holders run from the builds (their command lines name them).
            wait_for("no hub core or holder is left on this Mac", lambda: run(
                ["pgrep", "-f", hubs], check_rc=False)[0] != 0, timeout=20)
            run(["pkill", "-f", str(b)], check_rc=False)
            far.user_sh(f"{far.env}{far_root}/bin/nhc server stop", check_rc=False)
    finally:
        shutil.rmtree(b, ignore_errors=True)
        teardown_docker()


def b_scenario(hub, far, root, builds, label, restart, work):
    hub_onto(hub, 1, builds, label, restart)
    check(hub.json("server", "status")["build"]["config"] == f"dev:{BUNDLE}:scratch-test", "the hub is managed-test")
    stream = flows.Follower(hub, work / f"events-{label.replace('/', '-')}.jsonl")
    install_scenario(hub, far, label, interactive=True)
    LOG.say(f"### {label}: machine login (the shared ssh login)")
    out = hub.human("machine login", "machine", "login", "far")
    check(out["status"] == 0, "machine login succeeds")
    masters = list((root / "run").glob("ssh-*"))
    check(len(masters) == 1 and masters[0].is_socket(), f"one ControlMaster socket: {[m.name for m in masters]}")
    hub.nhc("machine", "connect", "far", check_rc=False)
    pid = terminal_scenario(hub, far, label)
    far.check_services(f"{far.root}/run/c.sock", f"dev:{BUNDLE}:remote:scratch-test")
    update_scenario(hub, far, pid, "far/ticker", builds, label, restart)
    downgrade_scenario(hub, far, builds, label, restart)
    flows.rm_scenario(hub, far, pid, label, stream)
    check(not list((root / "run").glob("ssh-*")), "machine rm ended the shared login")
    flows.check_stream(stream.stop(), label)


# --- main ------------------------------------------------------------------------------------

def keygen(path, comment):
    if not path.exists():
        run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", comment, "-f", path])
    return path


def main():
    global LOG
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--work", type=pathlib.Path, required=True, help="a new or empty directory for logs and keys")
    parser.add_argument("--builds", type=pathlib.Path, help="where builds go and are reused (default: <work>/builds)")
    parser.add_argument("--cache", type=pathlib.Path, help="Cargo target directories (default: <builds>/cache)")
    parser.add_argument("--round", action="append", choices=["a", "b"], help="default: a, and b on macOS")
    parser.add_argument("--arch", action="append", choices=sorted(ARCHES), help="far end's (default: this host's)")
    parser.add_argument("--remove-images", action="store_true", help="drop the nhc-e2e images at the end")
    args = parser.parse_args()
    rounds = args.round or (["a", "b"] if platform.system() == "Darwin" else ["a"])
    flows.bind(sys.modules[__name__])
    arches = args.arch or [HUB]
    work = args.work.absolute()
    if work.exists() and any(p.name not in ("builds",) for p in work.iterdir()):
        sys.exit(f"e2e-remote: {work} is not empty")
    work.mkdir(parents=True, exist_ok=True)
    builds = (args.builds or work / "builds").absolute()
    builds.mkdir(parents=True, exist_ok=True)
    LOG = Log(work / "e2e.log")
    commit = run(["git", "-C", HERE.parent, "rev-parse", "HEAD"])[1].strip()
    if run(["git", "-C", HERE.parent, "status", "--porcelain", "--untracked-files=no"])[1].strip():
        LOG.say("note: tracked changes in this checkout are not in the build; it builds HEAD " + commit)
    LOG.say(f"e2e-remote: HEAD {commit}, rounds {rounds}, far ends {arches}, work {work}")
    (work / "keys").mkdir(mode=0o700)
    keys = {"client": keygen(work / "keys/client", "nhc-e2e-hub")}
    for arch in arches:
        keys[f"host-{arch}"] = keygen(work / f"keys/host-{arch}", "nhc-e2e-far")
    started, failed = time.time(), None
    try:
        build(builds, commit, arches, rounds, (args.cache or builds / "cache").absolute())
        for r in rounds:
            for arch in arches:
                (round_a if r == "a" else round_b)(work, builds, arch, keys)
                LOG.say(f"=== round {r}/{arch}: passed")
    except Exception as e:  # noqa: BLE001 -- any failure ends the run, and teardown follows
        failed = e
        LOG.raw(traceback.format_exc())
        LOG.say(f"FAILED: {e!r}" if not isinstance(e, Failed) else f"FAILED: {e}")
    finally:
        teardown_docker()
        if args.remove_images:
            for arch in ARCHES:
                run(["docker", "image", "rm", f"nhc-e2e:{arch}"], check_rc=False)
    (work / "user-visible.txt").write_text("".join(f"## {w}\n{t}\n\n" for w, t in LOG.visible))
    LOG.say(f"e2e-remote: {'FAILED' if failed else 'passed'} in {time.time() - started:.0f} s; log {work}/e2e.log")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
