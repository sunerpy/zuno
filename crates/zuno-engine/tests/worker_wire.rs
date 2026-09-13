use async_trait::async_trait;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};
use zuno_engine::state::remote::{RemoteTurnPersistence, StateTransport};
use zuno_engine::state::wire::{
    StateCommand, StateReply, StateRequest, StateResponse, StoredMessage, WORKER_PROTOCOL_VERSION,
};
use zuno_engine::state::{
    InputMaterialization, LiveInputGate, TurnPersistence, TurnStateError, TurnStateScope,
};
use zuno_types::identity::{PrincipalScope, SessionId};

fn scope() -> TurnStateScope {
    TurnStateScope {
        owner: PrincipalScope::local().owner(),
        session_id: "session".to_owned(),
    }
}

#[test]
fn request_version_and_scope_injection_fail_closed() {
    for version in [1, WORKER_PROTOCOL_VERSION + 1] {
        assert!(
            StateRequest::decode(
                &serde_json::to_vec(&json!({
                    "version":version,"command":{"kind":"touch"},
                }))
                .unwrap()
            )
            .is_err()
        );
    }
    assert!(
        StateRequest::decode(
            &serde_json::to_vec(&json!({
                "version":WORKER_PROTOCOL_VERSION,"owner":"administrator","command":{"kind":"touch"},
            })).unwrap()
        )
        .is_err()
    );
    let request = StateRequest::new(StateCommand::Touch);
    assert!(matches!(
        StateRequest::decode(&request.encode().unwrap())
            .unwrap()
            .command,
        StateCommand::Touch
    ));
}

#[test]
fn message_parts_cannot_change_their_parent_or_session_on_the_wire() {
    let value = json!({
        "body":{"id":"message","sessionID":"session","role":"assistant","time":{"created":1}},
        "parts":[{"body":{"id":"part","sessionID":"other","messageID":"message","type":"text","text":"private"},"createdAtMs":1}],
    });
    let message: StoredMessage = serde_json::from_value(value).unwrap();
    assert!(MessageWithParts::try_from(message).is_err());
}

struct ReplyTransport {
    session: &'static str,
}
#[async_trait]
impl StateTransport for ReplyTransport {
    async fn exchange(&self, _request: StateRequest) -> Result<StateResponse, TurnStateError> {
        Ok(StateResponse::new(Ok(StateReply::Session {
            id: SessionId::new(self.session).unwrap(),
            parent_id: None,
        })))
    }
}

#[tokio::test]
async fn a_remote_session_uses_only_the_workers_bound_directory_and_scope() {
    let remote = RemoteTurnPersistence::new(
        Arc::new(ReplyTransport { session: "session" }),
        scope(),
        "/worker/workspace".to_owned(),
    )
    .unwrap();
    assert_eq!(
        remote.session(&scope()).await.unwrap().directory.as_deref(),
        Some("/worker/workspace")
    );
    let wrong = RemoteTurnPersistence::new(
        Arc::new(ReplyTransport { session: "foreign" }),
        scope(),
        "/worker/workspace".to_owned(),
    )
    .unwrap();
    assert!(wrong.session(&scope()).await.is_err());
    let encoded = StateResponse::new(Ok(StateReply::Session {
        id: SessionId::new("session").unwrap(),
        parent_id: None,
    }))
    .encode()
    .unwrap();
    assert!(!String::from_utf8(encoded).unwrap().contains("directory"));
}

struct LostAcknowledgement(Arc<AtomicUsize>);
#[async_trait]
impl StateTransport for LostAcknowledgement {
    async fn exchange(&self, _request: StateRequest) -> Result<StateResponse, TurnStateError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(TurnStateError::Unavailable)
    }
}

#[tokio::test]
async fn an_ambiguous_write_is_not_repeated_by_the_remote_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let remote = RemoteTurnPersistence::new(
        Arc::new(LostAcknowledgement(Arc::clone(&calls))),
        scope(),
        "/worker/workspace".to_owned(),
    )
    .unwrap();
    assert!(remote.touch(&scope()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct RefusedInput(Arc<AtomicUsize>);

#[async_trait]
impl StateTransport for RefusedInput {
    async fn exchange(&self, request: StateRequest) -> Result<StateResponse, TurnStateError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let request = StateRequest::decode(&request.encode()?)?;
        let StateCommand::ConsumeInput(input) = request.command else {
            panic!("expected input consumption");
        };
        assert_eq!(input.input_id.as_str(), "input");
        assert_eq!(input.turn_id.as_ref().unwrap().as_str(), "turn");
        let gate = input.live.expect("live claim crosses the state API");
        assert_eq!(gate.revision, Some(3));
        assert_eq!(
            gate.source,
            zuno_engine::interrupt::SoftInterruptSource::User
        );
        let response = StateResponse::new(Ok(StateReply::Boolean(false)));
        StateResponse::decode(&response.encode()?)
    }
}

#[tokio::test]
async fn a_refused_live_input_remains_unconsumed_across_the_worker_protocol() {
    let calls = Arc::new(AtomicUsize::new(0));
    let remote = RemoteTurnPersistence::new(
        Arc::new(RefusedInput(Arc::clone(&calls))),
        scope(),
        "/worker/workspace".to_owned(),
    )
    .unwrap();
    let consumed = remote
        .consume_input(
            &scope(),
            InputMaterialization {
                live: Some(LiveInputGate {
                    revision: Some(3),
                    source: zuno_engine::interrupt::SoftInterruptSource::User,
                }),
                input_id: Some("input".to_owned()),
                turn_id: Some("turn".to_owned()),
                message: MessageRecord::from_json(json!({
                    "id":"input","sessionID":"session","role":"user","time":{"created":1},
                }))
                .unwrap(),
                parts: vec![
                    PartRecord::from_json(
                        json!({
                            "id":"part","sessionID":"session","messageID":"input",
                            "type":"text","text":"Wait for authorization",
                        }),
                        1,
                    )
                    .unwrap(),
                ],
            },
        )
        .await
        .unwrap();
    assert!(!consumed, "a successful transport is not input admission");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
