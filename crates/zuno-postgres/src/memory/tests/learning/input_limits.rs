use super::maintenance_wake::{claim, complete_output, manual_note};
use super::*;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let workspace = WorkspaceId::new("input-limits").unwrap();
    let mut binding = grant(&workspace);
    binding.extraction_limits.maximum_input_bytes = 8192;
    binding.maintenance_limits.maximum_input_bytes = 8192;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
    let runtime = PostgresLearningRuntime::new(
        memory.clone(),
        TenantId::new("memory").unwrap(),
        vec![binding.clone()],
    )
    .unwrap();
    for name in ["input-limits-large", "input-limits-small"] {
        let actor = principal(name);
        setup(backend, admin, &actor, &workspace).await;
        let human = memory.for_user(actor.clone(), workspace.clone());
        human
            .request(request(
                "generation",
                MemoryCommand::SetPolicy {
                    session_id: None,
                    expected_revision: 0,
                    use_memories: true,
                    generate_private: true,
                },
            ))
            .await
            .unwrap();
        human
            .request(request(
                "automation",
                MemoryCommand::SetAutomation {
                    session_id: None,
                    expected_revision: 1,
                    enabled: true,
                },
            ))
            .await
            .unwrap();
        if name.ends_with("large") {
            manual_note(&human, "large-note", &"保留验证依据。".repeat(250)).await;
        }
        let source = source(backend, admin, &actor, &workspace, name).await;
        assert_eq!(runtime.schedule(8).await.unwrap(), 1);
        let extraction = claim(&runtime, &binding).await;
        assert_eq!(extraction.execution.principal.owner(), actor.owner());
        let LearningInput::Extraction(input) = &extraction.execution.input else {
            panic!("extraction");
        };
        assert_eq!(
            input.sources.len(),
            1,
            "small profiles still carry real evidence"
        );
        let reference = input.sources[0].reference_id.clone();
        complete_output(&runtime, &extraction, "extract", json!({
            "experiences":[{"kind":"user_correction","title":"Validation","summary":"Use cargo test for validation",
                "resolution":null,"confidence":1.0,
                "evidence":[{"kind":"user","source_id":reference,"excerpt":"Use cargo test for validation"}]}],
            "memories":[]
        })).await;
        let page = backend
            .client_learning_jobs(
                &actor,
                &workspace,
                zuno_application::learning_api::LearningPageRequest {
                    stage: Some(zuno_application::learning_api::LearningStage::Maintenance),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].source_job_id, source);
        if name.ends_with("large") {
            let failed = &page.items[0];
            assert_eq!(
                failed.state,
                zuno_application::learning_api::LearningState::Failed
            );
            assert_eq!(
                failed.failure.as_ref().unwrap().code,
                "learning_input_budget"
            );
            assert_eq!(failed.budget.model_requests.0, 0);
            assert_eq!(
                runtime.schedule(8).await.unwrap(),
                0,
                "an unchanged oversized batch is settled once"
            );
            assert_eq!(
                backend
                    .client_learning_job(&actor, &extraction.execution.id)
                    .await
                    .unwrap()
                    .state,
                zuno_application::learning_api::LearningState::Completed,
                "maintenance cannot roll back extracted evidence"
            );
        } else {
            let maintenance = claim(&runtime, &binding).await;
            assert_eq!(
                maintenance.execution.principal.owner(),
                actor.owner(),
                "another owner's oversized Memory cannot block learning"
            );
            complete_output(&runtime, &maintenance, "maintain", json!({"updates":[]})).await;
        }
    }
    for name in ["input-limits-large", "input-limits-small"] {
        memory
            .for_user(principal(name), workspace.clone())
            .request(request(
                "disable",
                MemoryCommand::SetAutomation {
                    session_id: None,
                    expected_revision: 2,
                    enabled: false,
                },
            ))
            .await
            .unwrap();
    }
}
