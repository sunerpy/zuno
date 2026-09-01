# 发布流水线

Zuno 的发布产物只构建一次。release-please PR 会在其精确 head commit 上完成验证；合并后，
只有发布 tag 与候选产物拥有相同 Git tree 时，候选归档才会被晋级。

## 工作流职责

- `ci.yml` 是普通贡献 PR 的必需门禁。它使用标准 GitHub-hosted Linux 与 Windows runner；
  public fork 只获得只读权限，不会得到仓库 secrets。
- `release.yml` 负责 release-please、精确候选调度、发布身份校验和 GitHub 资产发布。
  候选晋级路径不安装 Rust，也不重新编译二进制。
- `release-candidate.yml` 负责完整测试和六个发布目标。每个目标在同一 job 中共同构建
  `zuno` 与 `zuno-smoke`，打包并解包归档，校验精确可执行架构并运行归档内二进制，生成
  provenance，最后才上传产物。Linux、Windows 和 arm64 macOS 原生执行；
  x86_64 macOS 在 `macos-15` Arm64 runner 上通过 Rosetta 2 执行。Windows x86_64
  使用 `windows-2022`，Windows ARM64 使用标准 `windows-11-arm` hosted runner。

按照仓库原生 Actions 审批策略，GitHub 可能会把 `GITHUB_TOKEN` 创建的 release PR
对应普通 `pull_request` workflow 标记为 `action_required`。这是有意保留的人工门禁，
不是测试失败，也不能由发布自动化绕过。仓库将
`actions/permissions/fork-pr-contributor-approval` 保持为
`all_external_contributors`，不会添加 CI skip 标记、改用 `pull_request_target`，
也不会给 release-please 配置特权 token。

release-please 创建或更新 PR 后，控制器不会相信第一次读取到的可变 PR API 结果。它会等待
PR base SHA 等于触发本次控制器的 `main` commit，验证由机器人创建的 release head
恰好只有这一个 parent，并再次读取 PR，确认 base/head 组合仍然一致。过期的 API 视图只会在
有界时间内重试，绝不会作为候选的 expected SHA 被调度。

维护者必须核对精确 head SHA；当 GitHub 把普通 `CI` run 标记为 `action_required` 时，
批准对应的精确 run。GitHub 放行 run 后，`ci.yml` 会有意忽略 `github.actor`：触发者可能是
批准或重新运行它的维护者，并不等于 PR 作者身份。轻量路由改为严格要求 release-please
机器人 PR 作者、同仓库 head、`main` base、release-please 分支前缀和
`autorelease: pending` 标签；它使用非受保护 check 名称，并跳过所有重复构建 job。
精确 head 的候选工作流仍是 `zuno/pr-gate` 的唯一所有者。普通 PR 与 fork PR 继续执行
完整 CI 矩阵。尚未批准的 `action_required` 表示正在等待人工操作，不能宣称发布已完成；
如果一直无人处理直到 GitHub 将其过期，就会留下容易误判的失败
`chore: release ...` 历史，本流程正是为了避免这种遗漏。

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
`CARGO_INCREMENTAL=0`，Cargo registry/Git 下载使用按平台隔离的缓存。普通 CI、候选测试、
Linux 发布目标和 Windows 发布目标都设置 `cache-targets: false`，不会上传大型 `target/`
目录。只有两个 macOS 候选 leg 启用 Rust 依赖 target 缓存；缓存 key 包含精确 Rust target，
并显式设置 `cache-workspace-crates: false`，因此 `x86_64-apple-darwin` 与
`aarch64-apple-darwin` 不会相互恢复对方的 target 产物。

Artifact 传输固定到 `actions/upload-artifact` v7.0.1 和
`actions/download-artifact` v8.0.1 的精确提交，这两个 action 使用 Node 24 runtime。
Linux musl leg 不再使用基于 Node 的 Zig setup action；`.github/scripts/install-zig.sh`
会按 Linux runner 的 x86_64 或 aarch64 架构选择 Zig 0.13.0 官方归档，并在解压前使用
硬编码的官方 SHA-256 校验。

Linux 作业会安装发行版提供的 `bwrap-userns-restrict` AppArmor profile，并在运行 Zuno 前
分别验证 user/mount/PID namespace 路径和 network namespace 路径。它不会关闭 Ubuntu
宿主级的非特权 user namespace 限制。部署依据见[沙箱 FAQ](faq.md)。

提交 CI 前，Linux 开发者运行 `make pre-ci`。它会执行主机侧源码门禁、构建并 smoke
打包后的主机归档，并通过 Zig 对完整 workspace 执行 `x86_64-pc-windows-gnu` Clippy，
随后通过 `cargo-zigbuild` 链接全部 Windows GNU 测试二进制但不执行。这条交叉检查可以在本地发现 Windows
条件编译与链接错误，但无法证明 MSVC、ConPTY、Windows Job Object 或 hosted runner
的 loopback 行为，因此原生 Windows CI 仍是最终证据。

两个 macOS 发布目标仍保留精确 Rust triple。`x86_64-apple-darwin` leg 在
`macos-15` Arm64 runner 上交叉构建，先通过 `lipo` 校验 `zuno` 与 `zuno-smoke`，
再用 `arch -x86_64` 运行 x86_64 smoke driver，通过 Rosetta 2 实际执行归档内的
x86_64 二进制。`aarch64-apple-darwin` leg 则以 `arch -arm64` 校验并执行。
翻译层、架构或 smoke 任一失败都会阻止 attestation 和上传；这项优化不会用静态检查替代执行。

仓库 ruleset 严格要求 `zuno/pr-gate`，并要求分支相对 base 保持最新。缺少这条规则时，候选
工作流会拒绝合并。`RELEASE_CANDIDATE_AUTOMATION=true` 只负责调度候选并认证精确的
release PR head，认证不等于批准或合并；自动合并由独立的
`RELEASE_CANDIDATE_AUTO_MERGE=true` 显式开启。未开启第二个开关时，维护者必须重新核对精确
head 后手动合并已认证的 PR，用户身份产生的 merge push 会自然唤醒发布收尾。只有显式开启
第二个开关时，候选工作流才为精确 head 启用 squash auto-merge、等待 GitHub 确认合并，并因
`GITHUB_TOKEN` 合并不会触发新 workflow 而显式调度发布收尾。

## 候选身份

封存后的 `release-candidate` Actions artifact 保留七天，只能通过 workflow run ID 访问。
其中包含六个归档、`SHA256SUMS`、逐目标证据和 `candidate-manifest.json`。Manifest 绑定：

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
端到端运行都在 20 分钟内完成、发布阶段不超过三分钟，并且下载后的发布资产通过 checksum、
provenance 与黑盒 smoke，才能宣称这项改造完成。时延目标不会放宽逐目标实际执行、
候选字节身份或发布校验。
