use async_trait::async_trait;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use zuno_db::message::MessageWithParts;
use zuno_engine::state::remote::{RemoteTurnPersistence, StateTransport};
use zuno_engine::state::wire::{
    StateCommand, StateReply, StateRequest, StateResponse, StoredMessage,
};
use zuno_engine::state::{TurnPersistence, TurnStateError, TurnStateScope};
use zuno_types::identity::{PrincipalScope, SessionId};

fn scope() -> TurnStateScope {
    TurnStateScope {
        owner: PrincipalScope::local().owner(),
        session_id: "session".to_owned(),
    }
}

#[test]
fn request_version_and_scope_injection_fail_closed() {
    assert!(StateRequest::decode(br#"{"version":2,"command":{"kind":"touch"}}"#).is_err());
    assert!(
        StateRequest::decode(
            br#"{"version":1,"owner":"administrator","command":{"kind":"touch"}}"#
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
