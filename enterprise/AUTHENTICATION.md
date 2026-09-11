# Enterprise identity adapters

`zuno-identity` defines `AccessTokenVerifier -> VerifiedIdentity`. Application
hosts depend on that interface. OAuth2 JWT validation, RFC 7662 introspection,
and Entra claim validation implement it; provider credentials remain in
`zuno-auth`.

This is the 2026-09-11 amendment to [the approved plan](PLAN.zh.md): authentication
uses a general OAuth2/OIDC boundary, with Entra as the first provider-specific
adapter. The plan's original text remains available unchanged.

The library is implemented independently of an HTTP entry point. The examples
below are its configuration documents, **not registered stable `zuno.json` keys**.
The enterprise BFF, login callback/session store, current organization policy,
and operation approvals are subsequent integration work. A valid API identity
does not certify those features.

## Protocol boundaries

OAuth2 authorizes access to a resource API. OIDC adds authentication and identity
tokens for interactive login. The browser BFF must own authorization-code + PKCE,
state/nonce validation, exact redirect URIs and protected cookies. An ID token is
not an API access token; a Graph token is not a Zuno API token.

The API adapter is selected by trusted deployment configuration. A JWT validation
failure never falls through to introspection. Access-token shape is not a
mechanism for choosing a more permissive validator.

| Adapter                | Implemented requirements                                                                                                              |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| Generic JWT, RFC 9068  | RS256; `at+jwt` or `application/at+jwt`; issuer, audience, expiry, `sub`, `client_id`, `iat`, `jti`, configured scope and actor proof |
| Generic provider JWT   | RS256; `JWT`; the same trust/lifetime boundaries plus an explicit access-token marker and claim mapping                               |
| RFC 7662 introspection | Authenticated HTTPS POST; `active=true`; API audience, expiry, stable subject, allowed client, scope and actor proof                  |
| Entra v2               | Fixed tenant authority; API application GUID audience; `tid + oid`; allowed `azp`; explicit delegated-scope or app-only role policy   |

RS256 is the implemented JWT algorithm. Symmetric, unsigned, encrypted and
sender-constrained/DPoP tokens are not accepted by these bearer adapters.
Additional algorithms and proof mechanisms need their own verified provider
support. Unknown critical JWT headers, token-supplied key URLs and inline JWKs
are rejected before key retrieval.

## Generic provider configuration

```json
{
  "authority": {
    "issuer": "https://identity.example.test/realms/company"
  },
  "claims": {
    "tenantId": "company",
    "audience": "https://zuno.example.test/api",
    "allowedClients": ["company-web"],
    "requiredScopes": ["session:access"],
    "principalKind": "user",
    "actorClaim": { "claim": "principal_type", "value": "user" }
  },
  "profile": { "type": "rfc9068" }
}
```

`principal_type` is an **example issuer-managed claim**, not an OAuth2 standard.
Choose a claim the configured authorization server actually guarantees. OAuth2
does not define a universal claim that distinguishes users from service
principals. Both actor proof and scopes/allowed clients are required; do not
invent a fallback based on email, names or a missing claim. Configure workload
admission separately.

For a provider JWT, select `{"type":"provider","accessTokenClaim":{"claim":
"token_use","value":"access"}}` only when that issuer supplies such a claim.
`scopeClaim` and `clientClaim` may name the provider's top-level string fields.
RFC 9068 always uses `scope` and `client_id`; scopes are space-separated,
case-sensitive tokens.

The generic data owner is the configured enterprise tenant plus a domain-framed
SHA-256 identity over issuer, opaque `sub`, and subject kind. Calling clients
have a separate issuer-scoped digest. Different issuers, user/workload kinds,
case changes and delimiter-containing subjects cannot alias through string
concatenation. Email/display-name changes do not change ownership. Treat an
issuer change as an identity-migration decision, not a cosmetic config edit.

## Discovery and key rotation

`OAuth2Authority` pins the expected issuer. It supports OIDC discovery or a
configured `jwksUrl` for an OAuth2 server without OIDC discovery. All endpoints
require HTTPS without URL credentials or fragments. Discovery must return the
exact configured issuer.

JWKS origins default to the issuer's origin. An administrator may add at most
eight explicit `additionalJwksOrigins` when an IdP publishes keys elsewhere.
Metadata cannot add an origin itself. Redirects are disabled; requests, documents
and key counts are bounded. Generic JWK `use` and issuer metadata are optional
as the standards allow; contradictory key purposes, duplicate IDs, private key
parameters and malformed RSA keys are refused.

`SigningKeyCache` coordinates one refresh at a time. By default, keys expire
after one hour and unknown-key refreshes have a global 60-second cooldown.
Cancelling/failing a refresh retains that cooldown. A slow unknown-key refresh
does not block still-valid known-key validation. Expired keys fail closed if
discovery cannot refresh them.

## Introspection

Configure `OAuth2IntrospectionConfig` with the expected `issuer`, explicit HTTPS
`endpoint`, and the same `claims` policy. The runtime resolves
`IntrospectionClientAuth` from its secret store, separately from serializable
configuration. `client_secret_basic` and `client_secret_post` are implemented;
the former applies the OAuth2 form encoding before HTTP Basic.

The token is sent in a form body with `token_type_hint=access_token`, never in a
URL. Redirects are disabled and request/response sizes and timeouts are bounded.
No positive introspection cache is used: each verification checks current
activity. An inactive token is rejected; transport/server failure is a typed
availability error, not authentication success.

RFC 7662 makes several response fields optional. This enterprise adapter requires
`aud`, `exp`, `sub`, allowed client, scopes and configured actor proof before it
can establish an API identity. If the response includes `iss`, it must match the
configured issuer. A provider unable to supply these facts needs an explicit
mapping integration; missing authorization facts are never guessed.

## Entra specialization

Entra shares the generic JWT validator, discovery transport and key cache. Its
adapter adds:

- Tenant-specific Microsoft public-cloud v2 authority and GUID validation.
- Exact Entra JWKS endpoint paths and tenant/key issuer-template checks.
- API app GUID audience; the app registration must request v2 access tokens.
- Immutable `tid + oid` ownership and calling-app `azp` allowlist.
- Delegated `scp` requirements for users; app-only `idtyp=app` and role
  requirements for workers. Neither token kind substitutes for the other.

Custom signing-key/application queries, national-cloud authorities and v1 tokens
are not silently mapped onto this profile. They require deliberate adapters.
Live Entra login and app registration validation still require test tenant/app
configuration; synthetic signature tests are not live-tenant evidence.

## Authorization and verification

`VerifiedIdentity` has no public constructor or Deserialize implementation.
It contains no bearer token. Only the verified provider can create it. Calling
`attribution` records a policy revision obtained from the host; neither the JWT
nor the browser chooses that revision.

Organization authorization must be checked at admission and again at execution.
OAuth scopes and token roles never bypass the resource ACL, Memory authority,
HITL, worker lease, or environment isolation requirements.

Run `cargo test -p zuno-identity`. Tests use ephemeral real RSA signatures and
cover wrong audiences/issuers/clients, forged signatures, token-kind confusion,
expiry, Entra/other-issuer ownership, key rotation/cancellation/concurrency,
introspection revocation/unavailability and credential encoding/redaction.

Reference specifications: [OAuth2](https://www.rfc-editor.org/rfc/rfc6749),
[JWT access-token profile](https://www.rfc-editor.org/rfc/rfc9068),
[introspection](https://www.rfc-editor.org/rfc/rfc7662),
[OIDC discovery](https://openid.net/specs/openid-connect-discovery-1_0.html),
and [Entra claim validation](https://learn.microsoft.com/en-us/entra/identity-platform/claims-validation).
See [中文](AUTHENTICATION.zh.md) and [status](STATUS.md).
