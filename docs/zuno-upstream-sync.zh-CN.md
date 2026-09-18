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
3. 否则在隔离 worktree 中准备候选：`git worktree add <tmp> rust-vX.Y.Z`，三方
   apply `git diff <baseline> <main>`，再改写 `UPSTREAM_CODEX.toml` 的 `[baseline]`。
   `main` 永不被修改。派生产物（`codex-rs/Cargo.lock`、
   `codex-rs/app-server-protocol/schema/**`、`codex-rs/core/config.schema.json`）不做
   文本合并：它们的冲突会被重置为上游字节，重放完成后巡检会从合并后的源码重新生成
   三者（`cargo update --workspace`、带与不带 `--experimental` 的
   `write_schema_fixtures.py`、`codex-write-config-schema`），再提交。
4. 重放干净时，用 `--force-with-lease` 强推 `upstream-sync/X.Y.Z`，并新建或刷新
   PR。`main` 一旦前进，下一次运行会重新准备候选，使 PR 始终重放当前已评审差量。
   更旧版本的候选 PR 和同标签的冲突 issue 会被标记为已取代并关闭。
5. 有冲突时，新建或更新一个 issue，列出冲突路径和本地解决命令。不推送任何东西。

同一时间只跟踪一个 release：最新正式标签。alpha 标签被忽略。

## 本地解决冲突

```sh
git fetch --tags --prune upstream
python3 scripts/zuno_upstream.py --no-fetch prepare \
  --source main --target rust-vX.Y.Z \
  --branch upstream-sync/X.Y.Z \
  --worktree ../zuno-upstream-X.Y.Z
# 在 ../zuno-upstream-X.Y.Z 中处理冲突标记，然后重新生成派生产物：
cd ../zuno-upstream-X.Y.Z/codex-rs
cargo update --workspace
python3 app-server-protocol/scripts/write_schema_fixtures.py
python3 app-server-protocol/scripts/write_schema_fixtures.py --experimental
cargo run -p codex-config-schema --bin codex-write-config-schema
cd .. && git add --all && git commit && git push -u origin upstream-sync/X.Y.Z
```

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

## 不变量

- 同步过程永不修改 `main` 与当前 worktree。
- 候选总是从干净的 `main` 准备；源脏则中止。
- 重放前校验精确的上游标签、提交与 tree。Codex 每个 release 都在独立的短分支上切出，
  因此目标通常是基线的兄弟而非后代；两者必须共享历史，仅存在于旧 release 分支上的
  提交会列在 PR 里。
- 不自动合并任何东西。合并就是评审门。
- 评审后的字节不再重新构建；晋升只是重新发布 PR gate 封存的产物。
