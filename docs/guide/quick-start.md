# Quick start

From nothing to a first successful run. Five steps, and the two that usually fail are
provider configuration and the sandbox probe, so they come first.

## 1. Confirm the binary and its paths

```sh
zuno --version
zuno debug paths
```

`debug paths` prints the roots this executable resolved. The `config` line is where
`zuno.json` belongs; everything below assumes it.

## 2. Verify the sandbox before relying on it

```sh
zuno debug sandbox --mode workspace-write --check
```

This runs the same backend Shell uses: it checks the launcher's ownership and trust, then
executes a probe through the real bubblewrap, capability-drop, and seccomp path. `--check`
exits unsuccessfully when the policy is not deployable.

If it fails, fix it now. A restricted sandbox mode that cannot be proved refuses to start
the session; it does not degrade to unconfined execution. On Linux the usual causes are a
bubblewrap older than 0.8.0 or a policy that forbids unprivileged user namespaces. See
[Permissions and sandboxing](/guide/permissions).

On macOS and Windows the confined backend is not implemented yet, so a restricted mode
reports an unsupported platform. Those hosts currently need explicit
`--sandbox danger-full-access`, which is a deliberate trust decision rather than a
workaround.

## 3. Configure a provider

Zuno ships no default model id. Declare a provider, its transport, and its models in
`zuno.json` under the config root:

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
$EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
```

```json
{
  "$schema": "https://raw.githubusercontent.com/sunerpy/zuno/main/schemas/zuno.json",
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model",
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "surface": "responses",
      "env": ["MYOPENAI_API_KEY"],
      "options": {
        "baseURL": "https://gateway.example.com/v1"
      },
      "models": {
        "primary-model": {
          "name": "Primary model",
          "reasoning": true,
          "tool_call": true,
          "limit": { "context": 200000, "output": 32000 }
        },
        "fast-model": {
          "name": "Fast model",
          "tool_call": true,
          "limit": { "context": 128000, "output": 16000 }
        }
      }
    }
  }
}
```

`transport` names the native Rust wire implementation and `surface` selects `responses`,
`chat`, or `messages`. Neither loads an npm package or starts Node. `myopenai` is an
ordinary provider id, not a reserved name.

## 4. Store a credential

```sh
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
```

Piped login reads standard input; interactive login disables terminal echo. Either way
the key stays out of shell history. Credentials land in
`$XDG_DATA_HOME/zuno/auth.json` with mode `0600` on Unix.

For the built-in `openai` provider, ask which methods exist before choosing one:

```sh
zuno providers methods openai
zuno providers login openai --method api-key
```

An environment variable declared under `provider.<id>.env` is consumed directly and is
never copied into the credential store, so a provider can already be authenticated
without any login command. See [Providers and credentials](/reference/providers) and
[Authentication](/config/authentication).

## 5. Confirm the model catalog, then run

```sh
zuno debug config
zuno models myopenai --verbose
```

`debug config` prints the merged configuration and names any rejected key, which is the
fastest way to catch a value placed in the wrong file. `models` confirms the exact
`provider/model` identifier that `run` and `tui` expect.

Then run something read-only first:

```sh
zuno run --agent plan "summarize how configuration precedence works in this repository"
```

`plan` is read-only: no write tool is registered, and its contract pins the sandbox to
`read-only` regardless of configuration. It is the safest way to confirm the whole path
works end to end.

Now do real work:

```sh
zuno run "add pagination to the /users endpoint and run the tests"
```

Or start the terminal application, which is also what bare `zuno` does:

```sh
zuno
```

## Common first-run failures

| Symptom | Cause | Fix |
| --- | --- | --- |
| `no trusted system bubblewrap executable was found` | No confinement backend | Install bubblewrap 0.8.0 or newer, then re-run `zuno debug sandbox --check` |
| `OS sandbox is not implemented for platform` | Confined mode on macOS or Windows | Use `--sandbox danger-full-access` deliberately, or run on Linux |
| A validation error naming a rejected top-level key | TUI-only key such as `theme` in `zuno.json` | Move it to `tui.json`. See [Files and precedence](/config/files) |
| Empty session list after switching builds | Source and release builds open different database files | See [Database lifecycle](/migration) |
| A model id is not found | Catalog cached before the provider was added | `zuno models --refresh` |

## See also

- [Your first session](/guide/first-session)
- [Configuration overview](/config/)
- [Providers and credentials](/reference/providers)
- [Permissions and sandboxing](/guide/permissions)
