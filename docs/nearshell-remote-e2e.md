# 远端机器端到端检查

状态：脚本可用，不进 CI；a、b 两轮已在 aarch64（原生）与 x86_64（仿真）对端上一次跑通（工作树构建）。日期：2026-11-26。

目标：在真实 OpenSSH 与 `systemd --user` 上验证 Nearshell 远端 core 的添加、安装、使用、更新、登出，以及终端里的添加、登录、更新流程与 App 收到的事件流。契约本身见 [nearshell-build.md](nearshell-build.md#机器安装与更新)。


`scripts/e2e-remote.py`（`just remote-e2e …`）在 Docker 里跑；辅助代码在 `scripts/e2e-remote-viewer.py`（伪终端与 App 的带 marks attach）和 `scripts/e2e-remote-flows.py`（应答式伪终端、事件流、主机密钥与 agent 场景、设计第 8 节的检查）。

需要：Docker 能运行特权容器并以 systemd 作 PID 1（`docker run --privileged --cgroupns=private`，已在 OrbStack 上验证）；与 `build-remote.py` 相同的 Rust 工具链、两个 musl 目标与 `ZIG`；第二轮需要 macOS。它从本 checkout 的 HEAD 克隆一份干净源码来构建（未提交的改动不在构建里），身份为 `dev` / `com.nearshell.e2e` / patchset `e2e`，序号 1 与 2。

```sh
just remote-e2e --work <新目录> [--round a|b] [--arch aarch64|x86_64] [--builds <可复用的构建目录>]
```

不指定轮次时 macOS 上跑两轮、Linux 上只跑 a；对端架构可重复指定，默认本机架构（其他架构经仿真）；构建目录按 commit 复用（`stamp.json`）。nearshell-test 载荷由 `build-remote.py --test` 生成。

要检查未提交的改动：把工作区（含未跟踪文件）拷进一份临时克隆，在那里临时提交，再运行那份克隆里的脚本；只改了 Python 时把它拷进同一克隆，commit 不变，构建照样复用。日志在 `<work>/e2e.log`，用户可见的文本汇总在 `<work>/user-visible.txt`。容器与网络结束时都删掉；镜像 `nhc-e2e:<arch>`（Debian 12 + systemd、sshd、tmpreaper）保留供下次使用，`just remote-e2e --remove-images …` 一并删除。密钥在运行时生成到 `<work>/keys`。b 轮另在本机 `/var/tmp/ne2e-*` 下放 hub 的根目录与 ssh 配置（socket 路径要短），结束时删除。

- **a 轮**：两台容器。hub 账户用远端产品构建当 hub，自己的 `~/.ssh`（config、密钥、known_hosts）以普通的 `ssh far` 连对端；对端账户的登录配置导出 `HC_SESSION`/`HC_SOCKET`。依次核对：空账户 `machine add` 得到 `hc_missing`；`add --install`（在伪终端里，人读输出）装上 release 1、打印 linger 警告并 `connected`，`machine ls` 显示对端构建；`run --on`、`read`、`exec`（`NEARSHELL_HC` 与 `PATH` 指向远端根）；不带 marks 的 `attach` 看到输出并以 ctrl+\ 退出；带 marks 的 attach（App 的做法，边读边回 CREDIT，按 App 解析器的规则核对每一帧，见 `scripts/e2e-remote-viewer.py`）应答 `resize_notifications: false`，收到 `paint_begin`（`attach`、`paintGen` 1，hub 报 `display-palette` 时带 `colors`）→ DATA → `paint_end` → 实时输出；对端 core 与 holder 是 `nearshell-serve-*`、`nearshell-hold-*` 用户服务，排在 `nearshell-shutdown-*` 之后，环境里没有 `HC_SESSION`/`HC_SOCKET`，`UnsetEnvironment` 列出这两个；socket 在 `/tmp/nearshell-<uid>-<8 位>/c.sock`，目录 0700；登出后（不开 linger）终端结束；以 root 执行警告里的 `loginctl enable-linger` 后，hub 换到 release 2 得到 `needs_update`，`machine update` 后 `connected` 在 release 2，终端 pid 不变且仍在输出；随后一条带 marks 的流（按终端现有尺寸）在两次断线中保持打开：对端 core `server restart`，线路自动恢复；对端手工换成 release 1 再重启，hub 停在 `needs_update`，流里只有一条 `discontinuity`，`machine update` 换回 release 2 后恢复。每次断线恰好一条 `discontinuity`，其后是 `stitch`、`paintGen` 更大、历史超出可见行数的重绘，再是实时输出（hub 换版本要重启并结束客户端的流，所以流跨的不是前面那次 `machine update`）；关掉全部登录后终端仍在；hub 退回 release 1 时 `machine_newer`，`update` 与 `add --install` 都被拒、对端仍是 release 2；`machine rm` 后对端 core、终端与文件都在。另外三项：会话的 `$HOME` 被登录配置改掉的账户、socket 目录被别的账户抢先建好的账户、`/tmp` 清理工具。
- **两轮共有**：从加入前到 `machine rm` 一直跟着 App 的事件流（`events.subscribe {}`，hub 重启后重连），核对它先听到 far 未同步、再听到 release 1 上 `connected` 且已同步，之后是终端的 `created`；更新时 `needs_update` 再到 release 2 上同步；hub 不重启时的一次断线后重新同步；`rm` 时先是 far 的移除，再是它的终端的 `removed`（`rm` 前先等流在最近一次重连后听到 far 已连上并同步：刚停下的 hub 还可能应答一次订阅）。`machine update --hold` 在 80 列伪终端里显示步骤，回车后退出 0。`rm` 之后再加入 far，依次：删掉 hub 这边的主机密钥 → `host_key_unknown`（b 轮由 `machine login --hold` 接受密钥，a 轮的 config 是 `StrictHostKeyChecking yes`，放回密钥后登录）；对端换一把主机密钥 → `host_key_changed`，`machine login --hold` 报错，换回后登录恢复；清空对端根目录 → `machine update --hold` 装上新的 core，hub 停在 `different_core`，输出说明做了什么、hub 到达的状态和下一步（`machine rm far`），`--hold` 后保持非零退出码。
- **b 轮**：本机的 managed-test hub（脚本充当 owner：持 `owner.lock`、写 verifier 与 `remote_payloads`、`server start --owner-token`）经 `/usr/bin/ssh` 连对端容器（端口只发布到 127.0.0.1）。对端是 nearshell-test 构建，sshd 的 `SetEnv` 让 `NEARSHELL_HC_TEST_ROOT` 等于拨号行用的路径。覆盖添加、安装、`machine login`（ControlMaster）、`ls`、`run`、`read`、两种 attach、对端服务与环境、更新、带 marks 的流跨两次断线（同 a 轮）、降级被拒、`rm`（同时结束共享登录）。
  - 本轮的 ssh config 对 far 用 `StrictHostKeyChecking ask`，known_hosts 起初为空：`--json machine add far` 得到 `host_key_unknown`；随后在 80 列伪终端里 `machine add --install --hold` 只回答问题：先给一个不合格的目标被要求重输，再给 `far`，名字直接回车取默认，ssh 问主机密钥时答 `yes`，看到登录、`not installed`、平台、发送进度（`… of … MB, arrived`）、启动、加入、等线路，最后一行 `added far: installed its remote core (release 1); connected`，回车后退出 0。
  - 只有终端有的 ssh agent：账户 far2 只认脚本自己起的 `ssh-agent`（`-a` 指向 scratch socket）里的密钥，config 用 `IdentityAgent SSH_AUTH_SOCK`；hub 的环境没有 `SSH_AUTH_SOCK`，终端有。hub 单独拨号得到 `needs_login`；终端里 `add far2 --install --hold` 成功，因为 hub 的线路走终端开的共享登录（ControlMaster）；结束这个共享登录并让线路断一次后，hub 回到 `needs_login`；终端没有 agent 时 `machine login far2` 失败，有 agent 时成功并 `connected`。
  - 与产品 managed hub 的差别：owner 是脚本而不是 App（没有 GUI 的 launchd 环境与登录 shell 的 PATH）；测试构建的根目录与 socket 来自 `NEARSHELL_HC_TEST_ROOT`；hub 的 `PATH` 最前面是一个 `ssh` 包装，执行 `/usr/bin/ssh -F <脚本的 config>`（只用脚本生成的密钥与 known_hosts，`IdentityAgent none`），用户的 `~/.ssh`、agent 与钥匙串不会被读写。没有用 `HC_SSH`：managed 的启动环境去掉全部 `HC_*`，它到不了 core。除这个包装外，拨号、安装与 `login` 走的是产品的 `ssh()`（`-S` 共享登录、`ControlMaster`、`-T`、`BatchMode` 等选项）。

已在 Debian 12（systemd 252、OpenSSH 9.2）上看到的行为：

- sshd 给会话的 `$HOME` 就是账户数据库里的 home。若登录配置改了 `$HOME`：`machine add` 得到 `hc_missing`；`add --install` 得到 `invalid_params`（`on <machine>: install --from takes this nhc itself, sent to <账户 home 下的 bin> under a temp name`），临时副本不残留。
- socket 目录已被别的账户建好：`add --install` 得到 `nearshell_socket_dir`，对端不留文件。
- 不开 linger 时，最后一个登录结束后用户管理器随即停止（logind 的 `UserStopDelaySec` 为 10 秒），core、holder 与终端一起结束，hub 重连后把终端列为 `finished`（`reason` 为 `stopped_by_restart`）。非 root 账户自己 `loginctl enable-linger` 被 polkit 拒绝（未装 polkit），所以出现警告。
- `systemd-tmpfiles --clean` 按 `q /tmp … 10d` 清理时删掉过期的普通文件和没人监听的 socket，保留正在监听的 socket 及其目录；`tmpreaper 7d /tmp` 默认不碰 socket；`tmpreaper --all` 删掉 socket 与目录，之后对端 `nhc` 返回 `nearshell_socket_lost`。Debian 自带的 `tmp.conf` 是 `D /tmp 1777 root root -`，不按时间清理。
- 由 `connect` 新起的 core 在第一次查看 logind 之前就回答 `hello`，曾让 `keeps_running_after_logout` 为空、`add --install` 不打印 linger 警告；现在 core 启动后的 `hello` 与 `server status` 最多等 2 秒，等第一次查看完成。

未覆盖：真实 x86_64 主机（只有仿真）；App 作 owner 的产品 managed hub；使用 pam_namespace 的主机；装有 polkit、允许用户自己开 linger 的主机；交互式 SSH 登录的密码与验证码（主机密钥的确认已覆盖）。

