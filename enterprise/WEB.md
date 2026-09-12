# Enterprise session workbench

The React and TypeScript application in `enterprise/web` consumes the same
authenticated application and activity contracts as other clients. It has no
Agent loop, Worker credentials or local command executor. Enterprise hosting
targets Linux amd64/arm64; a Windows or macOS browser can use the Linux service.

## Build and serve

```sh
npm --prefix enterprise/sdk ci --ignore-scripts
npm --prefix enterprise/web ci --ignore-scripts
npm --prefix enterprise/web run build
```

Configure the control plane's existing `browser` OIDC block and set
`service.webAssetsDirectory` to the absolute `enterprise/web/dist` directory,
or to the `web` directory extracted from a preview archive. The configured
callback remains `/auth/callback`; `/` redirects to `/app/`.
See [deployment](DEPLOYMENT.md) and [browser authentication](BROWSER.md).

The optional resource handler is mounted only when a real bundle is loaded.
Startup rejects symlinks, unsupported files and missing `index.html`. Limits are
128 files, 32 directories, 8 MiB per file and 32 MiB overall. The process serves
the bytes loaded at startup, so replacing files cannot mix versions within one
running control plane. Deploy a new bundle with a controlled process restart.
No request selects a filesystem path.

The build self-hosts Inter and Noto Sans SC fonts and includes third-party
notices at `/app/assets/licenses.txt`. Markdown loads as a separate chunk.
The response policy allows scripts, styles, fonts and API requests only from
the same origin; it does not enable inline scripts or `eval`. Assets use ETags
and revalidation, and do not contain an environment-specific token or endpoint.
Reverse proxies must preserve the configured public Host and HTTPS scheme.

## Current workbench

- Organization login/logout, configured workspace selection and owned-session
  pagination.
- Session creation, durable input admission, task interruption and exact request
  receipt lookup after a lost response. An unresolved input stays fixed until
  its original request is retried or its rejection is confirmed.
- Committed messages, tool action/source/state, collapsed provider-visible
  thinking, separate transient progress and authoritative approval details.
- A bounded 2,000-item history window. Loading older pages retains older content
  while the committed cursor advances; “return to latest” retrieves a fresh
  snapshot. Missing frames trigger authorized snapshot recovery.
- Responsive navigation/details drawers with focus containment and restoration,
  dark color tokens and reduced-motion behavior.

User/model text is rendered without raw HTML. Model-controlled external images
are not fetched; links require a user action. Encrypted reasoning, signatures,
private replay snapshots and execution grants stay on the server.
The browser keeps no access/ID token in Web storage.

Session switches abort old subscriptions. A browser-context header binds pending
requests to the tenant/principal/application loaded by the page; a changed
HttpOnly login cookie cannot silently execute an old page's request as another
account. The server compares this header to authenticated identity. The header
is a restriction and grants no permission.

Approval buttons show the current request, effect and expiry. The organization
service still decides the actor's role and approval-app eligibility at commit.
A displayed state or ACP answer does not substitute for that decision.

The workbench currently exposes sessions and their approval details. Memory
management, organization queues, Workflow/Council diagrams, learning, audit/admin,
ACP bridge and complete TUI integration remain tracked implementation work.
Their menu items are not registered before corresponding handlers exist.

## Validation and release

`npm --prefix enterprise/web test` builds the actual bundle and runs Chromium
against an explicitly separate mock API fixture. It checks 320/375/414/768/1280
widths, keyboard focus, no external image fetch, login/logout, account changes,
lost responses, history/session pagination and late approval replies.

For the actual service boundary, build the Web bundle and set
`ZUNO_ENTERPRISE_WEB_DIST` to its absolute path when running
`python3 scripts/check_enterprise_docker.py`. The native executable test starts
the TLS control plane, gateway and two Workers with PostgreSQL and rootless
Docker. Chromium then completes OIDC/PKCE login against the signed issuer
fixture, reads only its owner's history, submits a task, explicitly approves
its command and observes the continued model result. This is real service and
browser evidence using a fixture identity/model provider.

The enterprise client workflow checks generated contracts, SDK recovery and
browser behavior. Linux amd64/arm64 Docker lanes also run the native browser
scenario. The preview release downloads the client job's tested Web bundle and
packages those same bytes alongside each native executable; checksums and
provenance cover the complete archive. Stable installers and the stable docs
site are unaffected. Publication remains disabled until preview acceptance.

See [中文](WEB.zh.md), [activity protocol](ACTIVITY.md), [application API](APPLICATION.md)
and [implementation status](STATUS.md).
