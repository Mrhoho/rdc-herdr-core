# Nearshell 构建与 CLI 契约

状态：A1 已实现；A2–A5 尚未实现，当前 managed 构建不能作为完整受管发行版交付。日期：2026-10-30。

目标：保持默认构建的上游命令与寻址行为，提供独立的 Nearshell 分发身份和私有路径。连接生命周期修复及附加的 build 字段适用于两种构建。

## 构建

默认 `cargo build --bin herdr-core` 保持 `hc` 命令名称及 HC_SESSION、HC_SOCKET、XDG 规则。

Nearshell 使用 `cargo build --locked --features managed --bin nearshell-hc`（当前只支持 macOS）。必须在构建环境指定：

| 变量 | 契约 |
|---|---|
| `NEARSHELL_CORE_COMMIT` | 完整的 40 位 Git commit；发行构建由构建脚本保证源码与之匹配 |
| `NEARSHELL_CORE_PATCHSET` | 补丁契约版本；A1 不能声称具备 A2–A5 的能力 |
| `NEARSHELL_CORE_RELEASE_SEQ` | 分配给该发行物的正整数，单调递增；不从 semver 推断 |
| `NEARSHELL_CORE_CONFIG` | `dev` 或 `release` |
| `NEARSHELL_CORE_BUNDLE_ID` | 对应 App 的 bundle ID；Dev 必须与 Release 分开 |

`build-info --json` 直接输出 `{commit, patchset, releaseSeq, config}`；不解析路径、不连 core、不自启，运行时同名环境变量不能改变它。`hello.build` 与 `server.status.build` 描述正在运行的二进制。`config` 包含配置种类和 bundle ID；测试构建再带 `:scratch-test`。未提供发行参数的默认开发构建是 `unversioned` / `a1-dev` / 0 / `upstream`，不得当作发行身份。身份不是签名或内容哈希，后续 App 仍须验证发行物。

Cargo 启用 feature 会影响整个 crate；分发时只取 `cargo build --bin nearshell-hc` 指定的产物，不分发同次构建的 `herdr-core`。

## 地址

managed 构建通过当前 OS 账户的 home 目录读取：

- `~/Library/Application Support/<bundle-id>/config/herdr-core/config.toml`
- state：同根的 `state/herdr-core/default/`
- config 必须明确给出 `socket`：Release 为 `/tmp/nearshell-<uid>-r/c.sock`，Dev 为 `/tmp/nearshell-<uid>-d/c.sock`。

HC_SESSION、HC_SOCKET 只要存在（包括空值）就返回 `managed_address_override`；`connect --session` 也被拒绝。HOME 与 XDG 环境不改变地址。config 缺失、语法错误、socket 缺失/非法、路径过长返回 `managed_config`，不会回退上游默认路径。socket 父目录必须属于当前用户、mode 0700、不是符号链接；不存在时创建，不修复已有的不安全目录。为派生 holder socket 预留长度。

`managed-test` 隐含 `managed`，必须给出绝对路径 `NEARSHELL_HC_TEST_ROOT`，config/state 使用上述相对结构，socket 必须位于 scratch 内。没有这个变量时拒绝运行，不回退产品路径。此开关不编入生产 managed 构建；测试构建身份有明显后缀，不能分发。

## 命名与隔离

managed CLI 的版本前缀、help、skill、固定提示与内置模板说明使用 `nearshell-hc`。用户模板、程序输出、协议标签与数据键保持原值。远端拨号不再回退 `hc` 或 `herdr-core`；上游下载器及向上游安装目录复制二进制的功能返回 `managed_distribution`，防止写入用户自装副本。A2 的 owner 鉴权、禁止自启及程序环境契约仍未实现。

## 连接与验证

连接关闭在 Core 状态锁下撤销 admission 并回收 viewer/订阅；迟到的 attach/subscribe 返回 `connection_closed`，不能重新登记。已受理的其他 RPC 保留原有完成语义，死连接上的迟到回复被丢弃。headless 尺寸恢复及队列回收由确定性测试覆盖。

默认验证：`cargo test --locked`、`just check`、`just docs-check`。额外验证 managed 代码：`cargo test --locked -p herdr-core --features managed-test --lib --test managed`。上游集成套件刻意使用 HC_SESSION，不适用于 managed feature；单独运行默认套件及上述 managed 套件。
