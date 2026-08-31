use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use tower::ServiceExt;
use zuno_server::{
    AuthConfig, Delivery, EventFanout, RequestDirectory, ServerBuilder, ServerConfig, ServerError,
};

fn request(uri: &str, authorization: Option<&str>) -> Request<Body> {
    let mut request = Request::builder().uri(uri);
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    request
        .body(Body::empty())
        .expect("the test request is valid")
}

#[tokio::test]
async fn core_no_password_accepts_requests_without_credentials() {
    for password in [None, Some(String::new())] {
        let config = ServerConfig::default().with_auth(AuthConfig::new(password, None));
        let response = ServerBuilder::new(config)
            .router()
            .oneshot(request("/health", None))
            .await
            .expect("the health handler responds");
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn core_non_empty_password_requires_matching_basic_credentials_on_every_route() {
    let config =
        ServerConfig::default().with_auth(AuthConfig::new(Some("secret".to_owned()), None));
    let app = ServerBuilder::new(config).router();

    let missing = app
        .clone()
        .oneshot(request("/health", None))
        .await
        .expect("the middleware responds");
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        missing.headers()[header::WWW_AUTHENTICATE],
        "Basic realm=\"Secure Area\""
    );

    let wrong_username = app
        .clone()
        .oneshot(request(
            "/health",
            Some("Basic c29tZW9uZS1lbHNlOnNlY3JldA=="),
        ))
        .await
        .expect("the middleware responds");
    assert_eq!(wrong_username.status(), StatusCode::UNAUTHORIZED);

    let authorized = app
        .oneshot(request("/health", Some("Basic enVubzpzZWNyZXQ=")))
        .await
        .expect("the health handler responds");
    assert_eq!(authorized.status(), StatusCode::OK);
}

#[tokio::test]
async fn core_directory_header_and_query_forms_reach_extension_routes() {
    async fn show_directory(Extension(directory): Extension<RequestDirectory>) -> String {
        directory.into_string()
    }

    let routes = Router::new().route("/core/directory", get(show_directory));
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/fallback"))
        .with_routes(routes)
        .router();

    let from_header = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/core/directory")
                .header("x-zuno-directory", "%2Fworkspace%2Fheader")
                .body(Body::empty())
                .expect("the header request is valid"),
        )
        .await
        .expect("the directory route responds");
    assert_eq!(
        axum::body::to_bytes(from_header.into_body(), usize::MAX)
            .await
            .expect("the body is readable"),
        "/workspace/header"
    );

    let from_query = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/core/directory?directory=%252Fworkspace%252Fquery")
                .header("x-zuno-directory", "%2Fworkspace%2Fheader")
                .body(Body::empty())
                .expect("the query request is valid"),
        )
        .await
        .expect("the directory route responds");
    assert_eq!(
        axum::body::to_bytes(from_query.into_body(), usize::MAX)
            .await
            .expect("the body is readable"),
        "/workspace/query",
        "the query form has the oracle's precedence over the header form"
    );

    let fallback = app
        .oneshot(request("/core/directory", None))
        .await
        .expect("the directory route responds");
    assert_eq!(
        axum::body::to_bytes(fallback.into_body(), usize::MAX)
            .await
            .expect("the body is readable"),
        "/fallback"
    );
}

#[tokio::test]
async fn core_non_loopback_without_password_is_refused_before_binding() {
    let error = match ServerBuilder::new(ServerConfig::default().with_hostname("0.0.0.0"))
        .bind()
        .await
    {
        Ok(_) => panic!("an unauthenticated public listener must not start"),
        Err(error) => error,
    };
    assert!(matches!(error, ServerError::UnsecuredNonLoopback { .. }));
    let message = error.to_string();
    assert!(message.contains("--hostname"), "{message}");
    assert!(message.contains("expose"), "{message}");
    assert!(message.contains("ZUNO_SERVER_PASSWORD"), "{message}");
}

#[tokio::test]
async fn core_browser_auth_rejects_non_loopback_even_with_basic_auth() {
    let temp = tempfile::tempdir().expect("browser auth fixture");
    let config = ServerConfig::default()
        .with_hostname("0.0.0.0")
        .with_auth(AuthConfig::new(Some("secret".to_owned()), None))
        .with_browser_auth(temp.path().join("browser-auth.key"));
    let error = match ServerBuilder::new(config).bind().await {
        Ok(_) => panic!("browser authentication must remain loopback-only"),
        Err(error) => error,
    };
    assert!(matches!(error, ServerError::BrowserAuthNonLoopback { .. }));
}

#[tokio::test]
async fn core_browser_auth_exchanges_once_and_enforces_cookie_origin() {
    let temp = tempfile::tempdir().expect("browser auth fixture");
    let config = ServerConfig::default()
        .with_auth(AuthConfig::new(Some("secret".to_owned()), None))
        .with_browser_auth(temp.path().join("server/browser-auth.key"));
    let routes = Router::new().route("/mutate", post(|| async { "mutated\n" }));
    let mut server = ServerBuilder::new(config)
        .with_routes(routes)
        .bind()
        .await
        .expect("loopback browser-auth server binds");
    let address = server.local_addr();
    let origin = format!("http://{address}");
    let bootstrap = server
        .take_browser_bootstrap_uri()
        .expect("one bootstrap URI");
    assert!(server.take_browser_bootstrap_uri().is_none());
    let token = bootstrap
        .split_once("?token=")
        .map(|(_, token)| token)
        .expect("bootstrap token")
        .to_owned();
    let task = tokio::spawn(server.serve());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client");

    assert_eq!(
        client
            .get(format!("{origin}/health"))
            .send()
            .await
            .expect("unauthorized health request")
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let duplicate = client
        .get(format!("{origin}/auth/browser?token={token}&token={token}"))
        .send()
        .await
        .expect("duplicate token request");
    assert_eq!(duplicate.status(), StatusCode::UNAUTHORIZED);

    let exchange = client
        .get(&bootstrap)
        .send()
        .await
        .expect("bootstrap exchange");
    assert_eq!(exchange.status(), StatusCode::SEE_OTHER);
    assert_eq!(exchange.headers()[header::LOCATION], "/health");
    assert_eq!(exchange.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(exchange.headers()[header::REFERRER_POLICY], "no-referrer");
    let cookie = exchange.headers()[header::SET_COOKIE]
        .to_str()
        .expect("set-cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();
    assert!(!format!("{:?}", exchange.headers()).contains(&token));

    assert_eq!(
        client
            .get(&bootstrap)
            .send()
            .await
            .expect("replayed bootstrap request")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{origin}/health"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .expect("cookie health request")
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .post(format!("{origin}/mutate"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .expect("missing-origin mutation")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("{origin}/mutate"))
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, "http://127.0.0.1:1")
            .send()
            .await
            .expect("wrong-origin mutation")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("{origin}/mutate"))
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, &origin)
            .send()
            .await
            .expect("same-origin mutation")
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .post(format!("{origin}/mutate"))
            .header(header::AUTHORIZATION, "Basic enVubzpzZWNyZXQ=")
            .send()
            .await
            .expect("basic-auth mutation")
            .status(),
        StatusCode::OK,
        "Basic Auth remains sufficient without an Origin header"
    );
    assert_eq!(
        client
            .get(format!("{origin}/health"))
            .header(header::COOKIE, format!("{cookie}; {cookie}"))
            .send()
            .await
            .expect("duplicate-cookie request")
            .status(),
        StatusCode::UNAUTHORIZED
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn core_default_bind_uses_loopback_and_an_ephemeral_port() {
    let server = ServerBuilder::new(ServerConfig::default())
        .bind()
        .await
        .expect("the default listener binds");
    assert!(server.local_addr().ip().is_loopback());
    assert_ne!(server.local_addr().port(), 0);
}

#[tokio::test]
async fn core_stalled_subscriber_hits_a_queue_ceiling_and_observes_the_drop() {
    const CAPACITY: usize = 4;
    let fanout = EventFanout::with_capacity(CAPACITY);
    let mut stalled = fanout.subscribe();

    for event in 0..1_000_u64 {
        fanout.publish(event);
        assert!(
            stalled.queued() <= CAPACITY,
            "a stalled subscriber exceeded its declared queue ceiling"
        );
    }

    for expected in 0..CAPACITY as u64 {
        let delivery = stalled.recv().await.expect("the subscription is open");
        assert!(matches!(delivery, Delivery::Event(value) if *value == expected));
    }
    assert_eq!(
        stalled.recv().await,
        Some(Delivery::Lagged { dropped: 996 }),
        "drop-newest overflow must be explicit even when publishing has stopped"
    );
    assert_eq!(stalled.queued(), 0);
}
