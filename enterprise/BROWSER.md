# Enterprise browser authentication

The BFF uses the [general OAuth2/OIDC adapter](AUTHENTICATION.md). Entra is one
provider configuration. `EnterpriseBrowser` has real login, callback, session
and logout handlers; it is a host assembly component, not a registered
enterprise CLI command. Runtime/profile startup and the React application remain
separate work recorded in [STATUS.md](STATUS.md).

## Ports and ownership

| Component | Responsibility |
| --- | --- |
| `OidcLoginClient` | Authorization code, PKCE S256, nonce, issuer/client binding and verified API identity |
| `AuthorizationCodeExchange` | One bounded token POST; the HTTPS implementation supports client secret Basic or Post |
| `LoginStateCipher` | AES-256-GCM encryption with random nonces, authenticated coordinates and explicit retained-key rotation |
| `LoginTransactionStore` | Insert encrypted state and atomically consume it before network I/O |
| `BrowserSessionStore` | Create, look up and revoke opaque browser sessions |
| `BrowserLoginService` | Compose these ports without depending on a database or HTTP framework |
| `EnterpriseBrowser` | Same-origin HTTP boundary, protected cookies and identity extraction |
| `PostgresBrowserStore` | Tenant-bound storage, database time, capacities and atomic session/audit writes |

The BFF and data owner hold identity credentials and encryption keys. Workers,
command containers and public DTOs never receive them. The PostgreSQL adapter
stores encrypted PKCE/state/nonce transactions, and only SHA-256 digests of
random 256-bit session credentials. Access, ID and refresh tokens are not stored
in browser-session rows or returned to the browser.

A callback can reach a different BFF replica from the one that started login.
Replicas share the fixed tenant, validated provider configuration, PostgreSQL
state and encryption keyring. Consuming the state commits before exchanging
the code. Concurrent callbacks cannot both exchange it. A wrong browser binding
does not consume the legitimate browser's transaction.

## Trusted configuration

Use an exact HTTPS issuer and callback such as
`https://agent.example.test/auth/callback`. Discovery is bounded, requires the
exact issuer, and cannot redirect. Login endpoints default to the issuer's
origin. `additionalEndpointOrigins` explicitly permits an IdP's separate
authorization/token origin; metadata cannot extend this list. JWKS origins have
their own `additionalJwksOrigins` setting.

`OidcLoginOptions` is a validated library configuration document:

```json
{
  "transactionLifetimeSeconds": 300,
  "sessionLifetimeSeconds": 3600,
  "clockSkewSeconds": 30,
  "maxAuthenticationAgeSeconds": null,
  "additionalEndpointOrigins": []
}
```

Transactions allow 30–600 seconds, sessions 60–86,400 seconds, and skew 0–120
seconds. A configured authentication age allows 1–86,400 seconds and sends
`max_age`, then requires a matching `auth_time`. The effective session expiry
cannot exceed either verified token or the configured session cap.

The host resolves the client secret and 32-byte encryption keys separately.
The keyring retains at most eight explicitly named keys; new transactions use
the selected current key. Keep old keys only for outstanding short-lived
transactions. Identity HTTP clients accept an administrator-supplied private CA,
retain hostname verification and never accept a trust root from a request.

The login scopes must contain `openid` and the API's required delegated scope.
The ID token is checked for issuer, audience, nonce, time, authorized party and,
when present, access-token hash. The resource token independently passes the
configured API verifier and must identify a delegated user and the exact OAuth
client used for login. Generic adapters retain both the issuer-native client
proof and the distinct issuer-scoped application ID used by organization policy.
Comparing the raw client ID to that internal digest is incorrect.

These documents are not stable `zuno.json` keys. A host constructs the client,
stores and service explicitly; unsupported identity protocols are not enabled
through fallback. An OAuth2 provider without OIDC may authorize API access but
does not supply browser identity proof through this login flow.

## HTTP contract

| Request | Behavior |
| --- | --- |
| `POST /auth/login` | Persist a login transaction, set its binding cookie, redirect with code + PKCE parameters |
| `GET /auth/callback` | Validate the bound state, consume it once, exchange and verify tokens, create an opaque session, redirect to `/` |
| `GET /auth/session` | Return the authenticated tenant, principal, application and expiry |
| `POST /auth/logout` | Revoke the current session and expire its cookie |

The Host must match the configured public authority. Forwarded headers cannot
choose an origin. Mutations require the exact configured `Origin` and
`X-Zuno-CSRF: 1`; when supplied, `Sec-Fetch-Site` must be `same-origin`.
There is no credentialed CORS relaxation. The login callback instead uses its
server-owned state and browser binding, because it returns from the IdP.

Cookies use `__Host-zuno_preview_login` and `__Host-zuno_preview_session`,
`Secure`, `HttpOnly` and `Path=/`, with no Domain. The short-lived login cookie
uses `SameSite=Lax` for the top-level authorization response; the session cookie
uses `SameSite=Strict`. Only the latest login started in a browser has its
current binding cookie. Failed/unbound callbacks cannot overwrite that cookie;
successful completion expires it.

Responses are `Cache-Control: no-store`, `Pragma: no-cache`,
`Referrer-Policy: no-referrer` and `X-Content-Type-Options: nosniff`. Duplicate
callback fields/cookies and mismatched callback issuers are rejected. There are
no caller-selected return URLs. Configure reverse-proxy access logs to omit the
callback query, which contains the authorization code.

`authenticate_routes` attaches a verified identity to a fully assembled browser
router. Resource handlers must still use the current organization policy and
transactional authorization; this middleware does not grant tool or Memory
permissions. External bearer API and internal Worker routes use their own
authentication surfaces.

## Recovery and validation

Token exchange does not follow redirects or retry POSTs. A lost/failed response
requires a new login. It cannot replay the consumed authorization attempt.
Browser-state outages return a typed unavailable response, not an unauthenticated
success or a guessed logout; a failed logout keeps the cookie for retry.

Sessions expire at their bounded deadline and support explicit local logout.
This phase does not implement refresh tokens, IdP back-channel logout or a live
Entra app registration. Current organization authorization is independent of
upstream token expiry. Live-provider and real browser UI validation remain
separate acceptance evidence.

Run `cargo test -p zuno-identity` and
`python3 scripts/check_enterprise_postgres.py`. The latter uses temporary TLS
certificates, a real HTTPS OIDC issuer fixture with RSA signatures, separate BFF
instances and PostgreSQL. It verifies generic provider login, PKCE, no token
POST replay, wrong-client and unbound-callback boundaries, private sessions,
CSRF, logout and format-6 migration preservation/rollback. This is network and
storage evidence; the issuer is a fixture, not a live enterprise tenant.

See [中文](BROWSER.zh.md), [PostgreSQL](POSTGRES.md) and
[authorization](AUTHORIZATION.md).
