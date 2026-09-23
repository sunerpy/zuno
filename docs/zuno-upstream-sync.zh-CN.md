# Codex 上游发布同步

Zuno 是 [openai/codex](https://github.com/openai/codex) 的源码级 fork。每个 Codex
正式版（`rust-vX.Y.Z`）都会被折叠进 Zuno：把已评审的 Zuno 差量重放到该精确
release 上，六个平台包只构建一次，再把这些字节原样晋升为 `zuno-vX.Y.Z`。因此
Zuno 的版本号跟随 Codex。

整条流水线只有一个人工步骤：合并候选 PR。此前此后全部自动。

```text
openai/codex 打标签 rust-vX.Y.Z
        │  定时巡检（每 6 小时）或手动触发
        ▼
zuno-upstream-sync.yml ── 重放无冲突 ──▶ 分支 upstream-sync/X.Y.Z + PR（标签 upstream-sync）
        │                                          │
        └── 重放有冲突 ──▶ issue（标签             ▼
            upstream-sync-conflict）      zuno-ci.yml：zuno/pr-gate 构建、冒烟、
            本地解决后推送分支              签署并封存六个平台包
                                                   │
                                                   ▼
                                        人工评审 + 选择 "Create a merge commit"
                                                   │
                                                   ▼
                                        zuno-release.yml（pull_request: closed）
                                        校验合并形态，下载封存的候选产物，
                                        打 zuno-vX.Y.Z 标签，不重新构建直接发布预览版
```

## 巡检：`zuno-upstream-sync.yml`

每 6 小时定时运行，也可 `workflow_dispatch`（可选精确 `target` 标签、可选
`refresh`）。每次运行：

1. 拉取 Codex 标签，执行 `scripts/zuno_upstream.py check --allow-current`。
   若最新正式标签等于 `UPSTREAM_CODEX.toml` 记录的基线，直接结束，不产生任何副作用。
2. 查找 `upstream-sync/X.Y.Z` 上已打开的候选 PR，或该标签对应的冲突 issue。若其中
   任何一个已记录当前 `main` 提交（`Zuno-Source-Commit` trailer），本次运行为空操作。
   这保证了定时任务幂等。
3. 否则在隔离 worktree 中准备候选：`git worktree add <tmp> rust-vX.Y.Z`，执行
   `git merge-tree --merge-base=<baseline> rust-vX.Y.Z main`（ort 合并，带重命名检测，
   需要 git 2.40+）并检出到该 worktree，再改写 `UPSTREAM_CODEX.toml` 的 `[baseline]`。
   `main` 永不被修改。派生产物（`codex-rs/Cargo.lock`、
   `codex-rs/app-server-protocol/schema/**`、`codex-rs/core/config.schema.json`）不做
   文本合并：它们的冲突会被重置为上游字节，重放完成后巡检会从合并后的源码重新生成
   三者（`cargo update --workspace`、带与不带 `--experimental` 的
   `write_schema_fixtures.py`、`codex-write-config-schema`），再由
   `scripts/zuno_upstream.py finalize` 提交：提交是一个合并，**第一父节点是 `main`
   提交，第二父节点是精确的 release 提交**。因此候选包含 `main`，PR diff 恰好是上游改动，
   GitHub 合并是平凡的（晋升的 tree 等于认证 head 的 tree），release 提交也对下一次同步可达。
4. 冲突以 `diff3` 标记合并，随后经过**改名重放**（见下文）：凡是仅因 Zuno 改写了
   Codex 文案而产生的冲突块都会被自动解决。若 `origin` 上已存在同一 release 的已
   finalize 候选，它携带的全部合并后编辑（冲突解决、干净合并文件里对上游 API 变化的
   适配、孤儿文件删除）会被**复用**到所有 `main` 此后未改动的路径，因此 `main` 前进
   不会抹掉评审者已经做过的工作。
5. 合并干净时，用 `--force-with-lease` 强推 `upstream-sync/X.Y.Z`，并新建或刷新
   PR。`main` 一旦前进，下一次运行会重新准备候选，使 PR 始终重放当前已评审差量。
   PR 正文会列出改名重放解决、刷新、删除和复用了哪些路径。更旧版本的候选 PR 与冲突
   issue（本标签及更旧标签的）都会被标记为已取代并关闭。
6. 仍有冲突时，新建或更新一个 issue，列出冲突路径、重放摘要和本地解决命令。不推送任何东西。

同一时间只跟踪一个 release：最新正式标签。alpha 标签被忽略。

## 改名重放：`FORK_REBRAND.toml`

Zuno 差量的大部分是对用户可见 Codex 文案的改名（产品名、`ZUNO_HOME`、`~/.zuno`、命令示例、
仓库链接）。每个上游 release 都会碰到其中一些行，过去每次同步都因此冒出几十个平凡冲突。
`FORK_REBRAND.toml` 把这次改名记录为有序规则（字面或正则替换、Zuno 有意保留的身份词
保护列表如 `Codex Desktop`、`Codex Apps`，以及针对 TUI 状态卡快照的按路径 `${version}` 规则，
上游把它们渲染为 `v0.0.0`）。`prepare` 只在一个安全谓词成立时应用它们：

> 只有当把规则应用到某个冲突块的 Codex 基线文本能**逐字节**复现出 Zuno 侧文本时，
> 自动化才解决这个块；解决结果就是把同一组规则应用到新的上游文本。

按冲突路径具体来说：

- **内容冲突**（`diff3` 块，中间是基线）：每个纯改名块被替换为改名后的上游文本；规则复现
  不出来的块保留标记。只有不再有任何标记时文件才记为 *resolved*，否则记为 *partial*。
- **modify/delete**（上游删除了一个 Zuno 只改过名的文件）：随上游删除。
- **工作区版本**（`codex-rs/Cargo.toml`）：只包含 `version = "…"` 一行的冲突块取 release
  版本，因为 Zuno 版本跟随 Codex 版本。

干净合并的纯改名文件也会被**刷新**：若上游 release 在一个 Zuno 已改名的文件里新增了文案，
新文案同样会被改名，`${version}` 占位也会挪到新 release。上游在清单 `[[added]]` 作用域内
**新增**的文件（目前是 TUI 的 `.snap`）不经谓词直接改名：它们是渲染文本，源码已经写着 Zuno，
PR 门禁的快照测试会抓住任何漏改。其余 Zuno 差量之外的文件永不改写；规则会改动其新上游
文本的路径会以 `drift` 列在 JSON 报告里供审阅。

规则永远不会脱离谓词运行，所以规则集不完整只会损失覆盖率，不会损害正确性。用下面的命令
度量覆盖率：

```sh
python3 scripts/zuno_rebrand.py audit --baseline rust-vX.Y.Z --source main \
  --version "$(sed -n 's/^version = "\(.*\)"$/\1/p' codex-rs/Cargo.toml | head -n1)" --list-diverged
```

它会列出规则复现不出来的 Zuno 改动文件。当某个改名决定在整个树上一致时，把它加进
`FORK_REBRAND.toml`，下一次同步就会自动解决。`scripts/test_zuno_rebrand.py` 固定了规则语义；
`prepare --no-rebrand` 会保留全部冲突供人工处理。

## 本地解决冲突

```sh
git fetch --tags --prune upstream
# 若 origin 上已有该 release 的已 finalize 候选，加上 `--reuse origin/upstream-sync/X.Y.Z`，
# 它的合并后编辑（解决、适配、删除）会复制到所有 main 此后未改动的路径。
python3 scripts/zuno_upstream.py --no-fetch prepare \
  --source main --target rust-vX.Y.Z \
  --branch upstream-sync/X.Y.Z \
  --worktree ../zuno-upstream-X.Y.Z
# 以下命令都在仓库根目录执行。只有命令列出的路径仍带标记（diff3 风格：上游、Codex 基线、
# Zuno）；纯改名块已按 FORK_REBRAND.toml 重放，纯改名的 modify/delete 已删除，其余
# modify/delete 保留 Zuno 版本。UPSTREAM_CODEX.toml 已写入新 release。
# 在 ../zuno-upstream-X.Y.Z 中处理标记后，重新生成派生产物：
(cd ../zuno-upstream-X.Y.Z/codex-rs \
  && cargo update --workspace \
  && python3 app-server-protocol/scripts/write_schema_fixtures.py \
  && python3 app-server-protocol/scripts/write_schema_fixtures.py --experimental \
  && cargo run -p codex-config-schema --bin codex-write-config-schema)
# finalize 会拒绝残留的冲突标记，使用 prepare 记录的 main 提交（传入不同的 --source 会被拒绝；
# main 前进了就重新 prepare），并以 main 与 release 为双父节点提交，带 Zuno-Source-Commit /
# Zuno-Upstream-* trailer：
python3 scripts/zuno_upstream.py finalize --worktree ../zuno-upstream-X.Y.Z
git -C ../zuno-upstream-X.Y.Z push -u origin upstream-sync/X.Y.Z
```

finalize 之前先跑 TUI 全量测试（`cargo test -p codex-tui`，PR 门禁不会全跑）：规则没覆盖到的
上游文案（见冲突 issue 里的 drift 列表）会在这里表现为快照不匹配，改名后少一个字符的词也会
让渲染快照重新换行、光标列号偏移。审阅待定改动，只接受纯改名、宽度或版本造成的差异（`.snap`
文件用 `INSTA_UPDATE=always`；行内 `@"…"` 快照手改），新出现的用户可见文案要改源码而不是只改快照。

从该分支开 PR 并加上 `upstream-sync` 标签，晋升流程才会识别它。当之后某次对
`main` 的重放变干净时，巡检会自动关闭冲突 issue。

## 晋升：`zuno-release.yml`

当来自本仓库 `upstream-sync/*` 分支的 PR 合并进 `main` 时自动运行。所有输入都
从已合并的 PR 推导：head SHA、该 head 上 `codex-rs/Cargo.toml` 的版本、该 SHA 最近
一次成功的 `zuno-ci.yml` run 及其 `zuno-candidate` 产物。带显式输入的
`workflow_dispatch` 仍可用于其他发布。

发布前会校验：

- PR 已合并进 `main`，合并提交恰有两个父节点：PR base 与已认证的 head（squash 与
  rebase 会改变认证字节，因而被拒绝）；
- finalize 产出的候选 head 本身是合并：第一父节点为 PR base，第二父节点为其
  `UPSTREAM_CODEX.toml` 记录的 release 提交，且 tree 与记录一致；
- 合并后的 tree 等于认证 head 的 tree，即发布的字节就是 PR gate 构建的字节；
- head 保留了其 `UPSTREAM_CODEX.toml` 记录的 Codex 基线；
- 封存的 `candidate-manifest.json` 与每个归档都匹配 run、attempt、PR、head、父节点、
  tree 与版本，并带有来自 GitHub 托管 runner 上 `zuno-ci.yml` 的有效来源证明。

随后在合并提交上创建不可变标签 `zuno-vX.Y.Z`，上传六个归档、
`zuno-package_SHA256SUMS`、冒烟报告和 manifest，重新下载比对字节，最后发布为
非 latest 的预览版。

## 让门禁全自动的仓库密钥

用默认 `GITHUB_TOKEN` 打开的 PR 不会触发 `pull_request` workflow，因此
`zuno/pr-gate` 不会自动跑在候选上。请创建一个仅限本仓库的 fine-grained personal
access token，授予 **Contents: read and write** 与 **Pull requests: read and
write**，存为仓库 secret `ZUNO_UPSTREAM_SYNC_TOKEN`。巡检只在推送与开 PR 两步使用它。
没有该 secret 时候选 PR 仍会打开，巡检会留言说明，关闭再重新打开 PR（或向分支推送）
即可手动启动门禁。

## 手动控制

- 立即巡检：**Actions → Prepare Codex upstream sync → Run workflow**（可指定精确标签）。
- 重新准备已打开的候选：以 `refresh = true` 触发。
- 暂停自动化：在 Actions 页面禁用这两个 workflow 即可，无需改其他配置。
- 巡检始终检出并重放 `main`，即使从其他分支手动触发也是如此；对巡检本身的改动要合并后才生效。

## 不变量

- 同步过程永不修改 `main` 与当前 worktree。
- 候选总是从干净的 `main` 准备；源脏则中止。
- 重放前校验精确的上游标签、提交与 tree。Codex 每个 release 都在独立的短分支上切出，
  因此目标通常是基线的兄弟而非后代；两者必须共享历史，仅存在于旧 release 分支上的
  提交会列在 PR 里。
- 不自动合并任何东西。合并就是评审门。
- 自动化只在 `FORK_REBRAND.toml` 能从 Codex 基线逐字节复现 Zuno 侧时解决一个冲突块；其余
  冲突块都等待人工。`main` 前进时复用已评审的合并后编辑，而不是重新推导。
- 评审后的字节不再重新构建；晋升只是重新发布 PR gate 封存的产物。
