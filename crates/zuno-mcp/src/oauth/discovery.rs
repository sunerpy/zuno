use reqwest::Url;
use zuno_config::schema::mcp::McpRemote;

use crate::body::{BodyError, MAX_OAUTH_BODY_BYTES, read_bounded_json};
use crate::remote::RemoteError;

use super::support::{challenge_parameter, parse_url, server_origin};
use super::{AuthorizationServerMetadata, Discovery, ProtectedResourceMetadata};

pub(super) async fn discover(
    server: &str,
    config: &McpRemote,
    http: &reqwest::Client,
    challenge: Option<&str>,
) -> Result<Discovery, RemoteError> {
    let server_url = parse_url(server, &config.url, "server URL")?;
    let challenge_metadata =
        challenge.and_then(|value| challenge_parameter(value, "resource_metadata"));
    let (resource_candidates, resource_origin) = if let Some(metadata) = challenge_metadata {
        (
            vec![parse_url(server, &metadata, "resource metadata URL")?],
            CandidateOrigin::PeerChosen,
        )
    } else {
        (
            protected_resource_candidates(&server_url),
            CandidateOrigin::ClientDerived,
        )
    };
    let mut resource = None;
    for candidate in resource_candidates {
        let response = match http.get(candidate).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(_) | Err(_) => continue,
        };
        match read_bounded_json::<ProtectedResourceMetadata>(response, MAX_OAUTH_BODY_BYTES).await {
            Ok(metadata) => {
                resource = Some(metadata);
                break;
            }
            // A candidate that answers with garbage, or whose body dies in transit, is
            // one this client moves past — that is what a candidate list is for. Whether
            // an oversized body joins them depends on who chose the URL: see
            // [`skip_or_fail`] and [`CandidateOrigin`].
            Err(error) => skip_or_fail(
                server,
                "protected-resource metadata",
                resource_origin,
                &error,
            )?,
        }
    }
    let resource = resource.unwrap_or_else(|| ProtectedResourceMetadata {
        resource: config.url.clone(),
        authorization_servers: vec![server_origin(&server_url)],
        scopes_supported: Vec::new(),
    });
    let authorization_server = resource
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| server_origin(&server_url));
    let authorization_server = parse_url(server, &authorization_server, "authorization server")?;
    let mut authorization = None;
    for candidate in authorization_metadata_candidates(&authorization_server) {
        let response = match http.get(candidate).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(_) | Err(_) => continue,
        };
        match read_bounded_json::<AuthorizationServerMetadata>(response, MAX_OAUTH_BODY_BYTES).await
        {
            Ok(metadata) => {
                authorization = Some(metadata);
                break;
            }
            // Always client-derived: this loop only ever asks the two `.well-known`
            // paths, and it asks them of a host that is either the configured server's
            // own origin or one named in a document — never a whole URL a peer handed
            // over. A large page on a guessed path says nothing about intent, and
            // discovery cannot silently downgrade by skipping one: with no
            // authorization metadata at all the flow still fails below.
            Err(error) => skip_or_fail(
                server,
                "authorization server metadata",
                CandidateOrigin::ClientDerived,
                &error,
            )?,
        }
    }
    let authorization = authorization.ok_or_else(|| RemoteError::OAuth {
        server: server.to_owned(),
        message: "authorization server metadata discovery failed".to_owned(),
    })?;
    Ok(Discovery {
        resource,
        authorization,
    })
}

/// Who chose the URL a metadata candidate was fetched from.
///
/// This decides what an oversized body means, and it is the whole difference between a
/// diagnosis and a denial of service. A URL the peer handed over in its
/// `WWW-Authenticate` challenge is one where it chose both the address and the size, so
/// a body past [`MAX_OAUTH_BODY_BYTES`] is its own doing and ending the flow says so
/// instead of quietly falling back to a default that looks like it worked.
///
/// A URL this client derived from configuration is a *guess*. A `.well-known` path
/// behind a catch-all — an SPA rewrite, a portal, a proxy error page — answers 200 with
/// a page that has nothing to do with OAuth, and its size is evidence of nothing. That
/// case used to end login permanently while the identical non-JSON page one byte under
/// the bound was skipped and discovery continued, which made a hard, user-visible
/// failure key purely on a number the peer picked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CandidateOrigin {
    /// The peer named this URL in its authentication challenge.
    PeerChosen,
    /// This client built the URL from the configured server URL.
    ClientDerived,
}

/// Fails closed for a peer-named candidate past the byte bound, and otherwise lets the
/// caller move to the next candidate.
///
/// Written as a helper returning `Result<(), _>` so both discovery loops share one
/// decision. A malformed or interrupted body says nothing about intent and is skipped;
/// an oversized one is skipped too unless the peer chose the URL as well as the size
/// (see [`CandidateOrigin`]). A skipped oversize is logged at `warn` naming the bound,
/// because unlike garbage it is a fact an operator may need in order to explain why a
/// server's own metadata was ignored.
fn skip_or_fail(
    server: &str,
    what: &str,
    origin: CandidateOrigin,
    error: &BodyError,
) -> Result<(), RemoteError> {
    if error.is_too_large() {
        if origin == CandidateOrigin::PeerChosen {
            return Err(RemoteError::OAuth {
                server: server.to_owned(),
                message: error.describe(what),
            });
        }
        tracing::warn!(
            server,
            what,
            // Not `message`: that field name is the one `zuno_observability` leaves
            // readable and prints with no `name=` prefix, so it reads as the event's own
            // sentence. This value embeds a `reqwest` error that can name a peer-supplied
            // URL.
            reason = %error.describe(what),
            "skipping an oversized OAuth metadata candidate this client derived itself"
        );
        return Ok(());
    }
    tracing::debug!(server, what, reason = %error.describe(what), "skipping an OAuth metadata candidate");
    Ok(())
}

fn protected_resource_candidates(server: &Url) -> Vec<Url> {
    let origin = server_origin(server);
    let path = server.path().trim_start_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty()
        && let Ok(url) = Url::parse(&format!(
            "{origin}/.well-known/oauth-protected-resource/{path}"
        ))
    {
        candidates.push(url);
    }
    if let Ok(url) = Url::parse(&format!("{origin}/.well-known/oauth-protected-resource")) {
        candidates.push(url);
    }
    candidates
}

fn authorization_metadata_candidates(server: &Url) -> Vec<Url> {
    let origin = server_origin(server);
    let path = server.path().trim_matches('/');
    let suffix = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    [
        format!("{origin}/.well-known/oauth-authorization-server{suffix}"),
        format!("{origin}/.well-known/openid-configuration{suffix}"),
    ]
    .into_iter()
    .filter_map(|value| Url::parse(&value).ok())
    .collect()
}
