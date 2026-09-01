use std::sync::Arc;
use zuno_engine::driver::{AgentDriver, DefaultAgentDriver};
use zuno_harness::{
    HostPlanningCapability, ProductCapabilityKind, ToolContributions, ToolManifest,
    default_profile, default_profile_with_tools, named_capability_key,
    orchestration_capabilities_bundle, profile_with_tools, profile_with_tools_and_public_http,
    skill_capability_key,
};
use zuno_orchestration::{
    CapabilityContents, CapabilitySnapshot, PackIdentity, ProfileDescriptor,
    SkillCapabilityDescriptor, WorkflowNodeDescriptor, WorkflowTemplateDescriptor, sha256_text,
};
use zuno_runtime::{CapabilityAvailability, HarnessRuntime};
use zuno_tool::erase;
use zuno_tools::invalid::InvalidTool;
use zuno_tools::registry::BuiltinSlot;

fn orchestration_snapshot(definition: &str) -> Arc<CapabilitySnapshot> {
    Arc::new(CapabilitySnapshot::new(
        PackIdentity {
            id: "test-pack".to_owned(),
            version: "1.2.3".to_owned(),
            upstream_revision: "test@fixture".to_owned(),
        },
        7,
        sha256_text("permission policy"),
        CapabilityContents {
            profiles: vec![ProfileDescriptor {
                name: "orchestrator".to_owned(),
                source_id: "builtin://agent/orchestrator".to_owned(),
                definition_sha256: sha256_text(definition),
                permission_sha256: sha256_text("orchestrator permissions"),
                tools: Some(vec!["task".to_owned()]),
                delegates: Some(vec!["explorer".to_owned()]),
            }],
            workflows: vec![WorkflowTemplateDescriptor {
                name: "release-hardening".to_owned(),
                source_id: "configuration:workflows.release-hardening".to_owned(),
                max_parallel: 2,
                max_agents: 4,
                nodes: vec![WorkflowNodeDescriptor {
                    id: "scan".to_owned(),
                    agent: "explorer".to_owned(),
                    prompt: None,
                    description: Some("Inspect the repository.".to_owned()),
                    depends_on: Vec::new(),
                }],
            }],
            skills: vec![SkillCapabilityDescriptor {
                name: "codemap".to_owned(),
                source: "builtin://zuno-orchestration/skills/codemap".to_owned(),
                metadata_sha256: sha256_text("codemap metadata"),
                content_sha256: Some(sha256_text("codemap body")),
            }],
            ..CapabilityContents::default()
        },
    ))
}

#[tokio::test]
async fn the_default_profile_publishes_only_complete_default_host_tools() {
    let runtime = HarnessRuntime::new("profile");
    runtime
        .activate_profile(default_profile())
        .await
        .expect("default profile activates");

    assert_eq!(
        runtime
            .service::<dyn AgentDriver>()
            .expect("agent driver")
            .name(),
        "default"
    );
    let tools = runtime.service::<ToolManifest>().expect("tool manifest");
    assert!(tools.contains(BuiltinSlot::Task));
    assert!(tools.contains(BuiltinSlot::Job));
    assert!(tools.contains(BuiltinSlot::Search));
    assert!(tools.contains(BuiltinSlot::Write));
    assert!(tools.contains(BuiltinSlot::Patch));
    assert!(
        !tools.contains(BuiltinSlot::Edit),
        "the legacy exact-replacement editor is runtime-internal, not model-visible"
    );
    assert_eq!(tools.slots(), zuno_tools::registry::DEFAULT_BUILTINS);
    assert!(!tools.contains(BuiltinSlot::Execute));
    assert!(!tools.contains(BuiltinSlot::Lsp));
    assert!(!tools.contains(BuiltinSlot::Plan));
    assert!(
        runtime
            .service::<ToolContributions>()
            .expect("tool contributions")
            .tools()
            .is_empty()
    );
    assert!(
        runtime
            .service::<zuno_network::PublicHttpClient>()
            .is_some(),
        "the public transport is a profile-owned typed service"
    );
    assert!(
        runtime.service::<HostPlanningCapability>().is_some(),
        "the default host owns durable planning independently of plan_update visibility"
    );
}

#[tokio::test]
async fn a_custom_harness_selects_its_driver_and_tool_surface_without_loop_changes() {
    let runtime = HarnessRuntime::new("profile");
    let driver: Arc<dyn AgentDriver> = Arc::new(DefaultAgentDriver);
    let public_http = Arc::new(zuno_network::PublicHttpClient::new());
    runtime
        .activate_profile(profile_with_tools_and_public_http(
            "benchmark",
            Arc::clone(&driver),
            ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Task]).expect("unique tool slots"),
            ToolContributions::default(),
            Arc::clone(&public_http),
        ))
        .await
        .expect("custom profile activates");

    let resolved_driver = runtime.service::<dyn AgentDriver>().expect("agent driver");
    assert!(Arc::ptr_eq(&resolved_driver, &driver));
    assert!(Arc::ptr_eq(
        &runtime
            .service::<zuno_network::PublicHttpClient>()
            .expect("public HTTP transport"),
        &public_http
    ));
    assert_eq!(
        runtime
            .service::<ToolManifest>()
            .expect("tool manifest")
            .slots(),
        [BuiltinSlot::Read, BuiltinSlot::Task]
    );
    assert_eq!(runtime.active_profile_id().as_deref(), Some("benchmark"));
    assert!(
        runtime.service::<HostPlanningCapability>().is_none(),
        "custom profiles must opt into host planning rather than inheriting a loop heuristic"
    );
}

#[tokio::test]
async fn a_profile_publishes_native_tool_contributions() {
    let runtime = HarnessRuntime::new("profile");
    let contributed = erase(InvalidTool::new());
    runtime
        .activate_profile(profile_with_tools(
            "custom-tools",
            Arc::new(DefaultAgentDriver),
            ToolManifest::new([]).expect("empty manifest"),
            ToolContributions::new([Arc::clone(&contributed)]).expect("unique tools"),
        ))
        .await
        .expect("custom profile activates");

    let tools = runtime
        .service::<ToolContributions>()
        .expect("tool contributions");
    assert_eq!(tools.tools().len(), 1);
    assert!(Arc::ptr_eq(&tools.tools()[0], &contributed));

    let key = named_capability_key(ProductCapabilityKind::Tool, contributed.id())
        .expect("valid tool capability key");
    let capability = runtime.capability(&key).expect("named tool capability");
    let identity = contributed.definition().schema_identity();
    assert_eq!(capability.owner(), "zuno.tool-contributions");
    assert_eq!(capability.availability(), CapabilityAvailability::Available);
    assert_eq!(capability.contract().interface(), "zuno.tool/v1");
    assert_eq!(
        capability.contract().schema_digest(),
        Some(identity.schema_sha256.as_str())
    );
    assert_eq!(
        capability.provenance().source(),
        "profile-contribution://tool/invalid"
    );
    assert_eq!(
        capability.provenance().package(),
        None,
        "the profile aggregate must not invent an extension package identity"
    );
}

#[tokio::test]
async fn orchestration_snapshot_publishes_typed_and_named_product_capabilities() {
    let runtime = HarnessRuntime::new("profile");
    let snapshot = orchestration_snapshot("v1");
    let profile =
        default_profile().with_bundle(orchestration_capabilities_bundle(Arc::clone(&snapshot)));

    runtime
        .activate_profile(profile)
        .await
        .expect("profile with orchestration capabilities activates");

    let resolved = runtime
        .service::<CapabilitySnapshot>()
        .expect("typed capability snapshot");
    assert!(Arc::ptr_eq(&resolved, &snapshot));

    for (kind, name, source, interface) in [
        (
            ProductCapabilityKind::AgentProfile,
            "orchestrator",
            "builtin://agent/orchestrator",
            "zuno.agent-profile/v1",
        ),
        (
            ProductCapabilityKind::WorkflowTemplate,
            "release-hardening",
            "configuration:workflows.release-hardening",
            "zuno.workflow-template/v1",
        ),
    ] {
        let key = named_capability_key(kind, name).expect("valid capability key");
        let descriptor = runtime.capability(&key).expect("published capability");
        assert_eq!(descriptor.owner(), "zuno.orchestration-capabilities");
        assert_eq!(descriptor.contract().interface(), interface);
        assert!(descriptor.contract().schema_digest().is_some());
        assert_eq!(descriptor.provenance().source(), source);
        assert_eq!(descriptor.provenance().package(), Some("test-pack@1.2.3"));
    }

    let skill_source = "builtin://zuno-orchestration/skills/codemap";
    let skill_key = skill_capability_key("codemap", skill_source).expect("valid Skill key");
    let skill = runtime.capability(&skill_key).expect("published Skill");
    assert_eq!(skill.contract().interface(), "zuno.skill/v1");
    assert_eq!(skill.provenance().source(), skill_source);
    assert_eq!(skill.provenance().package(), Some("test-pack@1.2.3"));
}

#[tokio::test]
async fn same_name_skills_from_distinct_sources_publish_independent_routes() {
    let runtime = HarnessRuntime::new("profile");
    let mut snapshot = orchestration_snapshot("skills").as_ref().clone();
    let second_source = "project://.agents/skills/codemap/SKILL.md";
    snapshot.skills.push(SkillCapabilityDescriptor {
        name: "codemap".to_owned(),
        source: second_source.to_owned(),
        metadata_sha256: sha256_text("project codemap metadata"),
        content_sha256: None,
    });

    runtime
        .activate_profile(
            default_profile().with_bundle(orchestration_capabilities_bundle(Arc::new(snapshot))),
        )
        .await
        .expect("same-name Skills with distinct sources activate");

    let builtin = skill_capability_key("codemap", "builtin://zuno-orchestration/skills/codemap")
        .expect("builtin Skill key");
    let project = skill_capability_key("codemap", second_source).expect("project Skill key");
    assert_ne!(builtin, project);
    assert_eq!(
        runtime
            .capability(&builtin)
            .expect("builtin Skill")
            .provenance()
            .source(),
        "builtin://zuno-orchestration/skills/codemap"
    );
    assert_eq!(
        runtime
            .capability(&project)
            .expect("project Skill")
            .provenance()
            .source(),
        second_source
    );
}

#[tokio::test]
async fn replacing_orchestration_snapshot_advances_generation_and_retires_the_old_route() {
    let runtime = HarnessRuntime::new("profile");
    let key = named_capability_key(ProductCapabilityKind::AgentProfile, "orchestrator")
        .expect("valid profile capability key");

    runtime
        .activate_profile(
            default_profile().with_bundle(orchestration_capabilities_bundle(
                orchestration_snapshot("v1"),
            )),
        )
        .await
        .expect("first generation activates");
    let first = runtime.capability(&key).expect("first generation");

    runtime
        .activate_profile(
            default_profile().with_bundle(orchestration_capabilities_bundle(
                orchestration_snapshot("v2"),
            )),
        )
        .await
        .expect("replacement generation activates");
    let second = runtime.capability(&key).expect("second generation");

    assert_eq!(second.generation(), first.generation() + 1);
    assert_ne!(second.contract(), first.contract());
    assert!(!runtime.capability_is_current(&first));
    assert!(runtime.capability_is_current(&second));
}

#[tokio::test]
async fn duplicate_product_descriptors_abort_the_candidate_without_partial_publication() {
    let runtime = HarnessRuntime::new("profile");
    let mut snapshot = orchestration_snapshot("duplicate").as_ref().clone();
    snapshot.profiles.push(snapshot.profiles[0].clone());

    let error = runtime
        .activate_profile(
            default_profile().with_bundle(orchestration_capabilities_bundle(Arc::new(snapshot))),
        )
        .await
        .expect_err("duplicate capability keys must fail activation");

    assert!(
        error.to_string().contains("is already staged"),
        "unexpected activation error: {error}"
    );
    assert!(runtime.active_profile_id().is_none());
    assert!(runtime.service::<CapabilitySnapshot>().is_none());
    assert!(runtime.service::<ToolManifest>().is_none());
}

#[tokio::test]
async fn the_default_profile_can_mount_process_owned_tool_contributions() {
    let runtime = HarnessRuntime::new("profile");
    let contributed = erase(InvalidTool::new());
    runtime
        .activate_profile(default_profile_with_tools(
            ToolContributions::new([Arc::clone(&contributed)]).expect("unique tools"),
        ))
        .await
        .expect("default profile with contributions activates");

    let tools = runtime
        .service::<ToolContributions>()
        .expect("tool contributions");
    assert_eq!(tools.tools().len(), 1);
    assert!(Arc::ptr_eq(&tools.tools()[0], &contributed));
}

#[test]
fn duplicate_tool_slots_fail_when_the_profile_is_built() {
    let error = ToolManifest::new([BuiltinSlot::Task, BuiltinSlot::Task])
        .expect_err("duplicate slots fail");

    assert_eq!(
        error.to_string(),
        "tool slot `task` is declared more than once"
    );
}

#[test]
fn duplicate_contributed_tool_ids_fail_before_mount() {
    let tool = erase(InvalidTool::new());
    let error =
        ToolContributions::new([Arc::clone(&tool), tool]).expect_err("duplicate tool ids fail");

    assert_eq!(
        error.to_string(),
        "tool `invalid` is contributed more than once"
    );
}
