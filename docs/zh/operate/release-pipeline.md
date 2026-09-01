# 发布流水线

Zuno 的发布产物只构建一次。release-please PR 会在其精确 head commit 上完成验证；合并后，
只有发布 tag 与候选产物拥有相同 Git tree 时，候选归档才会被晋级。

## 工作流职责

- `ci.yml` 是普通贡献 PR 的必需门禁。它使用标准 GitHub-hosted Linux 与 Windows runner；
  public fork 只获得只读权限，不会得到仓库 secrets。
- `release.yml` 负责 release-please、精确候选调度、发布身份校验和最终发布；它不安装 Rust，
  也不编译二进制。
- `release-candidate.yml` 负责完整测试和五个发布目标。每个目标在同一 job 中共同构建
  `zuno` 与 `zuno-smoke`，打包并解包归档，在原生架构上执行归档内二进制，生成 provenance，
  最后才上传产物。

Linux 源码门禁安装固定版本的 `cargo-nextest`。Linux 的 Clippy 与测试在同一 job 内复用
本地 target；原生 Windows 的 Clippy 与测试拆成两个并行 job，避免在测试执行前形成全局
串行屏障。Windows 使用 `scripts/test-parallel.sh`：Cargo 只编译一次，再由有界 worker
pool 并发运行测试二进制，避免为每个 test case 单独启动 Windows 进程。托管 Windows
同时运行四个二进制，每个二进制内部一次只执行一个测试；这样既隔离进程全局环境和时序状态，
`startup` 墙钟性能基准会先在无竞争状态下单独执行一次，避免其他进程使预算测量失真；ACP、
ConPTY 生命周期等所有功能 suite 仍留在并发池中，pool 之后不再追加串行队列。每个 suite
都有超时；超时时会终止完整
子进程树，调度期间也会持续输出进度。调度器通过原生 Python runner 将 Cargo 环境保存为
JSON，不再读取 Git Bash 文本格式的 `env`，因此 Windows `PATH` 等进程变量始终保持
原生表示。Python 探测会实际执行一次 import，再把已验证解释器解析为绝对路径；写入
Cargo runner 变量前还会转换成不含空格的 Windows short path，因此 Windows Store 的
应用执行别名无法伪装成可用 Python。

原生测试夹具也遵守同一平台边界：PTY API 在 Windows 使用 `COMSPEC`，不假定存在 `sh`；
LSP 夹具执行已验证的绝对 Python；祖先目录遍历测试使用每次运行唯一的 marker，开发者真实
主目录中的文件不会改变断言结果。

Windows 保留 Cargo 内建 `test` profile 和标准 `target/debug` 布局，但在 workflow 中覆盖
该 profile 的 debug 与 split-debug 字段，避免为约两百个短生命周期测试二进制生成和链接
调试数据库。panic 文本仍包含源码位置，开发环境和 Linux 测试仍保留行表回溯。Doctest 只由
Linux 源码门禁执行一次；Windows job 负责原生可执行行为，并显式设置 `RUN_DOCTESTS=0`，
不再重复一个曾增加八分多钟的跨平台 rustdoc 阶段。Windows 失败时会上传 Cargo timings、
构建/环境捕获日志和逐 suite 日志，供后续定位。

MSVC 版 `zuno.exe` 通过仅作用于该二进制的 build-script linker 参数保留 8 MiB 主线程栈。
原生 `dumpbin` 证据表明，PE 默认的 1 MiB 会在真实 session 构造路径中溢出。该参数不会写入
全局 `RUSTFLAGS`，因此库和约两百个测试二进制仍可复用原有编译缓存身份。

两个工作流都使用固定提交的官方 sccache action 及其 GitHub Actions 后端。CI 设置
`CARGO_INCREMENTAL=0`，Cargo registry/Git 下载使用按平台隔离的缓存；不上传原始
`target/`，避免 Cargo 文件锁竞争和跨 job 陈旧产物。

Linux 作业会安装发行版提供的 `bwrap-userns-restrict` AppArmor profile，并在运行 Zuno 前
分别验证 user/mount/PID namespace 路径和 network namespace 路径。它不会关闭 Ubuntu
宿主级的非特权 user namespace 限制。部署依据见[沙箱 FAQ](faq.md)。

提交 CI 前，Linux 开发者运行 `make pre-ci`。它会执行主机侧源码门禁、构建并 smoke
打包后的主机归档，并通过 Zig 对完整 workspace 执行 `x86_64-pc-windows-gnu` Clippy，
随后通过 `cargo-zigbuild` 链接全部 Windows GNU 测试二进制但不执行。这条交叉检查可以在本地发现 Windows
条件编译与链接错误，但无法证明 MSVC、ConPTY、Windows Job Object 或 hosted runner
的 loopback 行为，因此原生 Windows CI 仍是最终证据。

仓库 ruleset 严格要求 `zuno/pr-gate`，并要求分支相对 base 保持最新。缺少这条规则时，候选
工作流会拒绝合并。验证通过后，它只为精确 PR head 启用 squash auto-merge，等待 GitHub
确认合并，再显式唤醒发布收尾。`RELEASE_CANDIDATE_AUTOMATION=true` 是上线开关；未启用时
控制器可以更新 release PR，但不能自动调度或合并。

## 候选身份

封存后的 `release-candidate` Actions artifact 保留七天，只能通过 workflow run ID 访问。
其中包含五个归档、`SHA256SUMS`、逐目标证据和 `candidate-manifest.json`。Manifest 绑定：

- 仓库、签名 workflow ref 与 workflow commit；
- run ID 与 attempt；
- release PR、源 commit、release PR head 与 Git tree；
- 版本与预期 tag；
- 每个目标的归档名、字节数、SHA-256、build/smoke 结论、runner 与 attestation 身份。

发布过程不存在“选择最新 artifact”。晋级会校验 workflow 路径、事件、结论、源 SHA、PR
合并状态、manifest 字段、精确目标集合、大小、checksum 与 GitHub provenance。squash 后的
tag commit 可以与 PR head commit 不同，但两者 Git tree 必须逐字节一致。

## 发布与恢复

release-please 创建 tag 和 draft release。晋级过程只在 draft 状态上传资产；重新读取并确认
完整资产集合后才转为公开发布。任何不匹配都会让 draft 保持未发布。

自动失败绝不会回退到重新编译。恢复必须显式执行：

1. 在精确 release tag 上以 `mode=backfill` 调度 `release-candidate.yml`；
2. 记录成功的 run ID；
3. 以 `mode=promote` 调度 `release.yml`，传入该 run ID、已合并 release PR、候选源 SHA 和
   现有 tag。

这样正常路径始终只有一条，同时允许运维者从 artifact 过期或最终发布中断中恢复，而不降低
身份校验强度。

## 时延证据

从 release PR 创建开始计时，到公开 release 发布结束，runner 排队也计入。只有连续三次
端到端运行都在 15 分钟内完成、发布阶段不超过三分钟，并且下载后的发布资产通过 checksum、
provenance 与黑盒 smoke，才能宣称这项改造完成。
