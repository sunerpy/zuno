# MCP servers

Model Context Protocol servers contribute tools and resources Zuno does not ship. A server
is either spawned locally as a child process or reached over HTTP. Either way its tools
enter the same permission and authorization path as native tools; there is no second
plugin-only permission language.

One default is worth knowing before you register anything: an unknown MCP tool is treated
as side-effecting. It fails closed rather than being assumed safe.

## Register a server

```sh
zuno mcp add my-server -- npx -y @modelcontextprotocol/server-filesystem /srv/data
zuno mcp add remote-server --url https://mcp.example.com --header "Authorization: Bearer $MCP_TOKEN"
zuno mcp list
```

A local server's command goes after `--`. `--env` passes environment variables and
`--header` passes HTTP headers.

## Or configure it directly

The `mcp` map takes three shapes: a local server, a remote server, or a toggle for a server
another configuration layer defined.

```json
{
  "mcp": {
    "filesystem": {
      "type": "local",
      "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/srv/data"],
      "environment": { "LOG_LEVEL": "warn" },
      "cwd": "/srv/data",
      "enabled": true,
      "timeout": 5000
    },
    "docs": {
      "type": "remote",
      "url": "https://mcp.example.com",
      "headers": { "X-Tenant": "tenant-a" },
      "timeout": 5000
    },
    "inherited-server": {
      "enabled": false
    }
  }
}
```

| Field | Applies to | Meaning |
| --- | --- | --- |
| `type` | both | `local` or `remote` |
| `command` | local | Command and arguments, required |
| `cwd` | local | Working directory; relative paths resolve from the workspace |
| `environment` | local | Environment variables for the server process |
| `url` | remote | Server URL, required |
| `headers` | remote | Headers sent with every request |
| `oauth` | remote | OAuth settings, or `false` to suppress auto-detection |
| `enabled` | both | Start the server, or not |
| `timeout` | both | Request timeout in milliseconds; the runtime default is 5000 |

The toggle form takes only `enabled`, which is how a project layer disables a globally
defined server without repeating its definition.

A local server's `command` is a host executable, and the layer that declares it matters.
Declared in a project `zuno.json[c]` or `.zuno` file, it is refused: discovery fails with a
validation error naming `mcp.<name>.command`, because the checkout would be choosing a
program that runs on this machine with your authority. A remote server runs nothing locally
and is never refused, and the toggle form is how a project layer turns a globally defined
server off.

To let a checkout keep its own local servers, name it in a trusted layer:

```json
{
  "trust": {
    "project_host_commands": ["/home/you/src"]
  }
}
```

`true` trusts every checkout on this host, `false` is the default, and a list trusts the
project config files inside those absolute roots. A project layer that sets `trust` itself is
refused, so the grant always comes from the global `zuno.json`, `ZUNO_CONFIG`,
`ZUNO_CONFIG_CONTENT`, the managed config directory, or macOS managed preferences. Admitted
declarations are still logged one key at a time. Full precedence rules are in
[Files and precedence](/config/files).

## Authentication

For a remote server that needs OAuth:

```sh
zuno mcp auth remote-server
zuno mcp auth list
zuno mcp logout remote-server
```

The OAuth block controls the flow:

```json
{
  "mcp": {
    "docs": {
      "type": "remote",
      "url": "https://mcp.example.com",
      "oauth": {
        "clientId": "zuno-local",
        "scope": "read:docs",
        "callbackPort": 19876
      }
    }
  }
}
```

| Field | Meaning |
| --- | --- |
| `clientId` | Client id; absent means dynamic client registration (RFC 7591) |
| `clientSecret` | Client secret, when the authorization server requires one |
| `scope` | Scopes to request |
| `callbackPort` | Local callback port; the runtime default is 19876 |
| `redirectUri` | Full redirect URI, which overrides `callbackPort` |

Set `"oauth": false` to suppress auto-detection for a server that advertises OAuth metadata
you do not want used. For a static token, a header is simpler than an OAuth flow, but keep
the value in the environment rather than in the JSON file.

## ACP session servers

An ACP client may supply a complete session-local `mcpServers` list on
new/load/resume. Zuno supports stdio and Streamable HTTP there, not SSE. These
servers are not written into `zuno.json`: they are validated process-local
effects, all connect and discover before tools publish, and any partial startup
is disposed in reverse order. Client commands, environment values, and headers
are never stored in the session database or logs. See [zuno acp](/cli/acp).

## How MCP tools become available

Registration is not authorization. Four things must hold for a model to call an MCP tool:

1. The server is enabled and its connection succeeded.
2. The tool's wire id survives the agent's exact `tools` allowlist, when one is configured.
3. No explicit permission rule denies it.
4. For a delegated turn, the exact schema was visible in the parent Attempt.

Because unknown MCP operations default to side-effecting, granting one audited query tool
does not opt an agent into every tool the server exposes:

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "docs_search": "allow",
      "docs_write": "deny"
    }
  }
}
```

MCP and extension tools are not automatically available to every read-only agent. A
work-capable role may opt into automatic inheritance, and a read-only agent can be granted
one audited tool id explicitly in its own rules, but the parent schema ceiling and the
exact-schema check still apply. See [Agents](/guide/agents).

### Progressive schema discovery

Passing the four gates above makes an MCP tool executable; it does not require Zuno to
inject every connected schema into every provider request. For a root turn without an
exact Agent `tools` allowlist, matching MCP schemas are deferred behind `tool_search`.
That tool searches ids, display names, and descriptions. Matches are added cumulatively
to the next provider step and cannot be called from the same assistant tool-call batch
that discovered them.

This changes prompt exposure, not authorization: search cannot restore a denied tool or
one removed by capability filtering. An exact Agent `tools` allowlist is treated as an
intentional schema selection, so the MCP tools named there remain eagerly visible. A
delegated child receives only exact schemas recorded in the parent Attempt and does not
gain a broader catalog by searching.

ACP session-local `mcpServers` are also an explicit client contract. Their schemas are
present in the first provider request after the session's strict connection gate
succeeds; host-configured MCP servers in the same session remain progressively
discoverable. This distinction follows the session across child and background turns.

The provider-request snapshot records the exact post-search tool schemas. Search results
also record matched ids and the monotonic catalog revision in the durable tool result. If
another registered tool already defines `tool_search`, Zuno does not shadow it: schemas
stay eager for that turn and the host emits a warning.

## Concurrency and timeouts

```json
{
  "concurrency": {
    "mcp_connections": 8
  }
}
```

`mcp_connections` bounds simultaneous lifecycle operations across *different* servers. One
server's operations stay serialized, which avoids a second connect racing its own
disconnect. The field accepts `1..=64`.

Request timeouts are per server: set `timeout` on the entry. There is no global MCP
timeout key.

A failed MCP call is classified by what failed, not by the fact that it failed. A
5xx, 408, or 429 response and a dropped connection are recoverable, so the turn
retries them with backoff. Every other 4xx — 400, 401, 403, 404, 405 — and an OAuth
failure such as a server you set `"oauth": false` for blocks instead, because the
identical request keeps failing until configuration changes.

A client-side deadline is treated as a third case, because the request may already
have taken effect on the server. The remote tool proxy declares itself
non-replayable, so a timed-out call returns a recovery note saying the side effect
may have happened and must not be replayed until authoritative external state proves
it did not complete. The two resource-listing tools declare themselves replay-safe,
since `resources/list` mutates nothing.

Stdio framing is bounded. One JSON-RPC frame may be up to 64 MiB, over four times
the largest base64 resource blob a server can legitimately send. A longer frame fails
every pending call with a protocol error and closes the stream, because a stream cut
mid-frame cannot be resynchronised. Server stderr is bounded at 8 KiB per line and
truncated rather than treated as fatal, since it has no pending caller.

## When tools do not appear

```sh
zuno mcp list
zuno mcp debug my-server
zuno debug agent build
zuno debug permissions
```

`mcp debug` probes one registered server, which separates a server fault from a
registration fault. `debug agent` shows whether the tool survived the agent's capability
filtering, and `debug permissions` shows whether a rule denied it. Checking those two
before suspecting the server saves the most time, because a tool hidden by an allowlist
looks exactly like a server that failed to start.

## MCP prompts

A server may also expose prompts, which become commands. Zuno asks the server for its
prompt with every declared argument bound to the literal `"$1"`, `"$2"`, and so on, then
fills those placeholders with your real arguments. See
[Workflows and commands](/config/workflows).

## See also

- [zuno mcp](/cli/mcp)
- [Tools](/guide/tools)
- [Plugins and extensions](/plugins)
- [Configuration reference](/reference/configuration)
