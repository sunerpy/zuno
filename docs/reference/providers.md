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

The checked starter file is [`examples/config/zuno.json`](../../examples/config/zuno.json). `transport` always names a native Rust implementation. Use `openai` for an OpenAI Responses or Chat Completions endpoint. Use `openai-compatible` only when a gateway implements a generic compatible protocol whose behavior differs from OpenAI. Neither transport loads npm packages, starts Node, or runs an AI SDK.

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

When Zuno is installed without a source checkout, create the same `zuno.json` directly under the configuration root. The credential command reads standard input, so the key does not need to appear in shell history.

## Credential storage

`zuno auth` is an alias of `zuno providers`. Credentials are stored by provider id in `$XDG_DATA_HOME/zuno/auth.json` (normally `~/.local/share/zuno/auth.json`) with mode `0600`. `ZUNO_AUTH_CONTENT` can replace credential reads with a JSON object for ephemeral or managed environments.

Credential precedence is:

1. `provider.<id>.options.apiKey`, including an explicitly empty string;
2. the matching entry in `auth.json`;
3. no credential.

Putting `apiKey` in `zuno.json` is supported but exposes a secret to configuration backups and source control, so the credential store or an injected `ZUNO_AUTH_CONTENT` is preferable.

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
