use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use oc_server::{
    AuthConfig, Delivery, EventFanout, RequestDirectory, ServerBuilder, ServerConfig, ServerError,
};
use tower::ServiceExt;

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
        .oneshot(request("/health", Some("Basic b3BlbmNvZGU6c2VjcmV0")))
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
                .header("x-opencode-directory", "%2Fworkspace%2Fheader")
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
                .header("x-opencode-directory", "%2Fworkspace%2Fheader")
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
    assert!(message.contains("OPENCODE_SERVER_PASSWORD"), "{message}");
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
