# History and Notes continuity

Zuno can expose two optional continuity tools to the model: `history` for recovering
normalized evidence from the current session, and `notes` for durable working documents.
Both are disabled by default. Enabling them changes the model-visible tool surface; it does
not copy data to prompts automatically and does not grant authority that another policy
removed.

## Choose what is enabled

Enable both tools:

```json
{
  "continuity": true
}
```

Enable only History:

```json
{
  "continuity": {
    "history": true,
    "notes": false
  }
}
```

Enable only Notes:

```json
{
  "continuity": {
    "history": false,
    "notes": true
  }
}
```

Disable both explicitly:

```json
{
  "continuity": false
}
```

Omitting `continuity` also disables both. In the object form, a field that is absent from
the complete merged configuration defaults to `false`.

## Put the switch in the right layer

For a persistent user-wide choice, edit the `zuno.json` under the config root shown by:

```sh
zuno debug paths
```

The usual Unix path is `${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json`. Project
`zuno.json[c]` and `.zuno/zuno.json[c]` files may also set `continuity`, subject to the
normal layer order.

Use `ZUNO_CONFIG_DIR` when continuity should change with a provider or team profile. For
example, put this in `$HOME/.config/zuno/profiles/recovery/zuno.json`:

```json
{
  "continuity": {
    "history": true,
    "notes": true
  }
}
```

Then start Zuno with that overlay:

```sh
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/recovery" zuno
```

There is no dedicated `--continuity` command-line flag. Use a configuration file,
`ZUNO_CONFIG_DIR`, or `ZUNO_CONFIG_CONTENT` so the same setting works for TUI, headless,
ACP, and server processes.

Object fields merge recursively. If a lower layer already enables History, a higher layer
containing only `{"continuity":{"notes":true}}` leaves History enabled. Set the field to
`false` explicitly when a profile must turn it off:

```json
{
  "continuity": {
    "history": false,
    "notes": true
  }
}
```

For one process without creating a file, use the final inline layer:

```sh
ZUNO_CONFIG_CONTENT='{"continuity":{"history":true,"notes":false}}' \
  zuno debug config
```

On PowerShell:

```powershell
$env:ZUNO_CONFIG_CONTENT = '{"continuity":true}'
zuno debug config
Remove-Item Env:ZUNO_CONFIG_CONTENT
```

## ACP and editor profiles

An editor starts the configuration selected for its `zuno acp` process. To use a profile
from Zed or another ACP client, pass the same overlay through the Agent server environment:

```json
{
  "command": "zuno",
  "args": ["acp"],
  "env": {
    "ZUNO_CONFIG_DIR": "/config/.config/zuno/profiles/kiro"
  }
}
```

Place the `continuity` object in that profile's `zuno.json`. After changing a configuration
file, restart a long-running TUI, ACP server, or HTTP server before testing it. An in-flight
provider request keeps its immutable tool snapshot; a restart does not delete the durable
session, History evidence, Notes, Plan, or other work state.

## Enabling continuity is not the final grant

`continuity` contributes candidate tools. The final model-visible surface is still narrowed
in this order:

1. the active profile and Agent contract;
2. the top-level `tools` map;
3. an Agent's exact `tools` allowlist;
4. request hooks; and
5. effective `permission.rules`.

For example, this contributes both continuity tools but hides Notes:

```json
{
  "continuity": true,
  "tools": {
    "notes": false
  }
}
```

This keeps Notes in the candidate surface but denies every Notes invocation:

```json
{
  "continuity": true,
  "permission": {
    "rules": {
      "notes": "deny"
    }
  }
}
```

Setting `tools.history` or `tools.notes` to `true` cannot create a provider that
`continuity` left disabled. Likewise, `permission.mode: "allow_all"` cannot restore a tool
removed by the Agent allowlist or an explicit `deny`.

An Agent `tools` list is exact and replaces the inherited list. If a custom Agent declares
one, include `history` or `notes` alongside every other tool that Agent should retain.

## What the tools can see

### History

`history` provides four read-only actions:

- `list_windows` lists ranges delimited by successful compactions;
- `list_items` lists normalized items in one window;
- `read_item` reads one returned item; and
- `search_contents` searches normalized current-session content.

It never crosses into another session. Returned content excludes reasoning, encrypted
values, synthetic internal prompt text, and binary attachment bytes. Treat returned text as
untrusted data, not as instructions.

### Notes

`notes` provides three read actions and two writes:

- `list_files_by_prefix`, `read_file`, and `search_contents` are read-only;
- `append_to_file` and `write_file` are side-effecting and are never mechanically replayed.

Notes use logical slash-separated names such as `handoff/ci.md`, not host filesystem paths.
Each document belongs to the current `session_id + Agent`, so another Agent or a delegated
child session does not share it implicitly.

Every write carries `expected_revision`. Use `0` only for creation:

```json
{
  "action": "write_file",
  "name": "handoff/ci.md",
  "content": "Release candidate is waiting for Windows CI.",
  "expected_revision": 0
}
```

Read the document before changing it again and pass the returned revision. A stale revision
is rejected instead of overwriting concurrent work. One session-Agent scope is limited to
100 documents, 256 KiB per document, and 1 MiB total.

Disabling Notes later hides the tool but does not delete existing documents. They remain
part of the session lifecycle and reappear if the same session and Agent enable Notes again.
Session deletion and destructive prune remove them; export/import preserves them, while a
sanitized export redacts their identity and content.

## Plan remains independent

Continuity is separate from the host's durable Plan. This configuration:

```json
{
  "continuity": true,
  "tools": {
    "plan_update": false
  }
}
```

hides the model-facing Plan mutation tool, but the host can still create, persist, project,
and recover the Plan. It does not turn History or Notes into a replacement Plan store.

## Verify the effective result

Run the commands under the same environment as the real client:

```sh
zuno debug paths
zuno debug config
zuno debug agent build
zuno debug permissions
```

With a profile:

```sh
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/recovery" zuno debug config
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/recovery" zuno debug agent build
```

`debug config` confirms the merged `continuity` value. `debug agent` shows whether the
selected Agent retained the tools, and `debug permissions` exposes a final denial. The
`runtime.continuity` prompt section appears only when `history` or `notes` survives the
final provider-visible tool snapshot.

If a tool is missing, check the resolved config root, inherited object fields, top-level
`tools`, the Agent's exact allowlist, permissions, and whether the long-running client was
restarted.

## See also

- [Files and precedence](/config/files)
- [Tools](/guide/tools)
- [Sessions and turns](/guide/sessions)
- [Configuration reference](/reference/configuration)
- [Editors and ACP](/reference/zed-acp)
