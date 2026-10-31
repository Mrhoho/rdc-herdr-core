# Nearshell 构建与 CLI 契约

状态：A1–A5 已实现；发行前须通过锁定源码构建与组 A 契约矩阵。日期：2026-10-30。

目标：保持默认构建的上游命令与寻址行为，提供独立的 Nearshell 分发身份和私有路径。连接生命周期修复及附加的 build 字段适用于两种构建。

## 构建

默认 `cargo build --bin herdr-core` 保持 `hc` 命令名称及 HC_SESSION、HC_SOCKET、XDG 规则。

Nearshell 使用 `cargo build --locked --features managed --bin nearshell-hc`（当前只支持 macOS）。必须在构建环境指定：

| 变量 | 契约 |
|---|---|
| `NEARSHELL_CORE_COMMIT` | 完整的 40 位 Git commit；发行构建由构建脚本保证源码与之匹配 |
| `NEARSHELL_CORE_PATCHSET` | 与锁定源码实际具备能力一致的补丁契约版本 |
| `NEARSHELL_CORE_RELEASE_SEQ` | 分配给该发行物的正整数，单调递增；不从 semver 推断 |
| `NEARSHELL_CORE_CONFIG` | `dev` 或 `release` |
| `NEARSHELL_CORE_BUNDLE_ID` | 对应 App 的 bundle ID；Dev 必须与 Release 分开 |

`build-info --json` 直接输出 `{commit, patchset, releaseSeq, config}`；不解析路径、不连 core、不自启，运行时同名环境变量不能改变它。`hello.build` 与 `server.status.build` 描述正在运行的二进制。`config` 包含配置种类和 bundle ID；测试构建再带 `:scratch-test`。未提供发行参数的默认开发构建是 `unversioned` / `a1-dev` / 0 / `upstream`，不得当作发行身份。身份不是签名或内容哈希，后续 App 仍须验证发行物。

Cargo 启用 feature 会影响整个 crate；分发时只取 `cargo build --bin nearshell-hc` 指定的产物，不分发同次构建的 `herdr-core`。

发行构建使用 [managed universal 构建脚本](../scripts/managed-release.md)，分别指定 Release/Dev 的 bundle ID 与 config。

## 地址

managed 构建通过当前 OS 账户的 home 目录读取：

- `~/Library/Application Support/<bundle-id>/config/herdr-core/config.toml`
- state：同根的 `state/herdr-core/default/`
- config 必须明确给出 `socket`：Release 为 `/tmp/nearshell-<uid>-r/c.sock`，Dev 为 `/tmp/nearshell-<uid>-d/c.sock`。

HC_SESSION、HC_SOCKET 只要存在（包括空值）就返回 `managed_address_override`；`connect --session` 也被拒绝。HOME 与 XDG 环境不改变地址。config 缺失、语法错误、socket 缺失/非法、路径过长返回 `managed_config`，不会回退上游默认路径。socket 父目录必须属于当前用户、mode 0700、不是符号链接；不存在时创建，不修复已有的不安全目录。为派生 holder socket 预留长度。

`managed-test` 隐含 `managed`，必须给出绝对路径 `NEARSHELL_HC_TEST_ROOT`，config/state 使用上述相对结构，socket 必须位于 scratch 内。没有这个变量时拒绝运行，不回退产品路径。此开关不编入生产 managed 构建；测试构建身份有明显后缀，不能分发。

## 命名与隔离

managed CLI 的版本前缀、help、skill、固定提示与内置模板说明使用 `nearshell-hc`。用户模板、程序输出、协议标签与数据键保持原值。远端拨号不再回退 `hc` 或 `herdr-core`；上游下载器及向上游安装目录复制二进制的功能返回 `managed_distribution`，防止写入用户自装副本。A2 的 owner 鉴权、禁止自启及程序环境契约见下文。

## 连接与验证

连接关闭在 Core 状态锁下撤销 admission 并回收 viewer/订阅；迟到的 attach/subscribe 返回 `connection_closed`，不能重新登记。已受理的其他 RPC 保留原有完成语义，死连接上的迟到回复被丢弃。headless 尺寸恢复及队列回收由确定性测试覆盖。

默认验证：`cargo test --locked`、`just check`、`just docs-check`。额外验证 managed 代码：`cargo test --locked -p herdr-core --features managed-test --lib --test managed`。上游集成套件刻意使用 HC_SESSION，不适用于 managed feature；单独运行默认套件及上述 managed 套件。

## Managed program environments (P3)

Only the Cargo features `managed` / `managed-test` enable `[programs]`. Default builds
retain upstream environment behavior. Configure program values in the private
`config.toml`, for example:

```toml
[programs]
unset = ["XDG_CACHE_HOME"]
[programs.env]
XDG_CONFIG_HOME = "/user/config"
XDG_STATE_HOME = "/user/state"
XDG_DATA_HOME = "/user/data"
XDG_RUNTIME_DIR = "/user/runtime"
PATH = "/private/bin:/usr/bin:/bin"
NEARSHELL_HC = "/private/bin/nearshell-hc"
```

Run, nested run, replacement shells (keep), and exec use the same merge:
core environment without HC internals, the five XDG homes and terminal identity;
configured program values; per-call identity overrides; configured unset.
Fallback locale and terminal defaults belong to the base, so unset wins over them.
Omit each user XDG value that is unset; core-private XDG never becomes a fallback.
Per-call `env` accepts only TERM, TERM_PROGRAM, TERM_PROGRAM_VERSION and COLORTERM
(including exec); other keys return `managed_program_env`, without echoing values.
Invalid environment entries also fail closed. Managed waited runs do not inject
PAGER/GIT_PAGER/GH_PAGER; configure pager policy in `[programs]` if desired.

After the merge, HC fields from all sources are removed. Run/keep add fresh
terminal identity, socket, scope and PWD; exec adds only its scope and PWD.
Keep re-reads configuration, re-computes terminal identity, and persists only the
per-call override in `restart_env`. Invalid new configuration leaves recovery
pending rather than spawning with private/default environment values.

## Managed owner lifecycle (P4)

Managed is a build feature, not a runtime flag that a CLI caller can disable.
Default builds keep upstream auto-start and management behavior. Managed ordinary
commands work while core is alive; they never auto-start it. Holders keep PTYs
alive after core loss, but wait for an owner to start a new core.

The App launcher must implement this contract (all files under the compiled
AppSupport root, not its socket cleanup directory):

1. Open `owner.lock` without replacing its inode; acquire exclusive nonblocking
   flock and hold it for the App lifetime. A second App is a client only.
2. Before replacing a verifier, acquire exclusive `management.lock` (never
   replace its inode). Every accepted management operation holds a shared lease
   on this lock, including server validation; rotation waits for those operations.
   CLI lifecycle commands additionally acquire exclusive `lifecycle.lock` to
   serialize start/stop/restart/update. Direct App lifecycle RPCs must similarly
   hold a shared management lease and exclusive lifecycle lock through the reply
   and service EOF plus release of the existing core.lock inode. Release both before invoking a CLI lifecycle command, which
   acquires them itself. A new owner begins work only after verifier publication.
3. Generate 32 random bytes, encode as 64 hex characters, keep this token in
   owner memory only. Atomically write its lowercase SHA-256 hex digest into
   top-level `owner_token_sha256` in private config.toml. This is a verifier,
   not a token file. Rotate it on every owner takeover, including a surviving
   core; old tokens then fail. Release management.lock after config publication.
4. Invoke `nearshell-hc server start|stop|restart --owner-token TOKEN` or
   `nearshell-hc update --from FILE --owner-token TOKEN`. The option is explicit,
   never read from environment/config/token files. Do not log commands containing
   it. The server requires the same `owner_token` parameter on stop/restart,
   start (against an existing core), and `server.authorize` (pre-swap check).

Authorization requires both a matching verifier and a currently locked
owner.lock. Missing/invalid capability, orphaned verifier, or conflicting CLI
management returns `managed_by_owner`, with fixed text. Plain and framed RPC use
identical checks before lifecycle side effects. Each CLI operation checks the
live server as well as local config. `server.status` remains public.

Managed CLI stop/restart prepares its read timeout before sending the request:
on macOS, setting SO_RCVTIMEO after the peer closes can return EINVAL even for a
clean EOF. A successful response acknowledges the operation; it does not alone
prove service departure. The CLI then observes EOF and acquires the existing
`state/herdr-core/default/core.lock` inode, opened before the request. It releases
that lease before owner start. With no reachable listener, `stopped:false` is
returned only if core.lock is absent or available; an unreachable service still
holding the lock is unconfirmed, not evidence of completed departure.
The request/response read and the completion wait
are each bounded by 30 seconds; waiting for completion never resends the request.

- `managed_by_owner` still means the owner capability/lease check rejected an
  operation; no IO failure in shutdown completion is reclassified as denial.
- `managed_shutdown_unconfirmed` means preparation failed or no successful
  acknowledgement was confirmed. The effect can be unknown; inspect state first.
- `managed_shutdown_incomplete` means acknowledgement was received but the pinned
  service lock was not released within the completion deadline. Keep the managed
  files; a disconnected client socket is not a cleanup barrier.
- `managed_shutdown_transport` means acknowledgement and service-lock release
  were both observed, but the transport ended abnormally (or sent extra bytes).
  This is a completed service departure with a transport error, not a permission
  rejection; do not resend shutdown. A restart/update reports this state without
  automatically starting another core, leaving the next step to owner recovery.

These completion codes are CLI observations; direct RPC still acknowledges the
server operation and its caller must enforce the departure barrier itself.

Owner start/restart strips all inherited HC_* and XDG_* plus terminal identity,
then sets only the private XDG_CONFIG_HOME/XDG_STATE_HOME (HC_RUNTIME is added
internally for the daemon). Other launcher environment values form the program
base. Programs receive their user XDG via P3. Direct hidden `serve` requires both
explicit authorization and this clean environment. The owner token is never in
program/holder environments or restart_env. `server::serve()` without a token
cannot start a managed core.

**替换受管副本后必须 owner restart 才能新建终端。** 受管路径统一为
`<root>/bin/nearshell-hc`。即使 R 与 B 的 P8 身份和 hash 完全相同，只要 D 不同或损坏，
也必须先原子复制可信 B 为 D，再由 owner 计划内 restart；待确认新 core 身份后才能
继续新建终端。已有 PTY 由 holder 保留，不修改 holder 的构建身份校验。
不要把“旧终端仍存活”视为新 core/holder 已可接收新任务。

Managed update only replaces `<root>/bin/nearshell-hc`. It rejects other config
identities, lower releaseSeq, or unequal identities at equal releaseSeq, even
with --force. It reads candidate, installed and running identities; checks the
staged candidate and installed identity again; then revalidates local and live
server authorization immediately before the atomic rename. Refused authorization
or incompatible build leaves installed bytes unchanged. Already-replaced copies
must be restarted explicitly before creating new terminals; PTYs remain in holders. A failure after rename is
reported (it is not rolled back); the App's three-party recovery decision in the
design handles that state. Only --from is supported; upstream distribution paths
remain disabled. Build identity is not a signature/hash check: the App must
validate its bundled artifact before passing it to update (A5).

This is a cooperative same-user owner capability, not a sandbox against arbitrary
code running as the same OS user. The owner must follow the shared/exclusive lease and
lifecycle lock ordering above; a command authorized before owner death may
finish, and the next owner waits for that operation before rotating the verifier.
Group B must implement the lock lifetime and atomic config publication above.

Run `just managed-check` on macOS with the same Zig as `just check`. It freezes a
default-build hc in scratch before compiling managed-test; the cross-distribution
test starts that hc with a second scratch XDG/config/socket set. It never touches
the installed hc or the default user session. Ordinary `cargo test --features
managed-test --lib --test managed` skips only that fixture-dependent test and the
existing upstream namespace helper. Do not distribute managed-test builds.


## P1 ordered paint contract

`hello.features` advertises `paint-marks`; API callers enable it with
`terminal.attach {marks:true}`. The complete MARK payload, ordering, CREDIT,
resync and resource contracts are in [the wire specification](../schema/README.md#ordered-paint-marks-fork-extension).
This applies to both default and managed builds. CLI attach remains unmarked.
Remote relay currently rejects marked attach explicitly; connect to the target
core directly. No holder protocol or vendored Ghostty source changes are needed.

The default shared pending limit is 64 MiB (`viewer_pending_bytes` in config,
server restart to apply). A 4,600-row per-cell RGB history produced more than
4 MiB of VT paint with a measured shared-budget peak around 23 MiB for two
viewers and interrupted repaint. Twenty-four paused readers hit the 64 MiB cap;
allocations stayed within it and all reservations were released on disconnect.
Budget exhaustion may close affected streams (`paint_limit`); it does not kill
the terminal or lift the cap. Product workload tuning belongs to the later
CoreHost integration gates.

## P2 palette and dark preference

`hello.features` includes `display-palette`. Display accepts a sparse 0–255
palette and an explicit dark/light preference. The [wire contract](../schema/README.md#display-colors-display-palette-fork-feature)
defines replacement/omission, validation, query ownership, and the required
`paint_begin.colors` default layer. Both default and managed builds implement
this opt-in color replay contract.

The pinned Ghostty FFI already exposes default/effective colors and preserves
OSC overrides when defaults change. No vendor source or submodule revision
change is needed. Frozen replay clones recover override bits by temporarily
changing defaults to different colors and restoring them before formatting;
this handles overrides equal to defaults without depending on snapshot binary
layout. Canonical terminals are never probed this way. Snapshot restoration
retains saved color defaults; explicit dark is a viewer fact and must be sent
again after reconnect, while program colors survive handover.

## Marked paint history

For `terminal.attach` with `marks: true`, `history: N` is a subscription bound,
not only an initial-paint option. Every later synthetic paint (resize, display
facts, overflow, stitch or holder restoration) replays up to the same N history
rows. Receivers replace their parser at `paint_begin`, so a screen-only resync
would otherwise silently erase scrollback. Alternate screens retain their
screen-only semantics. CREDIT and shared pending budgets still apply to the
entire replay. Unmarked viewers keep upstream behavior.
