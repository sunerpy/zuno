# 已排除的命令

有七个命令出现在 `zuno --help` 中，但不做任何实际工作：`console`、`web`、`stats`、`github`、
`pr`、`uninstall` 与 `generate`。它们存在的意义是让一个从上游继承来的命令名解析到一个明确的
答复，而不是一个未识别子命令错误。每一个都会打印它为何不可用、以及有什么替代它，然后以
失败状态退出。

它们是作为解释被注册的，不是作为占位实现。没有任何开关能把其中任何一个打开，运行时里也
没有任何东西会把它们分派到某个隐藏后端。

下面每个命令都以非零状态退出，因此调用其中之一的脚本会失败，而不是悄悄继续。

## zuno console

```sh
zuno console
```

```text
`console` is not available: Zuno does not provide a hosted console; use `providers` (alias `auth`) for local credentials instead
```

凭据是本地的。用 [`zuno providers`](/zh/cli/providers) 管理它们。

## zuno web

```sh
zuno web
```

```text
`web` is not available: the bundled hosted web application is excluded from this headless Rust scope; use `serve` and connect a supported client instead
```

运行 [`zuno serve`](/zh/cli/serve) 并让一个受支持的客户端指向它。

## zuno stats

```sh
zuno stats
```

```text
`stats` is not available: upstream stats reads the excluded stats package's session SQL directly; use `db stats` from todo 84 instead
```

用 [`zuno db`](/zh/cli/db) 直接查询 session 存储。

## zuno github

```sh
zuno github
```

```text
`github` is not available: the hosted GitHub agent is outside the local-agent scope; run `zuno run` from the CI workflow instead
```

从 CI workflow 中调用 [`zuno run`](/zh/cli/run)，而不是托管一个 Agent。

## zuno pr

```sh
zuno pr
```

```text
`pr` is not available: the GitHub checkout helper is excluded from the local-agent runtime; use `gh pr checkout <number>` and then `zuno run` instead
```

用 GitHub CLI 把分支 checkout 出来，然后对它运行 [`zuno run`](/zh/cli/run)。

```sh
gh pr checkout 1234
zuno run "review this pull request"
```

## zuno uninstall

```sh
zuno uninstall
```

```text
`uninstall` is not available: self-uninstallation is excluded from the runtime; remove `zuno` with the package manager or installer that placed it
```

通过当初安装它的那个途径移除可执行文件。就地更新仍然由
[`zuno self-update`](/zh/cli/self-update) 支持。

## zuno generate

```sh
zuno generate
```

```text
`generate` is not available: the command is a TypeScript source-tree SDK/OpenAPI generator that depends on Prettier and is excluded from the runtime binary; use the server's `/openapi.json` document instead
```

启动 [`zuno serve`](/zh/cli/serve) 并读取运行中的服务器发布的 OpenAPI 文档。

## 参见

- [CLI 参考](/zh/cli/)
- [zuno serve](/zh/cli/serve)
- [zuno providers](/zh/cli/providers)
- [zuno db](/zh/cli/db)
- [zuno run](/zh/cli/run)
- [zuno self-update](/zh/cli/self-update)
- [迁移](/zh/operate/migration)
- [FAQ](/zh/operate/faq)
