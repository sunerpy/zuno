# Authentication and credentials

Zuno keeps two things apart that other tools blur: which provider a model route
belongs to, and where that provider's credential comes from. A provider entry in
`zuno.json` is a catalog and a transport choice. A credential is a separate object,
stored outside configuration by default, resolved at request time.

[Providers and credentials](/reference/providers) is authoritative for provider
transports, login methods per provider, and the request path. This page covers the
configuration surface and the storage model.

## Two paths to a credential

An API key is the general case. Zuno stores it under a provider id and sends it as a
bearer credential to the configured endpoint.

OAuth is provider-specific. The built-in `openai` provider owns the ChatGPT login,
its refresh protocol, the ChatGPT endpoint rewrite, and the account header. Setting
`transport: "openai"` on a custom provider does not grant it that flow. A custom
OAuth provider needs its own registered login method and a request-side consumer; an
OAuth-shaped JSON object alone is not an integration.

```sh
zuno auth methods openai
zuno auth login openai --method chatgpt-browser
zuno auth login openai --method chatgpt-device
printf '%s' "$OPENAI_API_KEY" | zuno auth login openai --method api-key
```

`zuno auth` is an alias of `zuno providers`. Listing methods first is worth the
extra command: a configured provider id receives only the API-key method when its
resolved native transport actually consumes that credential, and an arbitrary or
credential-only id is rejected before Zuno reads standard input.

## Declaring where credentials come from

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `provider.<id>.env` | `string[]` \| `null` | none | Environment variables that supply this provider's credentials |
| `provider.<id>.api` | `string` \| `null` | none | Base API URL for the provider |
| `provider.<id>.id` | `string` \| `null` | the map key | Provider id override |
| `provider.<id>.name` | `string` \| `null` | none | Display name |
| `provider.<id>.transport` | enum \| `null` | none | Native request transport implemented by Zuno |
| `provider.<id>.surface` | `chat` \| `responses` \| `messages` \| `null` | none | Default request surface for this provider's models |
| `provider.<id>.options` | object \| `null` | none | Provider-level options, including SDK options this schema does not name |
| `provider.<id>.headers` | object of `string` \| `null` | none | Default extra HTTP headers for every model in this provider |
| `provider.<id>.models` | map of model \| `null` | none | Per-model configuration and overrides |
| `provider.<id>.whitelist` | `string[]` \| `null` | none | Models to keep, to the exclusion of the rest |
| `provider.<id>.blacklist` | `string[]` \| `null` | none | Models to drop |

`env` is a list, not a single name, and the first non-empty variable wins. That is
what lets one provider entry work across machines that name the same secret
differently:

```json
{
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "surface": "responses",
      "env": ["MYOPENAI_API_KEY", "OPENAI_API_KEY"],
      "options": { "baseURL": "https://gateway.example.com/v1" }
    }
  }
}
```

## Precedence

Credential resolution is ordered and has no hidden fallback:

1. `provider.<id>.options.apiKey`, including an explicitly empty string;
2. the matching entry in `auth.json`;
3. the first non-empty variable declared by `provider.<id>.env`;
4. no credential.

An explicitly empty `apiKey` therefore wins and yields no credential. That is
deliberate — it gives you a way to prove a provider is unauthenticated rather than
silently picking up an ambient environment variable.

An environment key is consumed directly and never copied into `auth.json`. This is
why a provider can already be authenticated on a fresh machine where nobody ran a
login command.

## Where credentials live

| What | Path |
| --- | --- |
| Credential store | `$XDG_DATA_HOME/zuno/auth.json`, normally `~/.local/share/zuno/auth.json` |
| Mode on Unix | `0600` |
| Override | `ZUNO_AUTH_CONTENT` replaces credential reads with a JSON object |

`ZUNO_AUTH_CONTENT` is the mechanism for ephemeral and managed environments —
containers, CI, a secret manager that injects at start. When credentials come from
that variable, Zuno does not persist rotated OAuth tokens back to disk, because
there is no file it owns.

The variable is withheld from the `shell` tool, so a command the model composes
cannot read the injected credentials. The whole `ZUNO_*` namespace is withheld, not
just this variable, which closes the same leak by the other route: an inline
`provider.<id>.options.apiKey` supplied through `ZUNO_CONFIG_CONTENT` is a provider
credential too, and it is withheld as well. A nested `zuno` started from inside such
a command therefore inherits neither, and resolves configuration and credentials the
ordinary way, which means it needs its own credential store or configuration. Plan
for that in a container that supplies credentials only through the environment. See
[Tools](/guide/tools#what-a-shell-command-inherits).

Putting `apiKey` directly in `zuno.json` is supported but exposes a secret to
configuration backups and source control. Prefer the credential store or an injected
`ZUNO_AUTH_CONTENT`. If you do use `options.apiKey`, keep it out of any layer that
gets committed; see [Variables and substitution](/config/variables) for reading a
value from a file or environment instead.

## Inspecting without leaking

```sh
zuno auth list
```

This prints active credential kinds, the storage path, and the matching environment
variable names, without printing secret values. A stored credential with no current
login-capable provider route is retained and labelled `orphan` so you can remove it
with `zuno auth logout`.

ChatGPT OAuth stores an access token, refresh token, expiry, and account id in the
same file. Before a request, Zuno refreshes a token that is near expiry and persists
the rotated tokens, unless credentials came from `ZUNO_AUTH_CONTENT`.

Codex and Claude Code product subagents are separate. They inherit the native
command's existing login, never appear in `zuno auth login`, and their credentials
are not copied into `auth.json`.

## Which providers are enabled

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled_providers` | `string[]` \| `null` | none | When set, the only providers to enable |
| `disabled_providers` | `string[]` \| `null` | none | Providers to drop even when their credentials are present |

`disabled_providers` is the answer to "an ambient environment variable is
authenticating a provider I do not want in this project". It drops the provider even
though the credential resolves.

## See also

- [Providers and credentials](/reference/providers)
- [Model routing](/config/models)
- [Variables and substitution](/config/variables)
- [Diagnostics](/operate/diagnostics)
