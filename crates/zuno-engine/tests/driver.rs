use futures::future::BoxFuture;
use std::sync::Arc;
use zuno_engine::driver::{AgentDriver, AgentDriverComponent, DefaultAgentDriver};
use zuno_engine::r#loop::{RunTurnRequest, TurnContext, TurnError, TurnEventSender, TurnOutcome};
use zuno_runtime::HarnessRuntime;

struct BenchmarkDriver;

impl AgentDriver for BenchmarkDriver {
    fn name(&self) -> &str {
        "benchmark"
    }

    fn drive<'a>(
        &'a self,
        _request: RunTurnRequest,
        _context: TurnContext<'a>,
        _events: TurnEventSender,
    ) -> BoxFuture<'a, Result<TurnOutcome, TurnError>> {
        Box::pin(async { unreachable!("this test only verifies runtime composition") })
    }
}

#[tokio::test]
async fn profile_driver_is_a_replaceable_runtime_service() {
    let profile = HarnessRuntime::new("profile");
    profile
        .mount(AgentDriverComponent::new(Arc::new(DefaultAgentDriver)))
        .await
        .expect("default driver mounts");
    let session = profile.child("session");

    assert_eq!(
        session
            .service::<dyn AgentDriver>()
            .expect("session inherits profile driver")
            .name(),
        "default"
    );

    profile
        .replace(AgentDriverComponent::new(Arc::new(BenchmarkDriver)))
        .await
        .expect("driver replacement commits");

    assert_eq!(
        session
            .service::<dyn AgentDriver>()
            .expect("session observes replacement")
            .name(),
        "benchmark"
    );
}
