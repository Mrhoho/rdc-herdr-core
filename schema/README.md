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
after its reply. Use framed mode for `terminal.attach` and the four upload
methods `terminal.put_begin`, `terminal.put_chunk`, `terminal.put_finish` and
`terminal.put_abort`. Other methods work in both modes.

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
`terminal.attach`. Omitted/false preserves the unmarked VT paint bytes. A
terminal on an added machine (`machine/t7`) works too: the core there makes the
marks and this core relays them in order with the bytes (see "Relayed marked
streams" below). That needs `paint-marks` in the hello of the core there as
well; without it the attach fails with `marks_local_only` (`the core on
<machine> does not support paint marks`) before anything is asked of that core,
instead of silently losing markers.

MARK payloads are JSON, on the attach channel, interleaved with DATA in one queue:

- `{"type":"paint_begin","reason":"attach|resync|stitch","paintGen":1,"cols":80,"rows":24}`
- `{"type":"paint_end","paintGen":1}`
- `{"type":"discontinuity","droppedBytes":123}`

The reason is one of the three strings (not the literal string with `|`). The
generation increases with every paint of one attach stream, across a relay's
reconnects too, and only the stream's first paint reads `attach`; a new attach
starts a new stream and generation.
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

**Relayed marked streams.** A marked stream on a terminal of an added machine
stays open when the line to that machine drops and comes back (the core there
restarted or was updated, the network failed). The viewer reads one
`discontinuity`, made by the relay, and no DATA after it until the next paint;
when the line is back, a paint with reason `stitch` and a larger `paintGen`,
then live output. A viewer that had not been painted yet gets no
`discontinuity`, and its first paint reads `attach`. The relay attaches there
again with the same `history`, which bounds every paint of a marked stream, the
stitched one included. While the machine is held (for example `needs_update`)
the stream waits after its `discontinuity` until the line is back, the machine
is removed or the client closes it; if the core there no longer lists
`paint-marks` when the line is back, the stream closes. A `discontinuity` from
the core there makes the relay drop what the viewer has not read and pass on
one `discontinuity` whose `droppedBytes` adds what it dropped. The relay
counts only bytes it dropped unread, so `droppedBytes` is a lower bound: `0`
does not mean nothing was lost (output written while the line was down is not
counted). A relayed marked stream gets no RESIZE, and its reply has
`resize_notifications: false` whatever was asked: the grid is in each
`paint_begin`. One whose backlog on the relaying core passes 4 MiB of DATA or
1024 queued marks closes with `paint_limit`, and its stream on the core there
is closed too; a viewer that reads and credits as it goes stays far below
that.

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

### Machines (`machine-events` fork feature)

A subscription with no selection at all (no `terminal`, `children_of`,
`labels` or `docs`) also hears of the machines this core added. Its reply and
every `missed` it gets carry `machines`: each one's `MachineView`, as
`machine.list` shows it. A change to a machine's view is a `machine` event:

```
{"jsonrpc":"2.0","method":"event","params":{"seq":44,"type":"machine","data":{"id":"…","name":"rohan","state":"connected","synced":true,…}}}
{"jsonrpc":"2.0","method":"event","params":{"seq":51,"type":"machine","data":{"id":"…","name":"rohan","removed":true}}}
```

`data` is the whole view (replace it, by `id`), or `{id, name, removed:
true}` once the machine is forgotten. It comes when the machine is added,
renamed or removed, and when its `state`, `why`, `synced`, `version`,
`build`, `features` or logout fields change; `since_ms` changing alone is no
event (it only moves with the state). Nothing else is told: the 3-second
status poll that refreshes the logout fields is silent while they stay the
same. A machine's event comes before any event of its terminals that the
same change causes: added before its terminals' `created`, renamed (same
`id`, new `name`) before their `removed` under the old name and `created`
under the new one, removed before their `removed`. Filtered subscriptions,
cores that dialed in, and waits inside the core hear none of this, and a
machine that is itself a hub does not pass its own machines on.

A machine speaks only for its own terminals. A record or terminal event it
sends whose address is not under its name here (`rohan/t3`) is dropped: it is
not listed, waited on or told to anyone, so it can neither stand in for a
terminal here or on another machine nor remove one. Any other address in what
it sends that is not one of its own becomes null, except `parent` naming a
terminal here that started a run on that machine. A `record` it puts in an
event's data is dropped: subscribers here get the record this core keeps. `machine.connect`,
`machine.rename` and `machine.rm` take `machine` as the name or the `id`. A
name is never 32 hex digits nor another machine's id: `machine.add` and
`machine.rename` fail with `invalid_params` for one.

`synced: true` means this core's terminals for that machine are exactly the
ones there now. It is false while the line is down, held or connecting, after
this core starts (it knows none of them until the line is up), and while a
snapshot from there is being applied: each time the line comes up, and each
time that core says this one missed events, `synced` turns false, the
changes arrive as `created`/`removed`, then a `machine` event says `synced:
true`. Until then a terminal missing here may still exist there. A refused
subscription there (an error reply) changes nothing here: the line serves
calls, the terminals stay as they were, `synced` stays false, and it asks
again after 2, 4, 8, 16, then 30 s.

States (`state`): `connecting`, `connected`, `needs_login` (a password or code
only a person can give), `host_key_unknown` (ssh has no host key for it and
nobody to accept one: `machine login` in a terminal shows it), `host_key_changed`
(it differs from the one known; nothing here accepts it — check why, then fix
known_hosts yourself), `needs_update`, `different_core` (another core answers
there: its state was wiped or it was reinstalled; `why` names it), and
`unreachable`; Nearshell builds add `machine_newer`, `build_conflict` and
`incompatible`. The two host-key states are dialed again as slowly as
`needs_login` (2, 5, then every 10 minutes) and at once by `machine.connect`;
`different_core` as `unreachable`. `machine.add` fails with `needs_login`,
`host_key_unknown` or `host_key_changed` for the same reasons. ssh's own
words decide the host-key states; anything else it prints stays `unreachable`.

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
parent and label filters exclude doc events. A subscription may also name the
documents it wants — `docs: ["name", "prefix*"]`, where a name ending in `*`
is a prefix — and hears theirs; naming narrows, as it does in every other
dimension. Naming documents alone gives an empty terminal list in the initial
snapshot and every `missed` snapshot. Naming documents and terminal filters
receives both kinds of events. The event carries no value;
call `doc.get` to read it, using `on` for another machine.

Subscribe before reading. Read documents after subscribing or reconnecting,
a `missed` notification, or a remote machine reconnect: snapshots contain
terminals only, not documents, and changes while disconnected are not replayed.
A remote stream's `missed`, and each successful establishment of its event
subscription (including the first connection), are reported locally as `missed`
to unfiltered and document subscribers, using the local sequence. Refetch documents on all
watched machines after it. Such a `missed` has `documents: true`: every
terminal event before it was delivered (its snapshot is current, as in any
`missed`), so a client may keep its terminal state and only refetch
documents. Without the mark it is a real `missed`: events were dropped
(this one too, if it did not fit in your queue) and the snapshot replaces
what you had.

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
full layered color replay requires the two features above. On a machine's
terminal the core there makes `colors` and the relay passes it on unchanged;
the relay checks only `paint-marks` there.

Color queries are answered once by core and removed from the viewer stream;
setters in a mixed set/query OSC still reach viewers. Batch OSC 4 queries use
the vendor's existing 8 MiB OSC capture bound instead of the relay's generic
512-byte heuristic. Reset/set operations themselves produce no reply. DA1
and DA2 retain Ghostty's `CSI ?62;22c` and `CSI >1;10;0c`; XTVERSION reports the
terminal's stored identity derived from TERM_PROGRAM/TERM_PROGRAM_VERSION,
not the core binary's command name. XTGETTCAP remains viewer-owned.
