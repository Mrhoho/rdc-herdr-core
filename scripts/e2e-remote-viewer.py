"""The viewers of scripts/e2e-remote.py, run where the hub is: a person's
terminal (PTY), and the App's attach: one `terminal.attach {marks: true}`
stream held open across several steps of the script, credited as it is read
and checked against the guards the App's parser applies to it."""
import json
import re
import subprocess
import threading


class Failed(Exception):
    pass


# A command in a pseudo-terminal, as a person runs it (nhc prints its human
# output only to a terminal). With a detach delay it is a viewer: what
# `attach` shows, then ctrl+\ detaches.
PTY = r"""
import fcntl, json, os, pty, select, signal, struct, sys, termios, time
detach, argv = float(sys.argv[1]), sys.argv[2:]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
out, start, sent, status = b"", time.time(), False, None
while time.time() - start < 120:
    if detach and not sent and time.time() - start > detach:
        os.write(fd, b"\x1c")
        sent = True
    if select.select([fd], [], [], 0.2)[0]:
        try:
            data = os.read(fd, 65536)
        except OSError:
            data = b""
        out += data
    done, code = os.waitpid(pid, os.WNOHANG)
    if done:
        status = os.waitstatus_to_exitcode(code)
        break
if status is None:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
print(json.dumps({"status": status, "output": out.decode("utf-8", "replace")}))
"""

# Runs where the hub's socket is: hello with framed: true, terminal.attach as
# a frame on channel 0 ([channel u32][kind u8][len u32]), then a JSON line on
# stdout for every frame, CREDIT back for every DATA. It ends at EOF on stdin.
PROGRAM = r"""
import json, select, socket, struct, sys, time
def out(**frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.sendall((json.dumps({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"framed": True}}) + "\n").encode())
buf = b""
while b"\n" not in buf:
    buf += s.recv(65536) or sys.exit("no hello")
line, buf = buf.split(b"\n", 1)
out(kind="hello", hello=json.loads(line))
body = json.dumps({"jsonrpc": "2.0", "id": 2, "method": "terminal.attach", "params": json.loads(sys.argv[2])}).encode()
s.sendall(struct.pack("<IBI", 0, 0, len(body)) + body)
ch, end = None, time.time() + 1800
while time.time() < end:
    ready = select.select([s, sys.stdin], [], [], 1)[0]
    if sys.stdin in ready and not sys.stdin.readline():
        break
    if s not in ready:
        continue
    chunk = s.recv(65536)
    if not chunk:
        out(kind="eof")
        break
    buf += chunk
    while len(buf) >= 9 and len(buf) >= 9 + struct.unpack("<I", buf[5:9])[0]:
        channel, kind, size = struct.unpack("<IBI", buf[:9])
        payload, buf = buf[9:9 + size], buf[9 + size:]
        if channel == 0:
            msg = json.loads(payload)
            if msg.get("id") == 2:
                ch = msg.get("result", {}).get("channel")
                out(kind="reply", reply=msg)
        elif channel != ch:
            out(kind="stray", channel=channel, frame=kind)
        elif kind == 0:
            s.sendall(struct.pack("<IBII", ch, 1, 4, len(payload)))
            out(kind="data", bytes=len(payload), text=payload.decode("utf-8", "replace"))
        elif kind == 6:
            out(kind="mark", mark=json.loads(payload))
        else:
            out(kind={2: "close", 3: "resize"}.get(kind, f"kind {kind}"), text=payload.decode("utf-8", "replace"))
"""


def ticks(text):
    return [int(n) for n in re.findall(r"tick (\d+)", text)]


class Viewer:
    """`argv` runs PROGRAM with the attach's `params`; `frames` is what it
    has read so far. `say`, `check` and `wait_for` are the script's."""

    def __init__(self, argv, params, say, check, wait_for):
        self.size, self.frames = (params["cols"], params["rows"]), []
        self.say, self.check, self.wait_for = say, check, wait_for
        self.proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        for line in self.proc.stdout:
            try:
                self.frames.append(json.loads(line))
            except ValueError:
                self.frames.append({"kind": "output", "text": line.decode(errors="replace").rstrip()})

    def __enter__(self):
        return self

    def __exit__(self, kind, error, tb):
        self.stop()
        self.say(f"  marked stream{' (failed)' if kind else ''}: {summary(self.frames)}")

    def stop(self):
        if self.proc.poll() is None:
            try:
                self.proc.stdin.close()
                self.proc.wait(10)
            except (OSError, subprocess.TimeoutExpired):
                self.proc.kill()
                self.proc.wait()
        self.reader.join(5)

    def frame(self, kind):
        return next((f[kind] for f in list(self.frames) if f["kind"] == kind), None)

    def events(self):
        return follow(list(self.frames))

    def newest(self):
        return max(e["paintGen"] for e in self.events() if e["type"] == "paint")

    def tick(self):
        return max([0] + [t for e in self.events() if e["type"] == "live" for t in ticks(e["text"])])

    def gap(self, at):
        """The first discontinuity since event `at`: its index and the events from it on."""
        events = self.events()
        kinds = [e["type"] for e in events[at:]]
        return (at + kinds.index("gap"), events[at + kinds.index("gap"):]) if "gap" in kinds else None

    def painted(self, since, reason):
        """The first paint after event `since` once it has ended and live
        output newer than it has followed: (paint, events since), else None."""
        events = self.events()[since:]
        paint = next((e for e in events if e["type"] == "paint"), None)
        if not paint or not paint["ended"] or not ticks(paint["text"]):
            return None
        after = events[events.index(paint) + 1:]
        newest = max(ticks(paint["text"]))
        live = [t for e in after if e["type"] == "live" for t in ticks(e["text"])]
        return (paint, events) if any(t > newest for t in live) else None

    def paint(self, since, reason, after):
        """The first paint after event `since`: `reason`, a paintGen above
        `after`, DATA, its end, then newer output. An attach: also the reply,
        the grid and the default colors. Returns it and the next event's index."""
        check = self.check
        if reason == "attach":
            reply = self.wait_for("the marked attach is answered", lambda: self.frame("reply"))
            check("result" in reply, "it is accepted" + ("" if "result" in reply else f": {json.dumps(reply)[:300]}"))
            check(reply["result"].get("resize_notifications") is False, "its reply has resize_notifications: false")
        paint, events = self.wait_for(f"a paint ({reason}) with DATA, its paint_end, then live output",
                                      lambda: self.painted(since, reason), timeout=90)
        check(paint["reason"] == reason and paint["paintGen"] > after,
              f"its paint_begin: reason {paint['reason']}, paintGen {paint['paintGen']} (after {after})")
        if reason == "attach":
            check((paint["cols"], paint["rows"]) == self.size, f"its grid is the terminal's, {self.size}")
            if "display-palette" in self.frame("hello")["result"]["features"]:
                colors = paint.get("colors") or {}
                check(len(colors.get("palette", [])) == 256 and type(colors.get("dark")) is bool,
                      "paint_begin.colors: 256 palette entries and dark (display-palette)")
        return paint, since + len(events)

    def stitched(self, at, last):
        """After the line dropped: one discontinuity since event `at`, nothing
        after it until a stitch paint above `last` with history beyond its
        rows, then live output. Returns the next event's index."""
        g = self.wait_for("a discontinuity", lambda: self.gap(at), timeout=90)[0]
        paint, end = self.paint(g + 1, "stitch", last)
        gaps = [e["droppedBytes"] for e in self.events()[at:end] if e["type"] == "gap"]
        self.check(len(gaps) == 1, f"one discontinuity before it (droppedBytes {gaps})")
        seen = ticks(paint["text"])
        self.check(min(seen) < max(seen) - self.size[1], f"the stitched paint has history beyond its "
                   f"{self.size[1]} rows (tick {min(seen)} to {max(seen)})")
        return end


def follow(frames):
    """The App's guards: no DATA before the first paint or between a
    discontinuity and the next paint, generations rising, `attach` only
    first, each end matching its begin; no RESIZE, CLOSE or anything else.
    Raises Failed at the first frame that breaks one. Returns paints (their
    DATA as `text`), gaps and runs of live output, in order."""
    events, last, painting, gap = [], 0, None, False
    for i, f in enumerate(frames):
        kind, m = f["kind"], f.get("mark", {})
        what = m.get("type") if kind == "mark" else kind
        bad = None
        if kind in ("hello", "reply"):
            continue
        if kind == "data" and (not last or gap):
            bad = "DATA outside a paint"
        elif kind == "data" and painting is not None:
            events[-1]["text"] += f["text"]
        elif kind == "data":  # one event each: indexes stay put as frames arrive
            events.append({"type": "live", "text": f["text"]})
        elif what == "discontinuity":
            if not last or type(m.get("droppedBytes")) is not int:
                bad = "a discontinuity before any paint, or without droppedBytes"
            painting, gap = None, True
            events.append({"type": "gap", "droppedBytes": m.get("droppedBytes")})
        elif what == "paint_begin":
            gen = m.get("paintGen")
            if painting is not None or type(gen) is not int or gen <= last:
                bad = "a paint_begin inside a paint or without a rising paintGen"
            elif (m.get("reason") == "attach") != (last == 0):
                bad = "attach is not (only) the first paint's reason"
            else:
                painting, last, gap = gen, gen, False
                events.append(dict(m, type="paint", text="", ended=False))
        elif what == "paint_end":
            if painting is None or m.get("paintGen") != painting:
                bad = "a paint_end that does not match the paint_begin"
            else:
                events[-1]["ended"], painting = True, None
        else:
            bad = "not a frame a marked stream has"
        if bad:
            raise Failed(f"marked stream, frame {i}: {bad}: {json.dumps(f)[:300]}")
    return events


def summary(frames):
    """The frames in a line: marks as they read, DATA runs counted."""
    parts, run = [], None
    for f in frames:
        if f["kind"] == "data":
            run = run or [0, 0]
            run[0], run[1] = run[0] + 1, run[1] + f["bytes"]
            continue
        if run:
            parts.append(f"DATA x{run[0]} ({run[1]} B)")
            run = None
        m = f.get("mark", {})
        if f["kind"] == "hello":
            continue
        if f["kind"] == "reply":
            parts.append("reply" if "result" in f["reply"] else f"error {json.dumps(f['reply'].get('error'))}")
        elif m.get("type") == "paint_begin":
            parts.append(f"begin {m.get('reason')} {m.get('paintGen')} {m.get('cols')}x{m.get('rows')}")
        elif m.get("type") == "paint_end":
            parts.append(f"end {m.get('paintGen')}")
        elif m.get("type") == "discontinuity":
            parts.append(f"discontinuity droppedBytes={m.get('droppedBytes')}")
        else:
            parts.append(json.dumps(f)[:200])
    if run:
        parts.append(f"DATA x{run[0]} ({run[1]} B)")
    return " | ".join(parts) or "nothing"
