# Nearshell 构建与 CLI 契约

状态：A1–A5 已实现；Linux 远端构建（寻址、生命周期、程序环境、`build-remote.py`）已实现；拨号与版本校验已实现；远端安装与更新（`remote_payloads`、`machine add --install`、`machine update`）已实现，由 Linux 测试与 macOS 脚本化对端验证，并由[端到端脚本](nearshell-remote-e2e.md)在 Debian 12 容器里经真实 OpenSSH 与 `systemd --user` 验证（对端 aarch64 原生、x86_64 仿真；hub 为 Linux 远端产品构建与本机 managed-test 构建），尚未在真实 x86_64 主机与 App 持有的产品 managed hub 上验证。发行前须通过锁定源码构建与组 A 契约矩阵。日期：2026-11-26。

目标：保持默认构建的上游命令与寻址行为，提供独立的 Nearshell 分发身份和私有路径。连接生命周期修复及附加的 build 字段适用于所有构建。

## 构建

默认 `cargo build --bin herdr-core` 保持 `hc` 命令名称及 HC_SESSION、HC_SOCKET、XDG 规则。

Nearshell 使用 `cargo build --locked --features managed --bin nhc`（只支持 macOS）。`managed` 隐含 `nearshell`：`nearshell` 负责身份隔离（`nhc` 命名、私有寻址、拒绝地址覆盖、禁用上游分发、构建身份必填与 `config` 后缀，`nhc` 目标也只要求它）；`managed` 再加 owner 生命周期与 `[programs]` 程序环境。只开 `nearshell` 不开 `managed` 的是[远端构建](#远端构建linux)，`build.rs` 只允许 Linux 目标，其他目标构建失败。必须在构建环境指定：

| 变量 | 契约 |
|---|---|
| `NEARSHELL_CORE_COMMIT` | 完整的 40 位 Git commit；发行构建由构建脚本保证源码与之匹配 |
| `NEARSHELL_CORE_PATCHSET` | 与锁定源码实际具备能力一致的补丁契约版本 |
| `NEARSHELL_CORE_RELEASE_SEQ` | 分配给该发行物的正整数，单调递增；不从 semver 推断 |
| `NEARSHELL_CORE_CONFIG` | `dev` 或 `release` |
| `NEARSHELL_CORE_BUNDLE_ID` | 对应 App 的 bundle ID；Dev 必须与 Release 分开 |

`build-info --json` 直接输出 `{commit, patchset, releaseSeq, config}`；不解析路径、不连 core、不自启，运行时同名环境变量不能改变它。`hello.build` 与 `server.status.build` 描述正在运行的二进制。`config` 为 `<dev|release>:<bundle-id>`，远端构建再带 `:remote`，测试构建的 `:scratch-test` 始终在最后（`dev:<bundle-id>:remote:scratch-test`）。格式只在 `crates/herdr-core/src/build_identity.rs` 写一次，`build.rs` 直接引用它。未提供发行参数的默认开发构建是 `unversioned` / `a1-dev` / 0 / `upstream`，不得当作发行身份。身份不是签名或内容哈希，后续 App 仍须验证发行物。

Cargo 启用 feature 会影响整个 crate；分发时只取 `cargo build --bin nhc` 指定的产物，不分发同次构建的 `herdr-core`。

发行构建使用 [managed universal 构建脚本](../scripts/managed-release.md)，分别指定 Release/Dev 的 bundle ID 与 config。

## 地址

managed 构建通过当前 OS 账户的 home 目录读取：

- `~/Library/Application Support/<bundle-id>/config/herdr-core/config.toml`
- state：同根的 `state/herdr-core/default/`
- config 必须明确给出 `socket`：Release 为 `/tmp/nearshell-<uid>-r/c.sock`，Dev 为 `/tmp/nearshell-<uid>-d/c.sock`。

HC_SESSION、HC_SOCKET 只要存在（包括空值）就返回 `managed_address_override`；`connect --session` 也被拒绝。HOME 与 XDG 环境不改变地址。config 缺失、语法错误、socket 缺失/非法、路径过长返回 `managed_config`，不会回退上游默认路径。socket 父目录必须属于当前用户、mode 0700、不是符号链接；不存在时创建，不修复已有的不安全目录。为派生 holder socket 预留长度。

`managed-test` 即 `managed` 加 `nearshell-test`；测试根覆盖由 `nearshell-test` 提供，必须给出绝对路径 `NEARSHELL_HC_TEST_ROOT`，config/state 使用上述相对结构，socket 必须位于 scratch 内。没有这个变量时拒绝运行，不回退产品路径。此开关不编入生产 managed 构建；测试构建身份有明显后缀，不能分发。

## 命名与隔离

Nearshell CLI 的版本前缀、help、skill、固定提示与内置模板说明使用 `nhc`。用户模板、程序输出、协议标签与数据键保持原值。远端拨号只用[绝对路径](#机器拨号与版本校验)，不回退 `hc` 或 `herdr-core`；上游下载器及向上游安装目录复制二进制的功能返回 `managed_distribution`，防止写入用户自装副本；`update` 不带 `--from` 在任何动作之前就返回它（上游会先重启较旧的 core）。远端构建的 `update`（带不带 `--from` 都一样）同样在任何动作之前返回 `managed_distribution`：远端 core 只经 hub 的[安装](#机器安装与更新)替换。A2 的 owner 鉴权、禁止自启及程序环境契约见下文。

所有 Nearshell 构建使用自己的名字：systemd 单元 `nearshell-<serve|hold>-<随机>` 与关机守护单元 `nearshell-shutdown-<hash>.service`，“没有 systemd 用户管理器”的提示以 `nhc:` 开头，替换二进制的临时文件为 `.nhc.new.<pid>`，远端“未安装”标记为 `the Nearshell remote core is not installed here`，`hc_missing` 的提示指向在 Nearshell 里添加机器。默认构建保持 `hc-…`、`.herdr-core.new.*` 与上游文案。

测试钩子与计时变量（`HC_SSH`、`HC_HOLDER_GRACE_MS`、`HC_RECONNECT_MS`、`HC_SPAWN_MS`、`HC_HEARTBEAT_MS`、`HC_FOREGROUND_MS`、`HC_RETAIN_MS`、`HC_READY_MS`、`HC_STITCH_MS`、`HC_QUIET_MS`、`HC_DRAFT_MS`、`HC_STARTED_MS`、`HC_SETTLE_MS`、`HC_IMAGE_ANSWER_MS`）统一经 `tuning` 读取：非测试的 Nearshell 构建（managed 与远端）一律忽略，测试构建与默认构建照旧。

## 连接与验证

连接关闭在 Core 状态锁下撤销 admission 并回收 viewer/订阅；迟到的 attach/subscribe 返回 `connection_closed`，不能重新登记。已受理的其他 RPC 保留原有完成语义，死连接上的迟到回复被丢弃。headless 尺寸恢复及队列回收由确定性测试覆盖。

默认验证：`cargo test --locked`、`just check`、`just docs-check`。额外验证 managed 代码：`cargo test --locked -p herdr-core --features managed-test --lib --test managed`。`tests/managed_machines.rs` 也属于 managed 套件（`check-managed.sh` 一并运行）。远端构建在 Linux 上用 `just remote-check`（`scripts/check-remote.sh`）：clippy、lib 单元测试、`tests/remote_build.rs` 与 `tests/remote_machines.rs`。上游集成套件刻意使用 HC_SESSION，不适用于 Nearshell feature；单独运行默认套件及上述两个套件。

## 远端构建（Linux）

`cargo build --locked --features nearshell --bin nhc`（不开 `managed`），只构建 `nhc`。它是被 Mac hub 经 SSH 接入的远端 core：身份隔离同 managed，生命周期与程序环境沿用上游。hub 如何拨到它、如何校验它见[机器：拨号与版本校验](#机器拨号与版本校验)；远端安装由后续步骤实现。

### 地址

| 项 | 位置 |
|---|---|
| 根目录 | `<账户 home>/.local/share/nearshell/<bundle-id>/`，新建时 0700 |
| 二进制（安装后） | `<根>/bin/nhc` |
| 配置（可选） | `<根>/config/herdr-core/config.toml`，缺失即默认值；模板在同目录的 `templates/` |
| 状态 | `<根>/state/herdr-core/default/` |
| socket | `/tmp/nearshell-<uid>-<bundle-id 的 sha256 前 8 位十六进制>/c.sock`，holder socket 在同目录 |

home 取自账户数据库（`getpwuid_r`），不读 `HOME`；XDG 变量不改变任何位置；配置里的 `socket` 不被采纳，拨号端与 core 因此总能算出同一个地址。socket 目录不存在时以 0700 创建；已存在但不属于当前用户、不是 0700 或是符号链接时返回 `nearshell_socket_dir`，不修复。`HC_SESSION`、`HC_SOCKET` 只要存在（包括空值）就返回 `managed_address_override`，`connect --session` 在解析路径之前被拒绝。账户数据库没有可用 home（或测试根缺失、socket 路径超长）返回 `nearshell_address`。

`nearshell-test` 下根目录取自绝对路径 `NEARSHELL_HC_TEST_ROOT`（不存在时以 0700 创建），socket 固定在 `<根>/run/c.sock`；没有这个变量时拒绝运行，不回退产品路径。

### 生命周期

与上游相同：`connect` 及其他普通命令在 core 未运行时启动它（Linux 有 systemd 用户管理器时作为用户服务，否则作为独立进程），holder 在 core 崩溃后把它拉起；`server restart`、`server stop` 不需要 token，重启期间 PTY 由 holder 保留；linger 处理与 `keeps_running_after_logout` 照上游；`restart --fresh` 与重启环境可用。

core 与 holder 的启动环境不继承任何 `HC_*`（只保留 core 自己设置的 `HC_RUNTIME`），包括 SSH 首次启动时从登录 shell 导入的变量；`HC_SESSION`、`HC_SOCKET` 始终列入 systemd 的 `UnsetEnvironment`，用户管理器里的同名变量也进不了服务。重启恢复的环境同样去掉 `HC_*`。

core 仍持有 `core.lock`、但它的 socket 路径已经不存在（`/tmp` 清理工具删除，或登录会话有独立的 `/tmp`）时，等待 15 秒后返回 `nearshell_socket_lost`，提示停止那个 `nhc serve` 进程后重试；以前只报 “core did not stop or start”。

### 程序环境

run、keep（替换 shell）与 exec 走上游的 `child_env` 与 exec 环境，不支持 `[programs]`：core 环境与 per-call env 里的 `HC_*` 先全部去掉，合并后注入 `NEARSHELL_HC=<根>/bin/nhc`，并把 `<根>/bin` 前置到 `PATH`（已有的同名项先移除，`PATH` 缺失时补 `/usr/local/bin:/usr/bin:/bin`），最后 run/keep 加终端身份（`HC_TERMINAL_ID`、`HC_TERMINAL_SOCKET`、`HC_SCOPE`、`PWD`），exec 加 `HC_SCOPE` 与 `PWD`。这些只进入子进程环境，core 自己的环境（重启时保存并恢复）不变。等待型 run 仍注入 `PAGER=cat` 等。用户自己的 `hc` 在远端终端里照常可用并连到用户自己的 core，两边互相看不到对方的终端。

### 构建制品

`scripts/build-remote.py` 与 `build-managed.py` 用同一套干净快照与身份参数（commit、patchset、release-seq、config、bundle-id），另须指定目标（`x86_64-unknown-linux-musl` 或 `aarch64-unknown-linux-musl`；目标参数可重复，同一输出目录放多个目标），Cargo 缓存目录可选：

```sh
python3 -B scripts/build-remote.py --commit "$(git rev-parse HEAD)" --patchset <patchset> --release-seq <n> --config dev --bundle-id <bundle-id> --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl --output <新目录>
```

`build-remote.py --test`（只能与 `build-remote.py --config dev` 同用）得到 nearshell-test 载荷：config 带 `:scratch-test` 后缀，只有 managed-test hub 接受，对端要有 `NEARSHELL_HC_TEST_ROOT`（如 sshd 的 `SetEnv`），不能分发。产出每个目标的 `<输出>/nhc-<target>` 与一个 `manifest.json`。输出目录就是 hub 的 [`remote_payloads`](#载荷目录)，格式见该节；manifest 另记源码 commit、子模块、驱动脚本 hash、工具链与 profile。macOS 上交叉编译、Linux 上原生构建用同一条命令：release profile（strip），rustc 自带的 `rust-lld` 链接，sha1 GNU build id，Cargo home 重映射为 `/nearshell/cargo`；工作区路径本来就是相对路径，因此同一 commit 在同一种主机上换源码目录构建，字节相同。macOS 交叉产物与 Linux 原生产物不保证逐字节相同（两边 `rust-lld` 写入的版本注释与字符串段顺序不同），所以制品只按各自 manifest 的 `sha256` 认定，不跨主机比对。能在本机运行的目标（同架构 Linux）由脚本执行 `build-info --json` 核对；交叉产物的 `build` 按构建参数写入，`executed:false`，真实身份由安装时的候选二进制报告。CI 的 `nearshell.yml` 在 Linux runner 上用 CI 身份原生构建两个目标，打包前核对 manifest 只有本目标一项、`executed:true` 且 `sha256` 与产物一致；`remote` 任务运行 `scripts/check-remote.sh`。

## 机器：拨号与版本校验

所有 Nearshell 构建（managed 与远端构建）都按本节拨号；默认构建的拨号命令、Windows 探测与安装保持上游。判定逻辑在 `crates/herdr-core/src/machines/nearshell.rs`，拨号与连线仍在 `machines.rs`、`remote.rs`。

### 拨号命令

拨 Unix 机器时，经 ssh 或 `--via` 在对端执行（`<bundle-id>` 为本构建编译进的 bundle ID）：

```sh
sh -c 'p="$HOME/.local/share/nearshell/<bundle-id>/bin/nhc"; [ -x "$p" ] || { echo "the Nearshell remote core is not installed here" >&2; exit 127; }; unset HC_SESSION HC_SOCKET; exec "$p" connect'
```

- 不查 PATH。未安装时打印“未安装”标记并以 127 退出，hub 返回 `hc_missing`。这个标记与 Windows 探测共用同一常量：看到它就不再按 Windows 重拨，未安装只拨一次。
- 先 `unset HC_SESSION HC_SOCKET`，用户在登录配置里为自己的 `hc` 导出的会话变量不会让 `nhc connect` 拒绝运行。
- `--via` 保持上游语义：这一行作为用户命令的最后一个参数传到对端展开。
- Nearshell 构建不接受机器的 session：`nhc machine add --session` 返回 `usage`（该选项在 `nhc` 的 help 中隐藏），`machine.add` 带 `session` 返回 `invalid_params`；旧记录里的 `session` 拨号时不使用。
- 第一次拨号失败且没有出现标记时，照上游再按 Windows 探测一次，Nearshell 的探测行是 `cmd /d /c "echo nearshell-windows-%OS% 1>&2 & exit /b 127"`，只有 cmd 会把 `%OS%` 展开成 `Windows_NT`。确认是 Windows 时返回 `unsupported_platform`，不进入上游的 Windows 安装分支。

### 版本校验

拨号拿到对端 `hello` 后，hub 用运行中的 core 编译进去的身份（`build_info()`）与对端 `hello.build` 比较，不看 semver。`machine.add` 与后台连线走同一条拨号路径，都经过这一步。

预期的远端 config 由 hub 自己的 config 推导：去掉末尾的 `:scratch-test` 与 `:remote`，补上 `:remote`，原来有 `:scratch-test` 再放回最后。因此：

| hub | hub config | 预期的远端 config |
|---|---|---|
| managed | `<dev\|release>:<bundle-id>` | `<dev\|release>:<bundle-id>:remote` |
| managed-test | `…:<bundle-id>:scratch-test` | `…:<bundle-id>:remote:scratch-test` |
| 远端构建当 hub | `…:<bundle-id>:remote` | 自身 |
| 远端测试构建当 hub | `…:<bundle-id>:remote:scratch-test` | 自身 |

| 对端 `hello.build` | 结果（错误码与机器状态） |
|---|---|
| config 为预期值，`releaseSeq`、`commit`、`patchset` 都相同 | 可用 |
| config 为预期值，`releaseSeq` 更小 | `needs_update` |
| config 为预期值，`releaseSeq` 更大 | `machine_newer` |
| config 为预期值，`releaseSeq` 相同但 `commit` 或 `patchset` 不同 | `build_conflict` |
| config 不是预期值（不论序号） | `build_conflict` |
| 没有 `build` 或无法解析（上游 hc 被放在这个路径，或不带身份的旧构建） | `build_conflict` |
| `hello` 返回 `unsupported_generation` | `incompatible` |

`incompatible` 只表示协议代际不符。`machine.add` 被拒时直接返回上表的错误码，机器不登记。

### 机器状态

`machine.list`、`ls` 的 `state` 在上游各状态（含 host key 与 `different_core`，见 schema/README.md）之外，Nearshell 构建还会出现 `machine_newer`、`build_conflict`、`incompatible`；`needs_update` 在 Nearshell 构建里表示对端序号更小。处于这四个状态的机器带 `since_ms` 与 `why`（拒绝原因，含两边的序号或身份），不按退避重拨：只有 `machine.connect`（`nhc machine login` 也会调用它）或 hub core 启动时会再拨一次。路由到这台机器的调用在 `needs_update` 时返回 `needs_update`，其余三个状态返回 `unavailable`。默认构建里协议代际不符仍记为 `needs_update` 并按退避重拨。

机器记录新增 `build`：最近一次连上时对端的构建身份，与 `version` 一起在每次连上时刷新；没有这个字段的旧记录照常读取（为 null）。`machine ls --json` 带出它；`nhc machine ls` 的人读输出在版本后显示 `build <releaseSeq> <commit 前 12 位> <patchset>`，没有记录时显示 `build unknown`。

### CLI

Nearshell 构建的 `nhc machine add` 只在 `hc_missing` 和 `needs_update` 时进入[安装](#机器安装与更新)（上游是 `hc_missing` 与 `incompatible`），且要带 `--install`。`machine_newer`、`build_conflict`、`incompatible`、`unsupported_platform` 不进入安装，直接输出错误与提示：

| 错误码 | 提示（hint） |
|---|---|
| `needs_update` | `update its remote core: <添加命令> --install`（路由调用为 `nhc machine update <name>`） |
| `machine_newer` | `update Nearshell on this Mac` |
| `build_conflict` | ``not handled automatically: compare `nhc build-info` here with `~/.local/share/nearshell/<bundle-id>/bin/nhc build-info` there`` |
| `incompatible` | `not handled automatically: make this Mac's Nearshell and the remote core there the same release` |
| `unsupported_platform` | `add a Linux machine` |

### 测试

- 单元：拨号命令字符串、预期远端 config 的推导与幂等、判定函数各分支（含 config 不符与缺少 `build`）、旧记录读取、`machine ls` 的人读输出。
- macOS（`tests/managed_machines.rs`）：managed hub 经 `--via` 与脚本化的对端，在本机存在的 sh、bash、zsh、dash、ksh、csh、tcsh 作登录 shell 时，未安装得到 `hc_missing`，对端被以 `connect` 参数、无 `HC_SESSION`/`HC_SOCKET` 执行；对端身份更新、同序号不同 commit、managed config、缺少 `build`、协议代际不符各得到对应错误码；CLI 只在未安装时进入安装（未配置载荷时得到 `remote_payloads_unset`）。
- Linux（`tests/remote_machines.rs`）：远端测试构建作 hub，经假 ssh（对端登录环境导出 `HC_SESSION`/`HC_SOCKET`）与 `--via` 连上同一构建；未安装只拨一次；Windows 探测返回 `unsupported_platform`；对端换成无身份或另一代际后机器停在对应状态、不自动重拨、`machine.connect` 才重拨。`check-remote.sh` 另外冻结四个身份不同的测试构建（同一 target 目录，每个只重编 herdr-core），被忽略的矩阵测试用它们覆盖 `needs_update`、`machine_newer`、两种 `build_conflict` 及各自的不重拨与恢复。

## 机器：安装与更新

Nearshell 构建（managed 与远端构建当 hub）用自己的载荷目录给 Linux 机器装远端 core：不下载、不复制 hub 自己的二进制、不写 `~/.local/bin`、不建 `hc` 链接。默认构建的 `--install` 与 `machine update` 保持上游。hub 侧判定在 `crates/herdr-core/src/machines/payload.rs`，IO 在 `machines/deliver.rs`；对端落位在 `crates/herdr-core/src/landing.rs`。

### 载荷目录

hub 的 config 写 `remote_payloads = "<绝对路径>"`（只有 Nearshell 构建读这个键，默认构建忽略）。目录里是 `build-remote.py` 的输出：

```json
{"schemaVersion": 2,
 "entries": [{"target": "aarch64-unknown-linux-musl", "file": "nhc-aarch64-unknown-linux-musl",
              "bytes": 0, "sha256": "<64 位小写 hex>", "build": {"commit": "…", "patchset": "…", "releaseSeq": 0, "config": "…"},
              "executed": false}],
 "source": {}, "buildDriverSha256": {}, "toolchain": {}, "profile": {}}
```

hub 只读 `schemaVersion`、`entries` 的 `target`、`file`、`bytes`、`sha256`、`build`，其余字段忽略。每个目标至多一项；`file` 必须是 manifest 旁边的普通文件名（不含 `/`、`\`、控制字符，不以 `.` 开头）。

### 安装与更新的步骤

`nhc machine add <target> --install`（未安装或 `needs_update`）与 `nhc machine update <name>` 走同一条路径：

1. hub 侧检查，任何一项失败都不向对端发送字节：正在运行的 hub core 的 `server.status.build` 必须等于这个 CLI 的 `build_info()`；`remote_payloads` 已配置且为绝对路径；manifest 可读且格式正确。
2. 在对端执行 `uname -sm`，只接受 Linux 且 `uname -m` 为 `x86_64`、`amd64`（→ `x86_64-unknown-linux-musl`）或 `aarch64`、`arm64`（→ `aarch64-unknown-linux-musl`）；Windows 照拨号返回 `unsupported_platform`，其他系统或架构也返回 `unsupported_platform`。
3. 取该目标的项：`build` 必须等于本 CLI 身份的预期远端版本（`config` 为[预期的远端 config](#版本校验)，其余字段相同）；文件字节数与 sha256 必须与 manifest 一致。
4. 经 ssh 或 `--via` 的 stdin 发送文件，对端执行（`N` 为字节数，`<hex>` 为 manifest 中 `build` 的 JSON 的 hex 编码）：

```sh
sh -c 'umask 077; unset HC_SESSION HC_SOCKET; d="$HOME/.local/share/nearshell/<bundle-id>/bin"; mkdir -p "$d" || exit 2; t="$d/.nhc.new.$$"; c() { rm -f "$t"; }; trap c EXIT; trap "exit 1" HUP INT TERM; head -c N > "$t"; [ $(($(wc -c < "$t"))) -eq N ] || { echo "the copy arrived incomplete" >&2; exit 3; }; echo copy arrived; chmod 700 "$t" && "$t" --json install --from "$t" --expect <hex>'
```

   这一行不含单引号与反斜杠，在 sh、bash、zsh、dash、ksh、csh、tcsh 作登录 shell 时都按同一语义执行；字节数不足以 3 退出，任何结果下临时文件都被删除；整份到齐后先打印一行 `copy arrived`（终端里的进度据此进入“启动”一步），结果仍在最后一行。每条对端命令的 ssh 带 `ServerAliveInterval=15`、`ServerAliveCountMax=4`，并有总时限：无输入的命令 60 秒，发送副本 120 秒加字节数按 16 kB/s 计的时间；超时得到 `install_timeout`。ssh 在 CLI 自己的进程组里，超时或 CLI 收到 INT/HUP/TERM 时整组先 TERM、2 秒后 KILL，CLI 随后按收到的信号退出。
5. 对端的隐藏子命令 `nhc install --from <临时文件> --expect <hex>`（只在远端构建里存在）：`--from` 必须是它自己、位于 `<根>/bin/` 且名字以 `.nhc.new.` 开头；自身身份必须等于 `nhc install … --expect <hex>` 给出的身份；在 `<根>/install.lock` 上取排他 flock（并发安装排队）；删除 pid 已不存在且超过 60 秒未修改的 `.nhc.new.<pid>`；按下表决定是否替换；替换时原子 rename 到 `<根>/bin/nhc`；正在运行的 core 若不是这个身份，经 `server.restart`（终端留在 holder 里）重启到 `<根>/bin/nhc`，再以 `server status` 核对身份。输出 `{build, was, replaced, restarted}`。
6. hub 核对输出的 `build` 等于 manifest 项的 `build`，然后重拨：`add` 登记的机器必须报告这个 `build`；`update` 每 200 ms 调一次 `machine.connect`，30 秒内必须成为 `connected` 且 `build` 为新身份。

对端已装的 `<根>/bin/nhc`（以 `build-info --json` 读取，缺失或无法运行视为未装）与正在运行的 core 各自按拨号的判定比较，不支持 `--force`：

| 对端已装或正在运行 | 结果 |
|---|---|
| 没有，或无法运行 | 安装 |
| 预期 config，`releaseSeq` 更小 | 替换（更新） |
| 与本副本相同 | 不替换；运行中的 core 不是这个身份时仍重启到它 |
| 预期 config，`releaseSeq` 更大 | `machine_newer` |
| 同序号不同 `commit`/`patchset`，或 config 不同 | `build_conflict` |

`nhc machine update <name>` 对状态为 `machine_newer`、`build_conflict`、`incompatible` 的机器在发送前就返回同名错误码，消息为 `<name>: <why>`，提示同拨号。人读输出：`<target>: installed its remote core (release M)`（`add` 写在 stderr）、`<name>: updated its remote core from release N to M`、`<name>: its remote core is already current (release M)`；重启过 core 时附 `; its terminals kept running`。`--json` 输出 `{build, was, replaced, restarted, machine, state}`。终端里（stdin、stdout、stderr 都是终端且未加 `--json`）`add`、`login`、`update` 逐步显示登录、平台、发送（已发/总量，原地刷新）、启动、等线路，最后一行为 hub 实际到达的状态（`added <name>: installed its remote core (release M); connected`）；未到 `connected` 时以该状态为错误码，消息为 `<做了什么>, but <hub 的状态>`，提示下一步。`--hold` 在结果后等回车，退出码不变；非终端不等待。

登出后保留：远端构建沿用上游的 linger 处理（core 在 systemd 下启动时尝试 `loginctl --no-ask-password enable-linger`，结果出现在 `hello` 与 `server.status` 的 `keeps_running_after_logout`、`logout_linger_user`），不另设机制；`machine add` 与 `machine update` 在对端不能保留时都输出同一条 `sudo loginctl enable-linger -- <user>` 警告。

### 错误码

| 错误码 | 消息 | 提示 |
|---|---|---|
| `core_build_mismatch` | `this core runs <描述\|no build identity>, but this nhc is <描述>` | managed：`restart the core onto this build (Nearshell does when it updates), then retry`；远端构建：``restart the core onto this build (`nhc server restart`), then retry`` |
| `remote_payloads_unset` | `no remote_payloads directory is configured` | `set remote_payloads = "<directory>" in <config>` |
| `remote_payloads_invalid` | `remote_payloads "<路径>" is not an absolute path`；`<目录>/manifest.json: <原因>`，原因为 `schemaVersion <n>, this build reads 2`、`unknown target "<t>"`、`two entries for <t>`、`file "<f>" is not a plain name beside the manifest`、`sha256 of <f> is not 64 lowercase hex digits` 或 JSON 解析错误 | `fix it in <config>`；`use a payload directory written by scripts/build-remote.py` |
| `remote_payloads_unreadable` | `<目录>/manifest.json: <IO 错误>` | `check remote_payloads in <config>` |
| `remote_payload_no_target` | `<目录> has no remote core for <target>` | `add one built for <target> with scripts/build-remote.py` |
| `remote_payload_build` | `<file> is <描述>, not <描述>`（对端核对自身身份时 `<file>` 为 `this copy`） | `use the remote cores built with this Nearshell` |
| `remote_payload_mismatch` | `<文件>: missing or unreadable`、`<文件>: <n> bytes, the manifest says <m>`、`<文件>: sha256 <x>, the manifest says <y>` | `the payload directory is damaged: write it again with scripts/build-remote.py` |
| `unsupported_platform` | `<machine> runs <os> <arch>, which Nearshell does not support yet` | `add a Linux machine` |
| `install_failed` | `on <machine>: <对端 stderr 最后一行>`（如 `the copy arrived incomplete`），或 `on <machine>: the install ended without a word`、`the remote core gave no install result`；对端：`cannot open install.lock: …`、`cannot lock install.lock: …`、`cannot put it at <路径>: …` | `nothing was changed there`（`uname` 失败）；`retry; the remote core there is replaced only by a whole copy that checked itself`；对端：`retry; nothing is replaced unless the install says so` |
| `machine_newer` | 对端：`on <machine>: a newer Nearshell remote core is here (release I; this copy: release C)`；`update`：`<name>: <why>` | `update Nearshell on this Mac` |
| `build_conflict` | 对端：`on <machine>: the remote core here is built as <config>, not <config>` 或 `on <machine>: another build of release N is here (<commit12> <patchset>; this copy: …)`；`update`：`<name>: <why>` | 同拨号 |
| `incompatible` | `update`：`<name>: <why>` | 同拨号 |
| `install_unverified` | `on <machine>: the remote core reports <描述>`；对端：`installed, but after its restart the core here runs <描述\|no core>`；hub：`<name> answers as <build> after its install`、`<name> is <state>[: <why>] after its install` | `its payload <file> says <描述>; write the payloads again`；``check `nhc server status` there; an update retries the restart``；``see `nhc machine ls`; `nhc machine update <name>` tries again`` |
| `install_timeout` | `<machine> did not finish within <n> s: the link is down or too slow` | ``nothing is replaced there unless a whole copy arrived; check that `ssh <machine>` answers, then run it again`` |
| `invalid_params` | `install --from takes this nhc itself, sent to <bin> under a temp name`、`the --expect value is not a build identity` | — |
| `managed_distribution` | 远端构建的 `update`；上游安装器 `machines::install` | 不变 |

`<描述>` 为 `release <n> (<commit 前 12 位> <patchset>, <config>)`。对端返回的 JSON 错误由 hub 原样转发错误码与提示，消息前加 `on <machine>: `。

### 测试

- 单元：manifest 校验（格式版本、未知或重复目标、文件名、sha256 形状、忽略多余字段）、目标映射、载荷选择与字节核对、安装命令文本（无单引号与反斜杠、`umask 077`、临时名）、对端结果解析、期望身份的编码往返；对端替换判定与陈旧临时文件判定。
- macOS（`tests/managed_machines.rs`）：managed hub 经 `--via` 与脚本化对端（假 `uname` 报 `Linux aarch64`，载荷是模拟落位的脚本）：各种载荷错误在发送前拒绝且对端无文件；本机 `uname` 得到 `unsupported_platform`；安装后对端只有 `nhc`、字节一致、以 `nhc --json install --from <bin>/.nhc.new.<pid> --expect <hex>` 的参数执行；`machine update` 重拨成 `connected`；安装命令在本机存在的各登录 shell 下完整副本落位、缺一字节以 3 退出且不留文件。`tests/managed.rs` 确认上游安装器仍返回 `managed_distribution`，Nearshell 安装在未配置载荷时返回 `remote_payloads_unset`。
- Linux（`tests/remote_install.rs`，经 `check-remote.sh`）：远端测试构建作 hub，经假 ssh 与 `--via`：载荷错误、`unsupported_platform`、陈旧临时文件清理、空账户安装（模式 0700、不写 `~/.local/bin`）、已是最新、损坏副本被替换且终端保留、短流以 3 退出、`update --from` 被拒；被忽略的矩阵测试用冻结的四个身份覆盖 hub CLI 与运行中 core 不符、旧版到新版更新且终端进程不变、降级与两种 `build_conflict` 拒绝（已装副本与仅运行中的 core 各一）、`update` 对 `build_conflict` 机器拒绝，以及两个并发安装在锁上排队、最终不留下旧版本与临时文件。

## 机器：端到端（真实 ssh 与 systemd）

`scripts/e2e-remote.py`（`just remote-e2e …`）在 Docker 里经真实 OpenSSH 与 `systemd --user` 验证添加、安装、使用、更新、登出与终端里的流程；不进 CI。用法、各轮覆盖与已观察到的行为见 [nearshell-remote-e2e.md](nearshell-remote-e2e.md)。

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
NEARSHELL_HC = "/private/bin/nhc"
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
4. Invoke `nhc server start|stop|restart --owner-token TOKEN` or
   `nhc update --from FILE --owner-token TOKEN`. The option is explicit,
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
`<root>/bin/nhc`。即使 R 与 B 的 P8 身份和 hash 完全相同，只要 D 不同或损坏，
也必须先原子复制可信 B 为 D，再由 owner 计划内 restart；待确认新 core 身份后才能
继续新建终端。已有 PTY 由 holder 保留，不修改 holder 的构建身份校验。
不要把“旧终端仍存活”视为新 core/holder 已可接收新任务。

Managed update only replaces `<root>/bin/nhc`. It rejects other config
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
A terminal on an added machine is relayed with its marks when the core there
advertises `paint-marks` (otherwise `marks_local_only`); the stream stays open
when the line drops, then reads a `discontinuity` and a `stitch` paint. No
holder protocol or vendored Ghostty source changes are needed.

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
rows; a machine relay attaches there again with the same N when its line comes
back. Receivers replace their parser at `paint_begin`, so a screen-only resync
would otherwise silently erase scrollback. As in upstream v0.1.21, attaching
on an alternate screen also restores the hidden primary screen and its bounded
history, so leaving the alternate screen reveals the original primary content.
CREDIT and shared pending budgets still apply to the
entire replay. Unmarked viewers keep upstream behavior.

## nhc 命令与环境

managed命令为nhc，不提供旧名或hc别名；NEARSHELL_HC仍是绝对命令入口。App owner负责在programs.env里前置私有bin并写入NEARSHELL_HC。run/exec/keep均重新读取config，新程序采用新环境，已有PTY进程不受追溯修改。改名时App先验签安装nhc、按既有owner restart保留旧PTY，再删除其私有bin旧入口。

Nearshell 机器连接只拨对端远端构建的绝对路径（见[机器：拨号与版本校验](#机器拨号与版本校验)），不依赖对端 PATH，不回退 upstream hc/herdr-core。via 字符串只负责传输，远端命令行仍由 core 生成。默认构建的安装与远端行为保持上游。
