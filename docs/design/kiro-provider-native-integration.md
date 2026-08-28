# Kiro Provider native Responses integration

Status: the Zuno session-affinity slice was implemented on 2026-08-27. The
remaining native Kiro instruction-projection work is owned upstream.

## Decision

Zuno should continue to use `kiro-provider` as an independently deployed
loopback service through the native Rust OpenAI Responses transport. It must
not embed the provider, copy authentication identities, add private headers,
or encode provider state in model-visible prompts.

The current integration is functionally usable, with one upstream limitation:

- Claude Opus 5 and GPT 5.6 Sol complete real Zuno turns;
- tools, streaming, effort, usage, cancellation, and durable Zuno history flow
  through the normal provider interface;
- the gateway owns OpenCode-shared authentication, account selection,
  encrypted reasoning replay, and upstream transport pools;
- Zuno sends each foreground root or child durable session id through standard
  Responses `metadata.zuno_session_id`, and keeps lifecycle model calls
  isolated;
- current Zuno instructions require the gateway's explicit
  `legacy-user-prefix` migration mode. Stable completion remains blocked until
  Kiro has a lossless native instruction projection.

## Pre-affinity validation baseline

The live validation used:

- Zuno release binary SHA-256
  `cafc5200e0ac40cd36643b86b57d25ae0c28eb1a2f9a05594aeaa85dc82a4e4d`;
- kiro-provider binary SHA-256
  `80c9eb07885f1fc20aefb94f80872af8e0559cea1a6e1b3a653510fe9fe19ee5`;
- `surface: "responses"`, `maxTokens: null`, and a private loopback Bearer key;
- provider `auth_source: "opencode-shared"`,
  `session_affinity_mode: "explicit-only"`,
  `sdk_http_keep_alive: false`, and
  `protocol_projection_mode: "legacy-user-prefix"`.

Observed results:

| Route | Result | Provider evidence |
| --- | --- | --- |
| `deep` → `claude-opus-5` | shell wrote 23 exact bytes, read returned the file, final text was `ZUNO_KIRO_OPUS5_TOOL_OK` | main calls used `effort=high` |
| `build` → `gpt-5.6-sol` | final text was `ZUNO_KIRO_GPT56_SOL_OK` | main call used `effort=max` |
| `/health` and authenticated `/ready` | both healthy after service restart | 59 catalog entries, including all six Opus 5 variants |

The same baseline logs showed `affinity_bound=false` for every Zuno request.
That request-shaping gap is the Zuno-side issue addressed by this design.

## Post-implementation E2E

The installed release binary used for the affinity validation had SHA-256
`d89ea044875e32143630d243b1070647e3944ffe381c54d93a714e695e2a4824`.
The kiro-provider binary remained
`80c9eb07885f1fc20aefb94f80872af8e0559cea1a6e1b3a653510fe9fe19ee5`.

The real validation covered two model families:

| Route | Durable session | Zuno result | Provider evidence |
| --- | --- | --- | --- |
| `deep` → `claude-opus-5` | `ses_600cc6ef5e47472cab0a2087b26cd8a2` | native `read` of `Cargo.toml`, exact turn-1 marker, provider service restart, native `read` of `AGENTS.md`, exact turn-2 marker | all four requests used `responses.metadata.zuno_session_id`, `affinity_bound=true`, one stable account hash, and one stable conversation hash across the provider-process restart |
| `build` → `gpt-5.6-sol` | `ses_13ee353337a24a08b8743659ac1e7e4b` | native `read` of `Cargo.toml`, then exact marker `ZUNO_AFFINITY_GPT56_SOL_OK` | both requests used `responses.metadata.zuno_session_id`, `affinity_bound=true`, one stable account hash, one stable conversation hash, and `effort=max` |

For Opus 5, `zuno session list` still exposed the durable session after the
gateway restart, and resuming that id produced the same upstream affinity
binding. For GPT 5.6 Sol, the second tool-loop request hit both the gateway's
transport and SDK client pools. No private header or model-visible session
prefix was used for either route.

## Implemented call path

The main path is:

```text
durable SessionIdentity
  -> zuno-cli turn host
  -> zuno-engine run_turn_in_span
  -> completion_request
  -> zuno_llm::CompletionRequest + private ProviderRequestContext
  -> official OpenAI endpoint: zuno-provider-openai
     custom OpenAI baseURL: zuno-provider-compatible
  -> Responses metadata.zuno_session_id
  -> POST /v1/responses
```

Both Responses adapters emit the same standard field:

```json
{
  "metadata": {
    "zuno_session_id": "ses_..."
  }
}
```

It hashes this value together with the authenticated tenant and protocol, then
persists the resulting account/conversation binding. The raw id does not enter
Kiro input or any model-visible field.

## Implemented design

### 1. Provider-neutral request context

`CompletionRequest` carries a private typed context rather than a Kiro-specific
field or arbitrary metadata bag:

```rust
pub enum ProviderRequestContext {
    MainTurn(ProviderSessionIdentity),
    ChildTurn(ProviderSessionIdentity),
    Title,
    Summary,
    Compaction,
    Reflection,
    Council,
}
```

`ProviderSessionIdentity` validates and wraps the durable `ses_...` identifier.
It is routing state, not prompt content and not an arbitrary JSON metadata bag.
`RequestPurpose` is derived from the context for durable observability.

### 2. Consumer ownership

The turn host supplies the identity:

- every main-turn request and tool continuation uses the root durable session
  id;
- every delegated child uses its own child session id;
- resume and process restart reuse the same durable id;
- title, summary, compaction, reflection, and Council calls have an explicit
  purpose and no main-session affinity unless their design later declares an
  independent durable identity.

The engine passes this context when constructing the request. Providers do not
rediscover it from messages, prompt hashes, directories, or headers.

### 3. OpenAI Responses projection

Only an OpenAI Responses surface maps the typed identity:

```json
"metadata": { "zuno_session_id": "<durable id>" }
```

Chat Completions and Anthropic Messages do not receive a fabricated equivalent.
The official OpenAI provider and the compatible provider used by custom OpenAI
base URLs implement the same rule. Existing user/provider metadata may coexist,
but `zuno_session_id` is reserved: an attempted override returns a typed local
request error rather than replacing the durable identity.

The field is added after ordinary body construction and before sending. It
must remain absent from `input`, `instructions`, tool definitions, logs that
could cross trust boundaries, and provider headers.

### 4. Durable observability

The model-visible request remains reconstructable from durable events. Each
foreground provider-request event also records non-model-visible routing intent:

- `requestPurpose`;
- `affinityAttached`;
- `affinitySource: "durable-session"` when attached.

The event deliberately omits the raw durable id. Zuno also does not log raw
gateway keys, OpenCode credentials, replay tokens, reasoning envelopes, or
upstream account/conversation identifiers.

### 5. Effort and resume semantics

Effective effort must be part of the resolved session/agent execution state.
Resuming a session or supplying an explicit `--model` must not silently turn
`high` or `max` into `null`. The implementation should resolve one effective
model plus effort before constructing every request, persist the selection
needed for restart, and use the same rule for tool continuations.

The CLI should also reject unsupported `--variant` combinations before
starting a session with a message that names the supported alternative.

### 6. Provider lifecycle and catalog

The provider remains one long-lived OS-user service:

- Zuno never spawns one process per session;
- startup/readiness failures are reported as provider unavailability;
- cancellation remains request-scoped and does not stop the shared service;
- gateway upgrades are independent from Zuno upgrades.

Zuno's static custom-provider catalog remains authoritative for capabilities.
`models --refresh` may later import a signed/validated compatible catalog, but
must not infer attachment, reasoning, context, or output capabilities from a
model id alone.

The checked Kiro profile intentionally advertises only text and inline image
input. Although the gateway's broad model catalog can report PDF support, its
current Responses protocol rejects file inputs, remote image URLs, stateful
response fields, structured output, and native Web Search. Zuno must not turn
catalog metadata into unsupported wire fields.

The gateway also exposes one upstream text field for each projected input
message. A Zed prompt containing a `resource_link` plus user text therefore
needs an explicit compatible-provider projection rule:

```json
{
  "provider": {
    "kiro-local": {
      "transport": "openai",
      "surface": "responses",
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "maxTokens": null,
        "responsesTextBlocks": "single"
      }
    }
  }
}
```

This setting is generic and opt-in. Zuno preserves the resource link and user
text as separate durable prompt parts, then joins only their text projections
with one blank line at the compatible Responses boundary. It does not key
behavior on `kiro-local`, alter OpenAI's default request shape, flatten images,
or make the gateway limitation part of the core agent loop.

## TDD evidence

- `zuno-llm` tests cover typed purpose, identity validation, and inaccessible
  routing state.
- Engine tests prove one identity across a main tool loop, a distinct child
  identity, isolated title/summary/compaction/reflection/Council requests, and
  durable routing provenance.
- Both OpenAI Responses adapters test exact metadata projection, collision
  rejection, unrelated metadata preservation, and absence on Chat where
  applicable.
- A real CLI loopback integration captures title plus a two-request tool loop,
  proves the title is isolated, proves identical foreground metadata, and
  proves the raw identity never enters `instructions` or `input`.
- Real kiro-provider validation proves `affinity_bound=true` and stable
  account/conversation hashes for Opus 5 across a provider-process restart and
  for a GPT 5.6 Sol native tool loop.

Changing `CompletionRequest` affects all native providers and compaction
callers. Constructors should therefore be updated in one change; providers
other than OpenAI Responses explicitly ignore the new routing context.

## Completion gates

The Zuno-owned session-affinity slice is complete only when:

- Opus 5 and GPT 5.6 Sol text and tool loops pass;
- same Zuno session, including restart, logs `affinity_bound=true` and stable
  account/conversation hashes;
- different root and child sessions remain isolated;
- title, summary, compaction, reflection, and Council do not join the main
  provider conversation;
- cancellation, usage, retry classification, proxy settings, and provider
  errors continue through existing typed interfaces;
- no private header, prompt prefix, copied OAuth identity, hard-coded project
  id, or authorization bypass is introduced.

Those Zuno-owned gates passed on 2026-08-27. Stable completion of the whole
native Kiro integration has one separate upstream gate: the gateway must be
able to return to `protocol_projection_mode: "safe"` after a proven lossless
Kiro instruction projection exists.

Until that upstream item is satisfied, the supported deployment is an explicit
pre-release migration configuration, not a stable native Kiro integration.
