# Provider authentication

Status: 2026-08-22.

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

## Method registration and catalog availability

Every provider has the generic `api-key` method. Provider-specific methods are explicit registrations. The shipped registry adds `chatgpt-browser` and `chatgpt-device` only to the `openai` id.

Catalog resolution receives the same registry. A stored OAuth credential makes a provider selectable only when that exact provider id has a native OAuth method. This joins the interface, provider, and consumer at the composition boundary:

- `openai` plus a ChatGPT OAuth credential is selectable;
- `myopenai` plus the same OAuth-shaped credential is not granted OpenAI OAuth behavior;
- `myopenai` with a config block, API key, or declared environment key remains selectable normally.

A future custom OAuth component must register its methods, implement authorization and refresh, and consume the resulting credential in its provider. Adding only a config value or credential shape is insufficient.

## OpenAI authentication

The official `openai` provider supports two independent families:

- Platform API key: read from config, storage, or environment and sent to the configured OpenAI API endpoint.
- ChatGPT OAuth: browser authorization with loopback PKCE or device-code authorization for headless hosts.

Browser authorization binds the allowlisted loopback ports `1455` and `1457`, verifies the callback state, and exchanges the code with its PKCE verifier. Device authorization prints the verification URL and one-time code, then polls until authorization completes or the bounded deadline expires.

The stored OAuth credential contains access, refresh, expiry, and optional account metadata. Before each request, the OpenAI provider refreshes a token within the expiry skew. A successful rotation updates the in-memory credential and `AuthStore`; an active `ZUNO_AUTH_CONTENT` override is never rewritten.

ChatGPT OAuth is accepted only for the Responses surface. Its request uses the ChatGPT Codex backend, includes `ChatGPT-Account-Id` when the token identifies an account, and forwards a constrained compute-residency claim when one is present. Custom OpenAI-wire providers treat a manually supplied OAuth access value as an ordinary bearer and do not receive ChatGPT endpoint or refresh semantics.

## Security properties

- Credential files are created with mode `0600` on Unix and repaired to that mode on later writes.
- Secret fields use a redacted `Debug` and `Display`; access requires the explicit `Secret::expose` boundary.
- Browser state and PKCE verifier values are omitted from debug output.
- API-key login disables terminal echo interactively and reads standard input in pipelines, avoiding command-line arguments and shell history.
- OAuth transport and protocol failures remain typed so retry policy does not depend on rendered text.

Windows uses inherited filesystem ACLs because Unix mode bits are unavailable. An OS keyring is not currently an authentication backend; the durable backend is the protected Zuno credential file.
