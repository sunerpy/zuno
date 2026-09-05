# Provider authentication

Status: 2026-09-05.

Provider authentication has three separate roles. A feature is complete only when all three are present:

1. `AuthStore` persists a typed credential without knowing how it was obtained or used.
2. `LoginMethodRegistry` declares the user-selectable ways a provider can obtain a credential.
3. The provider implementation consumes that credential, including provider-specific refresh, endpoint, and header behavior.

Keeping these roles separate prevents the existence of an `oauth` JSON variant from being mistaken for an implemented OAuth integration.

## Identity, transport, and authentication

A provider id is the configuration and credential identity, for example `openai` or `myopenai`. `provider.<id>.transport` selects a native Rust wire implementation. It does not change the provider id and does not select authentication.

Consequently, a custom provider using `transport: "openai"` gets OpenAI request and stream encoding but does not inherit the official `openai` provider's ChatGPT login. The same transport can safely serve multiple provider ids with different credentials and endpoints.

## Credential resolution

A turn resolves one credential in this order:

1. `provider.<id>.options.apiKey`;
2. the provider-id entry in `AuthStore`;
3. the first non-empty variable named by `provider.<id>.env`;
4. no credential.

An explicitly empty `options.apiKey` is intentional and prevents fallback. This lets a local endpoint declare that it accepts no key without accidentally receiving a stored vendor credential.

Environment credentials remain process inputs. They are not copied into the credential file. Stored credentials live at `$XDG_DATA_HOME/zuno/auth.json`, normally `~/.local/share/zuno/auth.json`. `ZUNO_AUTH_CONTENT` supplies an in-memory read override for managed environments.

### Amazon Bedrock is an AWS SDK credential consumer

Bedrock does not use the generic API-key ladder above when SigV4 is selected.
`zuno-aws-auth` follows Codex's `codex-aws-auth` structure and delegates to
`aws-config` and `aws-sigv4`:

- `amazon-bedrock` signs Mantle Responses with service `bedrock-mantle`;
- `amazon-bedrock-runtime` signs Runtime Responses with service `bedrock`;
- `amazon-bedrock-converse` signs ConverseStream with service `bedrock`.

An explicitly configured `profile` uses the SDK's profile-only credential
provider, so ambient `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` values
cannot silently select another account. Without an explicit profile, the AWS SDK
default chain owns environment credentials, shared profiles,
`credential_process`, IAM Identity Center, web identity, container credentials,
IMDS, refresh, and expiration. An explicit provider region is passed to the SDK
and therefore outranks environment and profile region sources.

The provider crates retain no hand-written credential chain or SigV4 signer.
They construct the protocol body and endpoint, ask `zuno-aws-auth` to sign the
exact bytes, and send the signed request through `zuno-network`.

## Credential file integrity

The credential file is durable user state, so `zuno-auth` owns three properties above a
plain serialize-and-write path.

**Publication is visibility-atomic, and crash-bounded on Unix only.** A write goes to a
temporary sibling created with `O_EXCL` at `0600`, is `fsync`ed, and is renamed over the
target; the containing directory is then `fsync`ed so the name transition itself
survives a power loss. Off Unix the publication is `zuno_atomic_file::replace`
(`ReplaceFileW`, chosen because `MoveFileEx` replacement is not gap-free), which
deliberately does not promise crash durability, and its temporary file is private to
that crate. `zuno-auth` adds the only narrowing reachable from outside the primitive: it
`sync_all`s the *published* file afterwards, best effort, logged rather than returned
because the document is already published. That moves the exposure from "the whole
document may never have reached the device" to "the interval between the name transition
and that flush may not have". Windows therefore has a weaker crash boundary than Unix
here; closing it needs a durable publication in `zuno-atomic-file`.

**Damage is data, not an error.** A file that exists and holds no store decodes to the
default store carrying a `StoreDamage`, and every read surface keeps working. There is
no `AuthError` variant for it: a store that returns `Err` for a zero-byte file denies
the login that would repair it, while leaving the file as broken as it found it, and a
file holding no bytes holds no credential a write could destroy. A file with *content*
this build cannot parse is still refused (`AuthError::Malformed`), because those bytes
may be a recoverable store. The damage report is latched once per `(kind, path)` per
process, so a per-request read path cannot bury it, and the same finding is returned as
data for surfaces whose log sink is off.

**Forward compatibility is preserved at two granularities.** An entry that does not
decode is kept verbatim and republished by every write, and is resolvable and removable
through `names`, `contains`, `is_preserved` and `remove`. A field that no modelled shape
names is carried per entry and merged back on write. The key list is declared on each
credential type rather than derived from serialization: a derived list cannot tell a
field this build never knew about from a field the user just cleared, and would
resurrect the cleared value. `store::settle_unmodelled` reconciles the carry against
what a mutation actually did — a carried key whose entry is gone goes with it, and a
carried key that lived inside a modelled object the write replaced is not re-attached to
the value that replaced it, because an unknown key inside `tokens` is a claim about the
tokens that were there.

**Absence is confirmed, not assumed.** A read whose result will be written back treats a
failed open that Windows cannot distinguish from a replacement in flight as no answer at
all, and re-probes for `ABSENCE_CONFIRMATION` (6 ms) before concluding the file is
absent; a file that is present but still unopenable is `AuthError::Unresolved` rather
than an empty store. The bound is owned here rather than inherited from
`zuno_atomic_file::metadata`, whose one-second expected-presence budget would be spent
in full on the ordinary first login, in `thread::sleep`, inside a synchronous function
that async callers reach.

## Method registration and catalog availability

Every login method is an explicit registration. The shipped registry gives the
official `openai` id `api-key`, `chatgpt-browser`, and `chatgpt-device`.
Configured provider instances receive `api-key` only when their resolved models
use a native transport that consumes stored API keys. Ambient-credential
transports such as Bedrock and Vertex do not advertise a login method.

Catalog resolution receives the same registry. A stored OAuth credential makes a provider selectable only when that exact provider id has a native OAuth method. This joins the interface, provider, and consumer at the composition boundary:

- `openai` plus a ChatGPT OAuth credential is selectable;
- `myopenai` plus the same OAuth-shaped credential is not granted OpenAI OAuth behavior;
- `myopenai` with a config block, API key, or declared environment key remains selectable normally.

A future custom OAuth component must register its methods, implement authorization and refresh, and consume the resulting credential in its provider. Adding only a config value or credential shape is insufficient.

## CLI selection

`zuno auth login` owns a short-lived terminal picker rather than borrowing the
resident TUI or putting selection policy in the provider registry. The official
`openai` id is always present. Other rows require both a configured, selectable
model route and an explicitly registered login method. A catalog entry or a
stored credential alone never creates a row, and there is no `Other` escape
hatch that stores an unusable credential. `enabled_providers` and
`disabled_providers` are applied before the list is rendered.

When the selected provider has several registered methods, a second picker
selects the method. Both pickers support arrows, paging, type-to-filter, Enter,
and Escape/Ctrl+C cancellation. They are entered only when standard input and
standard error are terminals. A redirected invocation remains deterministic:
the provider must be explicit, and piped standard input selects its registered
API-key method. An unsupported or unconfigured id fails before standard input is
read or the credential file is changed.

## OpenAI authentication

The official `openai` provider supports two independent families:

- Platform API key: read from config, storage, or environment and sent to the configured OpenAI API endpoint.
- ChatGPT OAuth: browser authorization with loopback PKCE or device-code authorization for headless hosts.

Browser authorization binds the allowlisted loopback ports `1455` and `1457`, verifies the callback state, and exchanges the code with its PKCE verifier. Device authorization prints the verification URL and one-time code, then polls until authorization completes or the bounded deadline expires.

The stored OAuth credential contains access, refresh, expiry, and optional account metadata. Before each request, the OpenAI provider refreshes a token within the expiry skew. A successful rotation updates the in-memory credential and `AuthStore`; an active `ZUNO_AUTH_CONTENT` override is never rewritten.

ChatGPT OAuth is accepted only for the Responses surface. Its request uses the ChatGPT Codex backend, includes `ChatGPT-Account-Id` when the token identifies an account, and forwards a constrained compute-residency claim when one is present. Custom OpenAI-wire providers treat a manually supplied OAuth access value as an ordinary bearer and do not receive ChatGPT endpoint or refresh semantics.

## Product-agent authentication is separate

Configured Codex and Claude Code subagents are not model providers and never participate in `zuno auth login`. Zuno starts the host-installed command and inherits that product's own configuration and login state. It does not read, translate, refresh, or copy those tokens into `AuthStore`, and it does not choose a product model from the Zuno catalog.

This boundary is intentional: `openai` provider OAuth authenticates Zuno's in-process OpenAI request implementation, while a Codex product agent authenticates the separate Codex installation and app-server process. See [Codex and Claude Code product agents](product-agents.md).

## Security properties

- Credential files are created with mode `0600` on Unix and repaired to that mode on later writes.
- Secret fields use a redacted `Debug` and `Display`; access requires the explicit `Secret::expose` boundary.
- Browser state and PKCE verifier values are omitted from debug output.
- API-key login disables terminal echo interactively and reads standard input in pipelines, avoiding command-line arguments and shell history.
- OAuth transport and protocol failures remain typed so retry policy does not depend on rendered text.
- AWS SDK targets that may log access-key identifiers or signing internals are
  forced to `WARN` even when the user requests global `DEBUG` or `TRACE`.

Windows uses inherited filesystem ACLs because Unix mode bits are unavailable. An OS keyring is not currently an authentication backend; the durable backend is the protected Zuno credential file.
