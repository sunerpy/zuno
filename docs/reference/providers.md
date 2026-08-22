# Providers and credentials

## Recommended native provider

`myopenai` is an ordinary provider id. Declare its endpoint, native transport, models, and default models in `zuno.json`:

```json
{
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model",
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "env": ["MYOPENAI_API_KEY"],
      "options": {
        "baseURL": "https://gateway.example.com/v1"
      },
      "models": {
        "primary-model": {
          "name": "Primary model",
          "reasoning": true,
          "tool_call": true,
          "limit": {
            "context": 200000,
            "output": 32000
          }
        },
        "fast-model": {
          "name": "Fast model",
          "tool_call": true,
          "limit": {
            "context": 128000,
            "output": 16000
          }
        }
      }
    }
  }
}
```

The checked starter file is [`examples/config/zuno.json`](../../examples/config/zuno.json). `transport` names a native Rust wire implementation; it is not the provider type, provider identity, or authentication method. Use `openai` for an OpenAI Responses or Chat Completions endpoint. Use `openai-compatible` only when a gateway implements a generic compatible protocol whose behavior differs from OpenAI. Neither transport loads npm packages, starts Node, or runs an AI SDK.

## First-run initialization

Zuno has no Node-based configuration generator. Initialize a checkout from the checked starter, edit the endpoint and model ids, then store the credential through the native CLI:

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
install -m 600 examples/config/zuno.json "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
$EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
zuno debug config
zuno models myopenai --verbose
```

When Zuno is installed without a source checkout, create the same `zuno.json` directly under the configuration root. Interactive API-key login disables terminal echo; piped login reads standard input. In either case, the key does not need to appear in shell history.

## Login methods

`zuno auth` is an alias of `zuno providers`. List the methods implemented for one provider before logging in:

```sh
zuno auth methods openai
zuno auth methods myopenai
```

In a terminal, a bare login opens a searchable provider picker. It includes the
official OpenAI integration and configured providers whose resolved model route
has a real API-key consumer. Catalog-only entries, historical credential ids,
and ambient-credential transports such as Bedrock are not login choices:

```sh
zuno auth login
```

Use the arrow keys or type to filter, then press Enter. If the selected provider
has several authentication methods, Zuno opens a second picker for the method.
Escape or Ctrl+C cancels either picker. Non-interactive invocations still require
an explicit provider so scripts cannot hang on a prompt.

The built-in `openai` provider supports three methods:

```sh
# Select browser OAuth, device-code OAuth, or API key interactively.
zuno auth login openai

# ChatGPT Plus/Pro in the local browser.
zuno auth login openai --method chatgpt-browser

# ChatGPT Plus/Pro on a headless or remote host.
zuno auth login openai --method chatgpt-device

# OpenAI Platform API key.
printf '%s' "$OPENAI_API_KEY" | zuno auth login openai --method api-key
```

A configured provider id such as `myopenai` receives only the API-key method
when its resolved native transport consumes that credential:

```sh
printf '%s' "$MYOPENAI_API_KEY" | zuno auth login myopenai
```

Configure a custom provider before logging in. An arbitrary or credential-only
id such as `kiro-auth` is rejected before Zuno reads standard input or writes
`auth.json`.

Using `transport: "openai"` does not grant a custom provider OpenAI's ChatGPT OAuth flow. The id `openai` owns that login, refresh protocol, ChatGPT endpoint rewrite, and account header. A custom OAuth provider needs its own registered login method and request-side consumer; an OAuth-shaped JSON object alone is not treated as a complete integration.

## Credential storage

Credentials created by `zuno auth login` are stored by provider id in `$XDG_DATA_HOME/zuno/auth.json` (normally `~/.local/share/zuno/auth.json`) with mode `0600` on Unix. `ZUNO_AUTH_CONTENT` can replace credential reads with a JSON object for ephemeral or managed environments.

Credential precedence is:

1. `provider.<id>.options.apiKey`, including an explicitly empty string;
2. the matching entry in `auth.json`;
3. the first non-empty variable declared by `provider.<id>.env`;
4. no credential.

Putting `apiKey` in `zuno.json` is supported but exposes a secret to configuration backups and source control, so the credential store or an injected `ZUNO_AUTH_CONTENT` is preferable.

An environment key is consumed directly and is not copied into `auth.json`. This is why a provider can already be authenticated even when the user never ran a Zuno login command. `zuno auth list` prints the active credential kinds, storage path, and matching environment variable names without printing secret values. A stored credential with no current login-capable provider route is retained and labelled `orphan` so it can be removed with `zuno auth logout`.

ChatGPT OAuth stores an access token, refresh token, expiry, and account id in the same file. Before a request, Zuno refreshes a token that is near expiry and persists the rotated tokens unless credentials came from `ZUNO_AUTH_CONTENT`.

## OpenAI API key versus ChatGPT OAuth

These are separate authentication products:

- An OpenAI Platform API key is sent to the configured OpenAI API endpoint as a bearer credential.
- ChatGPT OAuth signs into a ChatGPT subscription, requires the Responses surface, sends requests to the ChatGPT Codex backend, and includes the selected ChatGPT account id when available.

Selecting `--method api-key` never invokes ChatGPT login. Selecting either ChatGPT method never treats the resulting access token as a Platform API key.

Codex and Claude Code product subagents are a separate capability. They inherit the native command's existing login and never appear in `zuno auth login`; Zuno does not copy their credentials into `auth.json` or select their models. See [Codex and Claude Code product agents](../design/product-agents.md).

## How `myopenai` is called

The request path is native Rust:

1. `zuno-config` parses and merges `provider.myopenai`.
2. `zuno-llm` resolves the model catalog and builds a typed provider `Spec`.
3. `zuno-cli` selects `zuno-provider-openai` for the recommended `openai` transport.
4. `zuno-provider-openai` builds Responses or Chat Completions JSON, applies model capabilities and provider options, then sends the request with `reqwest`.
5. `zuno-llm` parses SSE framing and the provider crate translates chunks into shared stream events consumed by the engine.

The `openai-compatible` transport is implemented separately by `zuno-provider-compatible` and defaults to `/chat/completions`; rule-driven compatible providers may select `/responses`. `anthropic`, `bedrock`, and the Google transports use separate native crates because their request and stream protocols are not OpenAI-compatible.

Supported configuration values are `openai`, `openai-compatible`, `openrouter`, `anthropic`, `bedrock`, `bedrock-mantle`, `google`, `google-vertex`, and `google-vertex-anthropic`. Provider configuration has no `npm` field.

Important options include:

| Key | Meaning |
| --- | --- |
| `baseURL` or `endpoint` | API base URL; `endpoint` wins when both are set |
| `apiKey` | config-local credential, preferred over the credential store |
| `timeout` | whole-request timeout in milliseconds, or `false` |
| `headerTimeout` | response-header timeout in milliseconds, or `false` |
| `chunkTimeout` | maximum gap between streamed chunks in milliseconds |
| `maxTokens`, `temperature`, `topP`, `toolChoice` | generation controls forwarded by the native provider |
| `extraBody` | additional request fields after protected fields are assembled |

Run `zuno models myopenai --verbose` to inspect resolved models and `zuno debug config` to confirm the merged provider block without opening the credential file.

## Network proxies

Zuno applies one process-wide environment proxy contract to every in-process
outbound HTTP client used by a session: model providers, provider login and
OAuth, remote MCP, model and skill catalogs, remote instruction files,
`webfetch`, and `web_search`.

Set the standard variables before starting Zuno:

```sh
export HTTP_PROXY=http://127.0.0.1:1080
export HTTPS_PROXY=http://127.0.0.1:1080
export ALL_PROXY=socks5h://127.0.0.1:1080
export NO_PROXY=127.0.0.1,localhost,::1,.internal.example
zuno
```

The lowercase aliases `http_proxy`, `https_proxy`, `all_proxy`, and `no_proxy`
are also accepted. Scheme-specific variables take precedence over
`ALL_PROXY`; `NO_PROXY` bypasses every configured proxy for matching
destinations. HTTP, HTTPS CONNECT, SOCKS4, SOCKS5, and SOCKS5-with-proxy-DNS
URLs are supported. Restart Zuno after changing these variables because
connection pools capture the proxy policy when each client is constructed.

Commands started by shell tools, formatters, language servers, and local MCP
servers inherit the Zuno process environment. Their explicit environment
configuration may override individual proxy variables.

Amazon Bedrock runtime requests and AWS SSO credential requests use the same
environment proxy policy. This means a region that is reachable only through a
gateway needs no Bedrock-specific proxy option:

```sh
HTTPS_PROXY=http://127.0.0.1:1080 zuno
```

For a direct-versus-proxied comparison, add the resolved Bedrock hostname to
`NO_PROXY`, for example
`bedrock-runtime.us-east-1.amazonaws.com`. IMDS and the AWS-approved local ECS
credential endpoints are always direct, even when `HTTP_PROXY` or `ALL_PROXY`
is set; forwarding those metadata requests to an ambient proxy could expose
temporary AWS credentials. A remote HTTPS
`AWS_CONTAINER_CREDENTIALS_FULL_URI` remains proxy-aware and still honors
`NO_PROXY`.

The ownership and extension contract is specified in [Provider authentication](../design/provider-authentication.md).
