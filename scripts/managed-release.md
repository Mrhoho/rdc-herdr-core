# Managed macOS 发行构建

`scripts/build-managed.py`（`just managed-release`）只构建、签名和产出 manifest，不安装、不启动 core、不公证、不上传，也不修改钥匙串。默认上游构建命令和功能保持不变。

## 输入与用法

需要 macOS、Xcode command-line tools、Python 3、Cargo/Rust、Zig 0.16，以及 Rust 的 `aarch64-apple-darwin` 和 `x86_64-apple-darwin` targets。源码 checkout 必须位于指定的完整 40 位 commit，工作区（含未跟踪文件、递归子模块）干净，所有子模块已初始化且匹配 gitlink。

```sh
just managed-release --source /path/to/clean-core \
  --commit FULL_40_CHARACTER_COMMIT --patchset nearshell-a4 \
  --release-seq 1 --config release --bundle-id com.nearshell.agent \
  --identity 'Developer ID Application: YOUR NAME (TEAMID)' --timestamp \
  --output /path/to/artifacts/release

just managed-release --source /path/to/clean-core \
  --commit FULL_40_CHARACTER_COMMIT --patchset nearshell-a4 \
  --release-seq 1 --config dev --bundle-id com.nearshell.dev.ENVIRONMENT_ID.agent \
  --identity - --output /path/to/artifacts/dev
```

以上占位符必须换为真实值。`releaseSeq` 由发行方分配，是每个 config 内递增的正 u64；脚本不访问发行服务器、不自行分配序号。`patchset` 是发行方对该 commit 的补丁集声明。Dev 与 Release 分别调用，均为 Cargo release 优化构建；config/bundle ID 是发行身份和运行时隔离契约，Dev 不表示 `managed-test`。生产 bundle ID 必须与所属 App 一致，脚本不猜测 Dev 环境 ID。

本地验证显式传 `--identity -`，不允许同时要求可信 timestamp。使用证书时可选 `--timestamp`；省略时明确禁用 timestamp。签名始终启用 hardened runtime，并以 `bundle-id.core` 为代码签名 identifier。签名证书必须已经可用，脚本不创建或导入证书。

`--output` 必须不存在且在源码 checkout 之外；仅全部构建、签名验证、身份验证成功后发布该目录。失败时清理临时源码及产物，不留下看似成功的 manifest。可以用 `--target-dir /path/to/cache` 复用 Cargo 缓存；缓存需位于源码和输出目录之外，由发行脚本独占使用，勿同时运行直接指向该缓存的 Cargo。脚本会拒绝同时占用缓存的第二个发行构建。

`--deployment-target` 默认 `13.0`，同时约束两个 Rust 切片的 deployment target；使用较高的 App 最低系统版本可显式传参。本机验证不等于已在最低系统版本上运行验证。

## 来源与验证边界

构建前后校验完整 commit、干净工作区、子模块 pin；从 Git 对象导出源码到临时目录，忽略文件不参与编译。只用 `--locked --no-default-features --features managed`，不启用 `managed-test`。环境仅保留工具定位所需变量，发行身份由参数注入。工具链、Cargo registry/cache 和签名身份应来自受信任的构建机；此脚本不是运行不可信源码的沙箱。

保留上游 release 的 fat LTO、codegen-units=1，显式覆写 `profile.release.strip="none"`：本机 Rust 1.93/Xcode 27 的 strip 后过程宏 dylib 会因 `mis-aligned LINKEDIT string pool` 无法加载。保留符号避免此问题，不修改 Cargo.toml 的上游默认 profile。

先构建两个切片，`lipo -create` 后对 universal 整体签名，再执行 `codesign --verify --strict --all-architectures`。脚本执行本机架构的离线 `build-info --json` 并逐字段比对注入身份；跨架构运行需要额外在 Intel/Rosetta 下验证，manifest 的 `executedArchitectures` 不将结构检查冒充执行检查。

输出目录只包含 `nearshell-hc` 和 `manifest.json`。manifest 记录签名后的完整二进制 SHA-256、实际 build-info、semver、切片、签名类型/Team ID/authority/timestamp、源码及子模块 commit、驱动脚本 SHA-256、工具链与构建选项。ad-hoc 会明确标记，不能用于证明 Developer ID 发行者。manifest 本身不是签名信任根；App 必须从受信任的 bundle 获取预期身份/hash，并在执行磁盘副本前验证签名/Team ID/hash。

## 检查

`python3 -B -m unittest discover -s scripts -p test_managed_release.py` 验证身份输入、干净源码和缺失子模块拒绝、缓存互斥、签名证据和 build-info 比对；已接入 Unix `just lint`/`just check`。`just managed-check` 仍运行隔离的 managed-test 契约套件。
