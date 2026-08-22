# Providers and credentials

## Minimal custom provider

`myopenai` is an ordinary provider id, not a built-in SDK name. Declare its endpoint, protocol family, models, and default model in `zuno.json`:

```json
{
  "$schema": "../../schemas/zuno.json",
  "model": "myopenai/example-model",
  "provider": {
    "myopenai": {
      "name": "My OpenAI-compatible gateway",
      "id": "myopenai",
      "transport": "openai-compatible",
      "env": ["MYOPENAI_API_KEY"],
      "options": {
        "baseURL": "https://gateway.example.com/v1"
      },
      "models": {
        "example-model": {
          "id": "example-model",
          "name": "Example model",
          "reasoning": true,
          "tool_call": true,
          "limit": {
            "context": 200000,
            "output": 32000
          }
        }
      }
    }
  }
}
```

`transport` names a native Rust implementation. It is optional for a custom provider because `openai-compatible` is the default; declare it when the endpoint uses another implemented protocol. Zuno does not load npm packages or run a TypeScript AI SDK.

## Setting credentials

The recommended local flow reads the key from standard input without putting it in shell history:

```sh
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
zuno providers list
```

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
3. `zuno-cli` selects `zuno-provider-compatible` for the `openai-compatible` transport.
4. `zuno-provider-compatible` builds Chat Completions or Responses JSON, applies model capabilities and provider options, then sends the request with `reqwest`.
5. `zuno-llm` parses SSE framing and the provider crate translates chunks into shared stream events consumed by the engine.

The `openai` transport is implemented by `zuno-provider-openai` and normally uses the Responses API. A custom `openai-compatible` provider defaults to `/chat/completions`; rule-driven providers may select `/responses`. `anthropic`, `bedrock`, and the Google transports use separate native crates because their request and stream protocols are not OpenAI-compatible.

Supported configuration values are `openai`, `openai-compatible`, `openrouter`, `anthropic`, `bedrock`, `bedrock-mantle`, `google`, `google-vertex`, and `google-vertex-anthropic`. The old `npm` key is not part of Zuno's schema.

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
