# herdr-core API

`hc.openrpc.json` is the whole request/response API as an
[OpenRPC](https://open-rpc.org) document: every method with its params,
result and errors in JSON Schema. It is generated from the Rust types (a
test fails when it is stale), and `hc schema` or the `rpc.discover` method
returns the same document from a running core.

This file covers what OpenRPC does not describe: the two connection modes,
the attach stream, and event notifications.

Status: preview (protocol generation 1, version 0.x).

The API only grows. `released.openrpc.json` is the API as last released;
a test fails on any change that would break a client of it: a removed
method, param or result field, a new required input, an input that accepts
less, or an answer that can carry a new kind of value or loses its type.
At each release, copy `hc.openrpc.json` over it. A real break needs a new
protocol generation, decided on purpose.

## Connecting

On Unix, core listens on a Unix socket; on Windows, it uses a named pipe.
See [endpoint rules](../docs/security.md#state-and-endpoints) for default paths,
fallbacks, and overrides. `hc connect` speaks the same protocol over
stdin/stdout, so `ssh box hc connect` is a remote core.

A connection starts in **plain mode**: JSON-RPC 2.0, one message per line.
Any language or `socat` works.

```
→ {"jsonrpc":"2.0","id":1,"method":"terminal.list","params":{}}
← {"jsonrpc":"2.0","id":1,"result":{"terminals":[...],"hidden_finished":0,"machines":[]}}
```

Errors are JSON-RPC errors with a stable kind and a hint:

```
{"code":-32000,"message":"no terminal t9","data":{"kind":"not_found","hint":"..."}}
```

`hello` with `{"framed": true}` switches the connection to **framed mode**
after its reply. Attach needs it; everything else works in both modes.

## Framed mode

Every frame: `[channel u32 LE][kind u8][len u32 LE][payload]`, payload at
most 16 MiB.

| kind | name | payload |
|---|---|---|
| 0 | DATA | channel 0: one JSON-RPC message; other channels: stream bytes |
| 1 | CREDIT | u32 LE: bytes the receiver consumed |
| 2 | CLOSE | the channel is over; from the core, a JSON reason |
| 3 | RESIZE | u16 LE cols, u16 LE rows: viewer → core, core → observer |
| 4 | FOCUS | viewer → core: 1 gained focus, 0 lost it |
| 5 | FACTS | viewer → core: JSON display facts (`Display` in the schema) |
| 6 | AUTOMATIC_INPUT | viewer → core: terminal-generated reply bytes, not human activity |
| 6 | MARK | core → viewer: ordered paint metadata (opt-in, see below) |

Channel 0 carries JSON-RPC both ways. Calls on it run side by side.

### Attach streams

`terminal.attach` returns a `channel`. On it the core sends the first paint
(the current screen, optional history, the latest status report and title,
and `CSI ?9001h` or `l`: whether the program asked for win32-input-mode,
as Windows' ConPTY does when it starts), then the program's raw output,
minus terminal queries the core already answered. The core never encodes
keys: a viewer that honors 9001 sends key events in that format itself.
The viewer sends keys as DATA on the same channel, and RESIZE,
FOCUS and FACTS as it changes. The last active viewer sets the size
(active: it attached, typed, resized or gained focus); `observe` viewers never type or resize.
Automatic query answers (including mode 2048 reports, kitty graphics and XTGETTCAP
replies) use AUTOMATIC_INPUT, not DATA. Their bytes reach the program unchanged,
including duplicates from multiple viewers, but do not take size/display ownership
or reset the human quiet period. They follow the same input serialization as DATA.
Tag replies at their source, never by inspecting their bytes; arbitrary splits are valid.
Cores advertise `automatic_input` in `hello.features`; `terminal.attach` confirms
`automatic_input: true` only for a supporting path. Clients must require both before
sending kind 6; absent/false means send DATA (legacy activity behavior). Old cores
ignore unknown local frame kinds, so sending kind 6 without negotiation loses input.
Relays preserve the tag and close a previously capable stream if support is lost
on reconnect; attach again to negotiate the fallback. Untagged DATA is unchanged.
Observers may opt in with `resize_notifications: true` on `terminal.attach`:
the client must apply incoming RESIZE before consuming subsequent DATA.
The reply confirms `resize_notifications: true` only when every core on the path
supports it. False or an absent field means the client must track size through
size events instead; no replacement paints are injected for this request.
Cores advertise `resize_notifications` in both hello requests and replies, so an
older intermediate relay cannot silently drop the grid announcements. If a
previously capable path loses support on reconnect, its stream closes; attach
again to negotiate the fallback.
These confirmed observers receive the terminal's actual grid before their first paint.
When that grid changes, pending output is superseded by RESIZE followed by an
authoritative paint (starting with CAN to cancel any unfinished sequence).
Size/paint pairs share stream ordering and wait for DATA credit; RESIZE itself
does not consume credit. Intermediate unsent grids are coalesced, and a paint is
serialized only when the observer has credit. The attach reply precedes stream
frames, including on core-to-core relays and reconnects.
Without this opt-in, existing event-driven observers keep their previous output
behavior: no additional resize paints or RESIZE frames.

Flow control: a stream starts with 256 KiB of credit. The viewer sends
CREDIT for the bytes it has written out. A viewer that falls more than
4 MiB behind gets a fresh repaint instead of the backlog. A stalled stream
never blocks channel 0 or other streams.


### Ordered paint marks (fork extension)

Check `hello.features` for `paint-marks`, then pass `marks: true` to
`terminal.attach`. This is available on a **direct connection to the terminal's
core**; a machine-relayed marked attach returns `marks_local_only` instead of
silently losing markers. Omitted/false preserves the unmarked VT paint bytes.

MARK payloads are JSON, on the attach channel, interleaved with DATA in one queue:

- `{"type":"paint_begin","reason":"attach|resync|stitch","paintGen":1,"cols":80,"rows":24}`
- `{"type":"paint_end","paintGen":1}`
- `{"type":"discontinuity","droppedBytes":123}`

The reason is one of the three strings (not the literal string with `|`). The
generation increases per attach; reconnect starts a new stream and generation.
The begin grid belongs to that frozen paint. A marked viewer's actual grid change
invalidates the old transaction and sends a resync at the new size. MARK consumes
**no DATA credit**, but cannot jump over pending DATA. Process both frame kinds
in the same parser worker. Reset the VT parser on discontinuity, accept only a
matching end, and suppress external VT effects throughout replay. The paint tail
restores the captured partial parser sequence, **after** 9001/7501 metadata, so
subsequent live UTF-8/CSI/OSC bytes can complete it correctly.

Overflow drops queued DATA and MARK together and enqueues discontinuity, begin,
lazy DATA, end. A partially sent old transaction has no end; discontinuity aborts
it. `droppedBytes` counts discarded generated DATA and suppressed live bytes;
not-yet-generated paint has no byte count. An already handed-off socket frame
precedes the discontinuity and is not counted as discarded. Replacing an active
paint protects the new paint: further live bytes are suppressed and counted in
one deferred resync, captured after that paint completes. This bounds retries
under continuous output. Stitch counts its suppressed bytes and uses the same
ordered enqueue; marked stitch invalidates pending old paint. Upgrade recovery
uses an ordinary attach transaction.

Paint generation is lazy (at most 16 KiB per credit permit) over an immutable
snapshot, including when the paint is larger than 4 MiB. The shared config key
`viewer_pending_bytes` defaults to 67108864 (64 MiB), read at server start. It
accounts for queued payload/storage, frozen snapshots, decoded Ghostty clones,
producer chunks and reserved worker stacks. It is a pending-memory budget, not
an RSS limit: canonical screens, existing control queues, kernel buffers and
one in-flight wire frame (at most 16 KiB plus header per connection) are separate.
If a source/queue/worker cannot fit, that stream closes with `paint_limit`; its
terminal keeps running. A client may retry with less history or fewer viewers.
`server.status.viewer_pending` reports limit/used/peak/denied, generated paint
bytes and active paint workers. Closing a connection cancels its producers and
reclaims their allocations without waiting for credit.

A viewer detaches by sending CLOSE; the core answers with an empty CLOSE.
When the program ends, the core sends CLOSE with
`{"reason":"exited","exit_code":0,"signal":null}` (`reason` as in the
terminal record: exited, killed, server_stop, ...).

## Events

`events.subscribe` replies with a snapshot (`seq`, matching terminals) and,
in framed mode, a channel; in plain mode the connection then carries only
notifications. Notifications are JSON-RPC messages without an id:

```
{"jsonrpc":"2.0","method":"event","params":{"seq":42,"type":"status","terminal":"t7","data":{...}}}
{"jsonrpc":"2.0","method":"heartbeat","params":{"seq":42}}
{"jsonrpc":"2.0","method":"missed","params":{"seq":57,"terminals":[...]}}
```

Each event's `data`, and how to apply it to a snapshot:

| type | data | apply |
|---|---|---|
| `created` | the whole `Terminal` record | add it |
| `lost` | `{reason, exit_code, signal, finished_ms: null}` | kept terminal remains `live`; set these, clear `pid` and drop `foreground` |
| `restarted` | the whole `Terminal` record | replace it; reattach to the new shell |
| `keep` | `{keep}` | set `keep` |
| `finished` | `{reason, exit_code, signal, finished_ms}` | set `state: "finished"` and these, and drop `foreground`; status is cleared just before (a `status` event) |
| `removed` | `{}` | drop it |
| `status` | `{status, records, reported}`: the root status (null when cleared), every record by key, whether it ever reported | replace all three |
| `turn` | `{turn, state}`: the turn counter after a turn ended, and the word it ended on | set `turn` |
| `title` | `{title}` | set `title` |
| `labels` | `{labels}`: all labels now; plus `record` (the whole `Terminal`) when it newly matches your filter | replace `labels`; with `record`, add it; with a label filter, a terminal that no longer matches leaves your view (you still get this event) |
| `size` | `{cols, rows}` | set both |
| `foreground` | `{pid, name, argv, cwd}`, or null: what runs in it now (the foreground process group's leader) and its folder; read about twice a second | set `foreground` |

Terminals on machines this core added are in the same stream, addressed as
seen from here (`rohan/t7`), in the same order and filters (`terminal`,
`children_of`, labels). When a machine's line comes back, its terminals that
changed meanwhile arrive as `created` again (replace the record) and those
that went as `removed`; forgetting the machine removes its terminals.

`run --keep` or `terminal.keep {terminal, keep}` keeps a lost terminal live
without a program until this machine's shell can start in its last observed
folder (then its start folder, then home). The original command is never
re-run. Loss closes existing attach streams with its reason; wait for
`restarted` and attach again. An exit with a code and explicit kill stay final.
Death by signal without an exit code is loss (`signaled`) for kept terminals;
non-kept behavior is unchanged. Windows reports no signals, so `signaled` never
fires there: native termination normally reports an exit code. A replacement
killed by a shutdown signal is lost again instead of closing its record.
Repeated failures back off by 1.5, 3, 6, 12, 30, then 60 seconds forever; a
program that lived at least 10 seconds resets the delay. Recovery
runs on every OS after a grace delay, but not while the core stops/restarts
or Windows reports session shutdown. During recovery, send/keys/interrupt/attach
return `unavailable`: "lost, waiting for its new shell"; retry after `restarted`.
Server stop parks kept terminals; the next core start gives them fresh shells.
Waits end on `lost` as on `finished`: they follow the program, not the kept record.

An initial spawn that times out remains claimed with `reason: "spawn_pending"`;
input/attach are unavailable until the holder answers. It may already be running,
so core does not clear the claim or start another program on a guess. Explicit
kill cancels it; a core restart reconciles the holder. A late spawn failure clears
the claim and retries if kept, or finishes if keep was disabled.

`reason` on a finished terminal: `exited`, `killed`, `server_stop`,
`host_shutdown`, `holder_lost`, `stopped_by_restart`. Kept live records may
also have `reason: "signaled"` while waiting for a new shell. `seq` rises by one per event, in order. A `heartbeat`
arrives every 15 s with the current `seq` (when no events are queued for you). A subscriber that falls behind
gets `missed` with a fresh snapshot: drop events whose `seq` is not above
it. `seq` restarts with each core process; take a fresh snapshot after
reconnecting.

## Documents

`doc.get {name, on?}` reads opaque JSON and its version. An unwritten name
returns `{value: null, version: 0}`. `doc.set {name, value, version, on?}`
writes only if the version matches, returning the new version. A stale write
fails with kind `conflict` and the current version in its message; read again
before retrying. Writing null deletes the value but retains the version, so
an old writer cannot recreate it unnoticed.

Names follow the terminal name rules. Each value is limited to 1 MiB of
serialized JSON. Documents persist atomically in `docs.json` in core state,
survive restart and handover, and disappear when that state is wiped. Core
never interprets their contents. A broken document file is warned about and
renamed to `docs.json.bad`; the core starts with empty documents rather than
making terminals unavailable. These methods are API-only, with no CLI verb.

Unfiltered event subscribers receive changes as:

```
{"jsonrpc":"2.0","method":"event","params":{"seq":43,"type":"doc","data":{"name":"herdr-app","version":2}}}
```

There is no `terminal` on a doc event. Changes on an added machine arrive in
the same stream with `machine: "rohan"` (this core's name for it). Terminal,
parent and label filters exclude doc events. The event carries no value;
call `doc.get` to read it, using `on` for another machine.

Subscribe before reading. Read documents after subscribing or reconnecting,
a `missed` notification, or a remote machine reconnect: snapshots contain
terminals only, not documents, and changes while disconnected are not replayed.
A remote stream's `missed`, and each successful establishment of its event
subscription (including the first connection), are reported locally as `missed`
to unfiltered subscribers, using the local sequence. Refetch documents on all
watched machines after it.

## Status

A terminal's status is whatever its program reported through OSC 7501, the
Program Status Protocol (`idle`, `working`,
`done`, `blocked`, `error`, ...), stored as reported. hc never infers status
from the screen. Programs that don't speak 7501 have no status.

### Display colors (`display-palette` fork feature)

`Display` (attach's `display`, or a FACTS payload) accepts:

```json
{"fg":"#dddddd","bg":"#111111","cursor":"#ffffff",
 "palette":{"0":"#000000","1":"#cc2244","255":"#eeeeee"},"dark":true}
```

`palette` is an optional sparse map with decimal keys 0–255 and `#rrggbb`
values. A supplied map replaces the theme over the built-in palette; `{}`
restores the built-in defaults. Omission/null keeps the previously known
palette, just as omitted fg/bg/cursor keep previously known facts. Invalid
palette entries reject attach parameters; invalid FACTS are ignored before
viewer activity or display state changes. Only the active typing viewer's
facts apply (observe does not change terminal facts).

Program OSC overrides win over Display defaults, which win over built-in
colors. OSC 104 (one index or all) and OSC 110/111/112 reset the override to
the current default layer. `dark` explicitly overrides the preference inferred
from the Display background. Omission/null removes that explicit override;
the existing background luminance rule applies. Program OSC 11 changes the
program color override, not the host's dark/light preference. Core answers
CSI ?996n; the viewer remains responsible for 2031 change notifications.

Clients requiring color-correct replay negotiate both `paint-marks` and
`display-palette`, and use `marks:true`. **Before processing paint DATA**, the
same parser worker applies `paint_begin.colors` as **defaults**, not OSC
setters. This extra field has the following shape:

```text
colors: {fg: [r,g,b] | null, bg: [r,g,b] | null,
         cursor: [r,g,b] | null, palette: [[r,g,b], ... 256 entries], dark: bool}
```

The following VT paint resets stale overrides and restores only actual program
OSC overrides, before any indexed-color text. An override equal to the previous
default is still an override. The default layer must remain distinct so future
FACTS/theme changes and resets work correctly. Effective default changes queue
an ordered resync for marked viewers, including changes with no resize. These
snapshots and marks use P1's existing shared budget, cancellation and CREDIT
ordering. Unmarked legacy streams keep their original paint representation;
full layered color replay requires the two features above. Direct core
connections are required (P1's machine-relay limitation still applies).

Color queries are answered once by core and removed from the viewer stream;
setters in a mixed set/query OSC still reach viewers. Batch OSC 4 queries use
the vendor's existing 8 MiB OSC capture bound instead of the relay's generic
512-byte heuristic. Reset/set operations themselves produce no reply. DA1
and DA2 retain Ghostty's `CSI ?62;22c` and `CSI >1;10;0c`; XTVERSION reports the
terminal's stored identity derived from TERM_PROGRAM/TERM_PROGRAM_VERSION,
not the core binary's command name. XTGETTCAP remains viewer-owned.
