# Excluded commands

Seven commands appear in `zuno --help` but do no work: `console`, `web`, `stats`, `github`,
`pr`, `uninstall`, and `generate`. They exist so that a command name inherited from upstream
resolves to a definite answer instead of an unrecognized-subcommand error. Each one prints why
it is not available and what replaces it, then exits unsuccessfully.

They are registered as explanations, not as placeholder implementations. There is no flag that
turns any of them on, and nothing in the runtime dispatches to a hidden backend behind them.

Every command below exits with a non-zero status, so a script that calls one fails rather than
silently continuing.

## zuno console

```sh
zuno console
```

```text
`console` is not available: Zuno does not provide a hosted console; use `providers` (alias `auth`) for local credentials instead
```

Credentials are local. Manage them with [`zuno providers`](/cli/providers).

## zuno web

```sh
zuno web
```

```text
`web` is not available: the bundled hosted web application is excluded from this headless Rust scope; use `serve` and connect a supported client instead
```

Run [`zuno serve`](/cli/serve) and point a supported client at it.

## zuno stats

```sh
zuno stats
```

```text
`stats` is not available: upstream stats reads the excluded stats package's session SQL directly; use `db stats` from todo 84 instead
```

Query the session store directly with [`zuno db`](/cli/db).

## zuno github

```sh
zuno github
```

```text
`github` is not available: the hosted GitHub agent is outside the local-agent scope; run `zuno run` from the CI workflow instead
```

Invoke [`zuno run`](/cli/run) from the CI workflow instead of hosting an agent.

## zuno pr

```sh
zuno pr
```

```text
`pr` is not available: the GitHub checkout helper is excluded from the local-agent runtime; use `gh pr checkout <number>` and then `zuno run` instead
```

Check the branch out with the GitHub CLI, then run [`zuno run`](/cli/run) against it.

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

Remove the executable through whatever installed it. In-place updates are still supported by
[`zuno self-update`](/cli/self-update).

## zuno generate

```sh
zuno generate
```

```text
`generate` is not available: the command is a TypeScript source-tree SDK/OpenAPI generator that depends on Prettier and is excluded from the runtime binary; use the server's `/openapi.json` document instead
```

Start [`zuno serve`](/cli/serve) and read the OpenAPI document the running server publishes.

## See also

- [CLI reference](/cli/)
- [zuno serve](/cli/serve)
- [zuno providers](/cli/providers)
- [zuno db](/cli/db)
- [zuno run](/cli/run)
- [zuno self-update](/cli/self-update)
- [Migration](/migration)
- [FAQ](/faq)
