//! Same-origin BFF login. Register only after a host assembles real identity,
//! encrypted transaction storage and browser-session providers.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, RawQuery, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use zuno_identity::{
    IdentityError, VerifiedIdentity, browser_login::BrowserLoginService, login::LoginError,
};
use zuno_types::identity::{ClientId, PrincipalId, TenantId};

pub const LOGIN_PATH: &str = "/auth/login";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const LOGOUT_PATH: &str = "/auth/logout";
pub const SESSION_PATH: &str = "/auth/session";
pub const CSRF_HEADER: &str = "x-zuno-csrf";
pub const LOGIN_COOKIE: &str = "__Host-zuno_preview_login";
pub const SESSION_COOKIE: &str = "__Host-zuno_preview_session";

#[derive(Clone)]
pub struct EnterpriseBrowser {
    login: Arc<BrowserLoginService>,
    origin: String,
    authority: String,
}

#[derive(Debug, thiserror::Error)]
#[error("the BFF requires an HTTPS callback at /auth/callback on its public origin")]
pub struct BrowserConfigError;

impl EnterpriseBrowser {
    pub fn new(login: Arc<BrowserLoginService>) -> Result<Self, BrowserConfigError> {
        let redirect = login.redirect_uri();
        if redirect.scheme() != "https"
            || redirect.path() != CALLBACK_PATH
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return Err(BrowserConfigError);
        }
        let origin = redirect.origin().ascii_serialization();
        let authority = origin
            .strip_prefix("https://")
            .ok_or(BrowserConfigError)?
            .to_owned();
        Ok(Self {
            login,
            origin,
            authority,
        })
    }

    pub fn router(self) -> Router {
        Router::new()
            .route(LOGIN_PATH, post(login))
            .route(CALLBACK_PATH, get(callback))
            .route(LOGOUT_PATH, post(logout))
            .route(SESSION_PATH, get(session))
            .layer(DefaultBodyLimit::max(16384))
            .layer(middleware::from_fn_with_state(
                self.clone(),
                browser_boundary,
            ))
            .with_state(self)
    }

    /// Adds identity to a fully assembled browser router. Resource handlers must
    /// still check current organization policy in their data-owner transaction.
    pub fn authenticate_routes(&self, routes: Router) -> Router {
        routes
            .layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .layer(middleware::from_fn_with_state(
                self.clone(),
                browser_boundary,
            ))
    }

    fn same_origin_write(&self, headers: &HeaderMap) -> Result<(), Failure> {
        if unique_header(headers, header::ORIGIN.as_str()) != Some(self.origin.as_str())
            || unique_header(headers, CSRF_HEADER) != Some("1")
            || unique_header(headers, "sec-fetch-site").is_some_and(|value| value != "same-origin")
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        Ok(())
    }

    async fn identity(&self, headers: &HeaderMap) -> Result<VerifiedIdentity, Failure> {
        let cookie =
            unique_cookie(headers, SESSION_COOKIE).ok_or(Failure(StatusCode::UNAUTHORIZED))?;
        self.login
            .authenticate(&cookie)
            .await
            .map_err(login_error)?
            .ok_or(Failure(StatusCode::UNAUTHORIZED))
    }
}

struct Failure(StatusCode);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let error = match self.0 {
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::SERVICE_UNAVAILABLE => "authentication_unavailable",
            _ => "invalid_login",
        };
        private((self.0, Json(serde_json::json!({"error":error}))).into_response())
    }
}

fn login_error(error: LoginError) -> Failure {
    Failure(match error {
        LoginError::Unavailable
        | LoginError::Identity(
            IdentityError::KeysUnavailable | IdentityError::IntrospectionUnavailable,
        )
        | LoginError::Exchange => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    })
}

fn private(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn browser_boundary(
    State(service): State<EnterpriseBrowser>,
    request: Request,
    next: Next,
) -> Result<Response, Failure> {
    if request.headers().get_all(header::HOST).iter().count() > 1 {
        return Err(Failure(StatusCode::BAD_REQUEST));
    }
    let authority = unique_header(request.headers(), header::HOST.as_str())
        .or_else(|| request.uri().authority().map(|value| value.as_str()));
    if authority != Some(service.authority.as_str()) {
        return Err(Failure(StatusCode::BAD_REQUEST));
    }
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        service.same_origin_write(request.headers())?;
    }
    Ok(private(next.run(request).await))
}

async fn authenticate(
    State(service): State<EnterpriseBrowser>,
    mut request: Request,
    next: Next,
) -> Result<Response, Failure> {
    let identity = service.identity(request.headers()).await?;
    const CONTEXT: &str = "x-zuno-browser-context";
    if request.headers().contains_key(CONTEXT) {
        let expected = serde_json::to_string(&[
            identity.tenant_id().as_str(),
            identity.principal_id().as_str(),
            identity.client_id().as_str(),
        ])
        .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE))?;
        if unique_header(request.headers(), CONTEXT) != Some(expected.as_str()) {
            return Err(Failure(StatusCode::UNAUTHORIZED));
        }
    }
    request.extensions_mut().insert(identity);
    Ok(next.run(request).await)
}

fn unique_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn unique_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for line in headers.get_all(header::COOKIE) {
        let line = line.to_str().ok()?;
        if line.len() > 16384 {
            return None;
        }
        for cookie in line.split(';') {
            let (key, value) = cookie.trim().split_once('=')?;
            if key == name {
                if found.is_some() || value.len() > 256 {
                    return None;
                }
                found = Some(value.to_owned());
            }
        }
    }
    found
}

fn now() -> Result<u64, Failure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE))
}

fn cookie(name: &str, value: &str, lifetime: u64, same_site: &str) -> Result<HeaderValue, Failure> {
    HeaderValue::from_str(&format!(
        "{name}={value}; Max-Age={lifetime}; Secure; HttpOnly; SameSite={same_site}; Path=/",
    ))
    .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE))
}

fn redirect(location: &str) -> Result<Response, Failure> {
    Ok((
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            HeaderValue::from_str(location)
                .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE))?,
        )],
    )
        .into_response())
}

async fn login(
    State(service): State<EnterpriseBrowser>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let start = service.login.begin().await.map_err(login_error)?;
    let wants_json = unique_header(&headers, header::ACCEPT.as_str()).is_some_and(|accept| {
        accept.split(',').any(|entry| {
            entry
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        })
    });
    let mut response = if wants_json {
        Json(serde_json::json!({"authorizationUrl":start.authorization_url.as_str()}))
            .into_response()
    } else {
        redirect(start.authorization_url.as_str())?
    };
    response.headers_mut().append(
        header::SET_COOKIE,
        cookie(
            LOGIN_COOKIE,
            start.browser_binding.expose(),
            start.expires_at_seconds.saturating_sub(now()?),
            "Lax",
        )?,
    );
    Ok(response)
}

struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
    issuer: Option<String>,
}
impl Callback {
    fn parse(query: Option<&str>) -> Result<Self, Failure> {
        let query = query
            .filter(|value| value.len() <= 32768)
            .ok_or(Failure(StatusCode::BAD_REQUEST))?;
        let mut fields = std::collections::BTreeMap::new();
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if fields
                .insert(name.into_owned(), value.into_owned())
                .is_some()
                || fields.len() > 16
            {
                return Err(Failure(StatusCode::BAD_REQUEST));
            }
        }
        let state = fields
            .remove("state")
            .ok_or(Failure(StatusCode::BAD_REQUEST))?;
        let code = fields.remove("code");
        let error = fields.remove("error");
        if code.is_some() == error.is_some() {
            return Err(Failure(StatusCode::BAD_REQUEST));
        }
        Ok(Self {
            state,
            code,
            error,
            issuer: fields.remove("iss"),
        })
    }
}

async fn callback(
    State(service): State<EnterpriseBrowser>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let result = async {
        let callback = Callback::parse(query.as_deref())?;
        let binding =
            unique_cookie(&headers, LOGIN_COOKIE).ok_or(Failure(StatusCode::BAD_REQUEST))?;
        if callback
            .issuer
            .as_deref()
            .is_some_and(|issuer| issuer != service.login.issuer())
        {
            return Err(Failure(StatusCode::BAD_REQUEST));
        }
        if callback.error.is_some() {
            service
                .login
                .cancel(&callback.state, &binding)
                .await
                .map_err(login_error)?;
            return Err(Failure(StatusCode::BAD_REQUEST));
        }
        let login = service
            .login
            .complete(
                &callback.state,
                &binding,
                callback
                    .code
                    .as_deref()
                    .ok_or(Failure(StatusCode::BAD_REQUEST))?,
            )
            .await
            .map_err(login_error)?;
        let mut response = redirect("/")?;
        response.headers_mut().append(
            header::SET_COOKIE,
            cookie(
                SESSION_COOKIE,
                login.session_token.expose(),
                login.expires_at_seconds.saturating_sub(now()?),
                "Strict",
            )?,
        );
        Ok::<_, Failure>(response)
    }
    .await;
    // An unbound cross-site callback has no authority to overwrite this
    // browser's active login cookie. Failed consumed attempts expire normally;
    // starting a fresh login replaces that short-lived binding.
    let mut response = match result {
        Ok(response) => response,
        Err(error) => return error.into_response(),
    };
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "__Host-zuno_preview_login=; Max-Age=0; Secure; HttpOnly; SameSite=Lax; Path=/",
        ),
    );
    response
}

async fn logout(
    State(service): State<EnterpriseBrowser>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    // Preserve a valid cookie during a storage outage so the caller can retry.
    if let Some(cookie) = unique_cookie(&headers, SESSION_COOKIE) {
        service.login.logout(&cookie).await.map_err(login_error)?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            "__Host-zuno_preview_session=; Max-Age=0; Secure; HttpOnly; SameSite=Strict; Path=/",
        )],
    )
        .into_response())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdentity {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    client_id: ClientId,
    expires_at_seconds: u64,
}

async fn session(
    State(service): State<EnterpriseBrowser>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, Failure> {
    let identity = service.identity(&headers).await?;
    Ok(Json(SessionIdentity {
        tenant_id: identity.tenant_id().clone(),
        principal_id: identity.principal_id().clone(),
        client_id: identity.client_id().clone(),
        expires_at_seconds: identity.expires_at_seconds(),
    }))
}
