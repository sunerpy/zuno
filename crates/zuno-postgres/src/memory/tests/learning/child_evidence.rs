use super::*;
use zuno_application::child::{
    ChildDefinitionGrant, ChildDelivery, ChildDispatchStore, ChildInvocation, ChildWorkspacePolicy,
};
use zuno_types::identity::InvocationId;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("child-evidence");
    let workspace = WorkspaceId::new("child-evidence").unwrap();
    let root_session = setup(backend, admin, &actor, &workspace).await;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
    let human = memory.for_user(actor.clone(), workspace.clone());
    for session_id in [None, Some(root_session.clone())] {
        let suffix = if session_id.is_some() {
            "root"
        } else {
            "owner"
        };
        human
            .request(request(
                &format!("{suffix}-generation"),
                MemoryCommand::SetPolicy {
                    session_id: session_id.clone(),
                    expected_revision: 0,
                    use_memories: true,
                    generate_private: true,
                },
            ))
            .await
            .unwrap();
        human
            .request(request(
                &format!("{suffix}-automation"),
                MemoryCommand::SetAutomation {
                    session_id,
                    expected_revision: 1,
                    enabled: true,
                },
            ))
            .await
            .unwrap();
    }
    maintenance_wake::manual_note(&human, "note", "Keep validation evidence.").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    runtime
        .submit(
            &actor,
            JobSubmission {
                session_id: root_session.clone(),
                request_id: RequestId::new("root").unwrap(),
                expected_input_version: 0,
                text: "Inspect the project".to_owned(),
                configuration: configured(),
                selection: None,
            },
        )
        .await
        .unwrap();
    let root = runtime
        .claim(
            &WorkerInstanceId::new("root").unwrap(),
            LeaseDuration::new(30000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    let child = runtime
        .dispatch_child(
            &root.lease,
            ChildInvocation {
                invocation_id: InvocationId::new("child").unwrap(),
                arguments_sha256: "c".repeat(64),
                logical_key: "child".to_owned(),
                prompt: "Run validation".to_owned(),
                description: "Validation".to_owned(),
                delivery: ChildDelivery::Quiet,
                resume_session_id: None,
                presentation: json!({}),
            },
            &ChildDefinitionGrant {
                parent: configured(),
                child: configured(),
                selection: zuno_application::runtime::JobInputSelection {
                    agent: "child".to_owned(),
                    model: zuno_application::runtime::JobInputModel {
                        provider_id: "fixture".to_owned(),
                        model_id: "model".to_owned(),
                    },
                },
                maximum_depth: 16,
                maximum_children: 256,
                workspace: ChildWorkspacePolicy::ModelOnly,
            },
        )
        .await
        .unwrap();
    let MemoryReply::Policy { policy } = human
        .request(request(
            "child-policy",
            MemoryCommand::Policy {
                session_id: Some(child.session_id.clone()),
            },
        ))
        .await
        .unwrap()
    else {
        panic!("policy");
    };
    assert!(
        policy.automatic_private,
        "a child inherits the explicitly enabled parent automation flag"
    );
    let child_job = runtime
        .claim(
            &WorkerInstanceId::new("child").unwrap(),
            LeaseDuration::new(30000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child_job.job.id, child.job_id);
    let model = memory.for_worker(child_job.lease.clone());
    assert!(contents(&model).await.contains("Keep validation evidence."));
    human
        .request(request(
            "root-read-off",
            MemoryCommand::SetPolicy {
                session_id: Some(root_session.clone()),
                expected_revision: 2,
                use_memories: false,
                generate_private: true,
            },
        ))
        .await
        .unwrap();
    assert!(
        contents(&model).await.trim().is_empty(),
        "current ancestor recall policy reaches an existing child"
    );
    human
        .request(request(
            "root-generation-off",
            MemoryCommand::SetPolicy {
                session_id: Some(root_session.clone()),
                expected_revision: 3,
                use_memories: false,
                generate_private: false,
            },
        ))
        .await
        .unwrap();
    assert!(
        matches!(
            model
                .request(request("child-write", change("New derived fact.")))
                .await,
            Err(Error::Denied)
        ),
        "an inherited old child policy cannot widen a revoked ancestor grant"
    );
    query("UPDATE zuno_enterprise_preview.session SET parent_id=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str())
        .bind(root_session.as_str()).bind(child.session_id.as_str()).execute(admin).await.unwrap();
    assert!(
        model
            .request(request("cyclic-read", MemoryCommand::Read))
            .await
            .is_err(),
        "corrupt ancestry fails closed instead of looping or returning private data"
    );
    query("UPDATE zuno_enterprise_preview.session SET parent_id=NULL WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str())
        .bind(root_session.as_str()).execute(admin).await.unwrap();
    human
        .request(request(
            "owner-disable",
            MemoryCommand::SetAutomation {
                session_id: None,
                expected_revision: 2,
                enabled: false,
            },
        ))
        .await
        .unwrap();
    Box::pin(completed_graph(backend, admin)).await;
}

async fn completed_graph(backend: &PostgresBackend, admin: &PgPool) {
    let actor = PrincipalScope::new(
        TenantId::new("learning-child-graph").unwrap(),
        PrincipalId::new("owner").unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("enterprise-web").unwrap()),
        NonZeroU64::MIN,
    );
    let workspace = WorkspaceId::new("graph").unwrap();
    let session = setup(backend, admin, &actor, &workspace).await;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
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
    let runtime = backend.runtime(actor.tenant_id().clone());
    runtime
        .submit(
            &actor,
            JobSubmission {
                session_id: session.clone(),
                request_id: RequestId::new("root").unwrap(),
                expected_input_version: 0,
                text: "Inspect completed children".to_owned(),
                configuration: configured(),
                selection: None,
            },
        )
        .await
        .unwrap();
    let root = runtime
        .claim(
            &WorkerInstanceId::new("graph-root").unwrap(),
            LeaseDuration::new(30000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    let mut successful = None;
    for (id, complete) in [("completed", true), ("failed", false)] {
        let child = runtime
            .dispatch_child(
                &root.lease,
                ChildInvocation {
                    invocation_id: InvocationId::new(id).unwrap(),
                    arguments_sha256: "d".repeat(64),
                    logical_key: id.to_owned(),
                    prompt: "Inspect".to_owned(),
                    description: "Inspection".to_owned(),
                    delivery: ChildDelivery::Quiet,
                    resume_session_id: None,
                    presentation: json!({}),
                },
                &ChildDefinitionGrant {
                    parent: configured(),
                    child: configured(),
                    selection: zuno_application::runtime::JobInputSelection {
                        agent: "child".to_owned(),
                        model: zuno_application::runtime::JobInputModel {
                            provider_id: "fixture".to_owned(),
                            model_id: "model".to_owned(),
                        },
                    },
                    maximum_depth: 16,
                    maximum_children: 256,
                    workspace: ChildWorkspacePolicy::ModelOnly,
                },
            )
            .await
            .unwrap();
        let claimed = runtime
            .claim(
                &WorkerInstanceId::new(id).unwrap(),
                LeaseDuration::new(30000).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.job.id, child.job_id);
        query("UPDATE zuno_enterprise_preview.input SET state='consumed' WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(claimed.job.input_id.as_str())
            .execute(admin).await.unwrap();
        let finish = if complete {
            let message = format!("answer-{id}");
            query("INSERT INTO zuno_enterprise_preview.message(tenant_id,principal_id,session_id,id,role,data,time_created,time_updated)
                VALUES($1,$2,$3,$4,'assistant',$5,1,2)")
                .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(child.session_id.as_str()).bind(&message)
                .bind(json!({"id":message,"role":"assistant","time":{"created":1,"completed":2}})).execute(admin).await.unwrap();
            successful = Some(child.clone());
            JobFinish::Completed {
                result: json!(zuno_engine::advance::AdvanceState::Completed {
                    outcome: zuno_engine::r#loop::TurnOutcome::Completed {
                        assistant_message_id: message,
                        steps: 1,
                        unresolved_tool_failures: Vec::new(),
                    },
                }),
            }
        } else {
            JobFinish::Failed {
                code: "failed_inspection".to_owned(),
            }
        };
        runtime.finish(&claimed.lease, finish).await.unwrap();
    }
    let successful = successful.unwrap();
    query("UPDATE zuno_enterprise_preview.input SET state='consumed' WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(root.job.input_id.as_str())
        .execute(admin).await.unwrap();
    runtime
        .finish(
            &root.lease,
            JobFinish::Completed {
                result: json!({"closed":true}),
            },
        )
        .await
        .unwrap();
    let root_job = root.job.clone();
    let selected = memory
        .automate(
            actor.clone(),
            workspace.clone(),
            session.clone(),
            move |provider| {
                provider.execute(async |tx| provider.completed_source_jobs(tx, &root_job).await)
            },
        )
        .await
        .unwrap();
    assert_eq!(
        selected.0,
        vec![root.job.id.to_string(), successful.job_id.to_string()]
    );
    assert!(!selected.1);
    let digest: String = query_scalar("SELECT completion_digest FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(successful.job_id.as_str())
        .fetch_one(admin).await.unwrap();
    query("UPDATE zuno_enterprise_preview.runtime_child SET completion_digest=$4 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(successful.job_id.as_str())
        .bind("0".repeat(64)).execute(admin).await.unwrap();
    let root_job = root.job.clone();
    assert!(
        memory
            .automate(
                actor.clone(),
                workspace.clone(),
                session.clone(),
                move |provider| {
                    provider.execute(async |tx| provider.completed_source_jobs(tx, &root_job).await)
                }
            )
            .await
            .is_err(),
        "corrupt completion facts cannot lend lineage to evidence"
    );
    query("UPDATE zuno_enterprise_preview.runtime_child SET completion_digest=$4 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(successful.job_id.as_str())
        .bind(digest).execute(admin).await.unwrap();
    human
        .request(request(
            "child-disable",
            MemoryCommand::SetPolicy {
                session_id: Some(successful.session_id),
                expected_revision: 0,
                use_memories: true,
                generate_private: false,
            },
        ))
        .await
        .unwrap();
    let root_job = root.job;
    let root_id = root_job.id.to_string();
    let selected = memory
        .automate(actor, workspace, session, move |provider| {
            provider.execute(async |tx| provider.completed_source_jobs(tx, &root_job).await)
        })
        .await
        .unwrap();
    assert_eq!(
        selected.0,
        vec![root_id],
        "a completed child remains subject to its current generation policy"
    );
}
