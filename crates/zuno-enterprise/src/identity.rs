use crate::{
    Error,
    config::{self, BrowserConfig, VerifierConfig},
    invalid,
};
use std::sync::Arc;
use zuno_identity::{
    AccessTokenVerifier, EntraVerifier, IntrospectionClientAuth, KeyCacheOptions,
    OAuth2IntrospectionVerifier, OAuth2JwtVerifier, OidcKeySource, SigningKeyCache,
    browser_login::BrowserLoginService,
    login::{CodeClientAuthMethod, HttpCodeExchange, OidcLoginClient, OidcLoginConfig},
    login_state::LoginStateCipher,
};
use zuno_postgres::PostgresBackend;
use zuno_server::enterprise_browser::EnterpriseBrowser;
use zuno_types::identity::TenantId;

pub async fn verifier(config: &VerifierConfig) -> Result<Arc<dyn AccessTokenVerifier>, Error> {
    Ok(match config {
        VerifierConfig::Jwt {
            config,
            root_certificate,
        } => {
            let root = match root_certificate {
                Some(path) => Some(config::read_file(path, 65536).await?),
                None => None,
            };
            let source = Arc::new(OidcKeySource::with_root_certificate(
                config.authority().clone(),
                root.as_deref(),
            )?);
            let keys = Arc::new(
                SigningKeyCache::new(
                    config.authority().clone(),
                    source,
                    KeyCacheOptions::default(),
                )
                .map_err(|_| invalid("invalid signing-key cache"))?,
            );
            Arc::new(OAuth2JwtVerifier::with_keys(config.clone(), keys)?)
        }
        VerifierConfig::Entra { config } => Arc::new(EntraVerifier::new(config.clone())?),
        VerifierConfig::Introspection {
            config,
            client_id,
            client_secret_file,
        } => {
            // The existing adapter owns encoding, issuer/actor validation and
            // authenticated bounded HTTP transport.
            let auth = IntrospectionClientAuth::ClientSecretBasic {
                client_id: client_id.clone(),
                client_secret: config::secret(client_secret_file).await?,
            };
            Arc::new(OAuth2IntrospectionVerifier::new(config.clone(), auth)?)
        }
    })
}

pub async fn browser(
    options: &BrowserConfig,
    backend: &PostgresBackend,
    tenant: TenantId,
    access: Arc<dyn AccessTokenVerifier>,
) -> Result<EnterpriseBrowser, Error> {
    let root = match &options.root_certificate {
        Some(path) => Some(config::read_file(path, 65536).await?),
        None => None,
    };
    let source = Arc::new(OidcKeySource::with_root_certificate(
        options.authority.clone(),
        root.as_deref(),
    )?);
    let metadata = source.discovery_document().await?;
    let keys = Arc::new(
        SigningKeyCache::new(
            options.authority.clone(),
            source,
            KeyCacheOptions::default(),
        )
        .map_err(|_| invalid("invalid browser signing-key cache"))?,
    );
    let login = OidcLoginConfig::from_metadata(
        options.authority.clone(),
        options.client_id.clone(),
        &options.redirect_uri,
        options.scopes.clone(),
        &metadata,
    )?;
    let exchange = Arc::new(HttpCodeExchange::with_root_certificate(
        CodeClientAuthMethod::Basic,
        config::secret(&options.client_secret_file).await?,
        root.as_deref(),
    )?);
    let client = Arc::new(OidcLoginClient::new(login, keys, exchange, access)?);
    let store = Arc::new(backend.browser_sessions(tenant, Default::default())?);
    let cipher = Arc::new(LoginStateCipher::new(
        options.encryption_keys.active.clone(),
        options.encryption_keys.load().await?.into_iter().collect(),
    )?);
    EnterpriseBrowser::new(Arc::new(BrowserLoginService::new(
        client,
        cipher,
        store.clone(),
        store,
    )))
    .map_err(|_| invalid("invalid BFF public origin"))
}
