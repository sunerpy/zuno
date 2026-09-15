use super::*;
use crate::OneShotAgentBackendKind;
use crate::OneShotAgentFailureCategory;
use crate::OneShotAgentFailureStage;
use crate::OneShotAgentFuture;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct RecordingBackend {
    kind: OneShotAgentBackendKind,
    capabilities: AgentBackendCapabilities,
    runs: Arc<AtomicUsize>,
}

impl OneShotAgentBackend for RecordingBackend {
    fn kind(&self) -> OneShotAgentBackendKind {
        self.kind
    }

    fn capabilities(&self) -> AgentBackendCapabilities {
        self.capabilities.clone()
    }

    fn run<'a>(
        &'a self,
        request: OneShotAgentRequest,
        _cancellation: CancellationToken,
    ) -> OneShotAgentFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(OneShotAgentResult {
                backend: self.kind,
                final_answer: request.prompt,
                product_session_id: None,
            })
        })
    }
}

fn backend(
    kind: OneShotAgentBackendKind,
    capabilities: AgentBackendCapabilities,
    runs: Arc<AtomicUsize>,
) -> Arc<dyn OneShotAgentBackend> {
    Arc::new(RecordingBackend {
        kind,
        capabilities,
        runs,
    })
}

fn request(prompt: &str) -> OneShotAgentRequest {
    OneShotAgentRequest {
        prompt: prompt.to_string(),
        cwd: AbsolutePathBuf::from_absolute_path(std::env::current_dir().expect("cwd"))
            .expect("absolute cwd"),
    }
}

#[test]
fn backend_ids_are_typed_and_bounded() {
    let id = AgentBackendId::new("team/claude-review").expect("valid namespaced id");
    assert_eq!(id.as_str(), "team/claude-review");
    assert_eq!(
        serde_json::from_str::<AgentBackendId>(r#""team/claude-review""#).expect("deserialize id"),
        id
    );

    for invalid in ["", " leading", "trailing ", "line\nbreak"] {
        assert!(AgentBackendId::new(invalid).is_err(), "{invalid:?}");
    }
    assert!(AgentBackendId::new("x".repeat(129)).is_err());
}

#[test]
fn known_backends_publish_profile_capabilities() {
    let native = AgentBackendCapabilities::native_codex();
    assert!(native.operations.contains(&AgentBackendOperation::OneShot));
    assert_eq!(native.model, AgentBackendOptionScope::Profile);
    assert_eq!(native.reasoning_effort, AgentBackendOptionScope::Profile);
    assert_eq!(native.permission_mode, AgentBackendOptionScope::Profile);
    assert_eq!(native.service_tier, AgentBackendOptionScope::Profile);

    let claude = AgentBackendCapabilities::claude_code();
    assert_eq!(claude.model, AgentBackendOptionScope::Profile);
    assert_eq!(claude.reasoning_effort, AgentBackendOptionScope::Profile);
    assert_eq!(claude.permission_mode, AgentBackendOptionScope::Profile);
    assert_eq!(claude.service_tier, AgentBackendOptionScope::Unsupported);

    let acp = AgentBackendCapabilities::acp();
    assert_eq!(acp.model, AgentBackendOptionScope::Profile);
    assert_eq!(acp.reasoning_effort, AgentBackendOptionScope::Profile);
    assert_eq!(acp.permission_mode, AgentBackendOptionScope::Profile);
    assert_eq!(acp.service_tier, AgentBackendOptionScope::Unsupported);
}

#[test]
fn profile_capability_does_not_authorize_invocation_override() {
    let capabilities = AgentBackendCapabilities::native_codex();
    let profile_requirement = AgentBackendRequirements {
        model: AgentBackendOptionScope::Profile,
        ..AgentBackendRequirements::one_shot()
    };
    assert!(capabilities.supports(&profile_requirement));

    let invocation_requirement = AgentBackendRequirements {
        model: AgentBackendOptionScope::Invocation,
        ..AgentBackendRequirements::one_shot()
    };
    assert_eq!(
        capabilities.missing(&invocation_requirement),
        [AgentBackendRequirement::Model(
            AgentBackendOptionScope::Invocation
        )]
    );

    let resume_requirement = AgentBackendRequirements {
        operation: AgentBackendOperation::Resume,
        ..AgentBackendRequirements::one_shot()
    };
    assert_eq!(
        capabilities.missing(&resume_requirement),
        [AgentBackendRequirement::Operation(
            AgentBackendOperation::Resume
        )]
    );
}

#[test]
fn inventory_is_sorted_and_mount_disposes_exact_registration() {
    let registry = AgentBackendRegistry::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let zulu = AgentBackendId::new("zulu").expect("id");
    let alpha = AgentBackendId::new("alpha").expect("id");
    let zulu_mount = registry
        .mount(
            zulu,
            backend(
                OneShotAgentBackendKind::ClaudeCode,
                AgentBackendCapabilities::claude_code(),
                Arc::clone(&runs),
            ),
        )
        .expect("mount zulu");
    let alpha_mount = registry
        .mount(
            alpha.clone(),
            backend(
                OneShotAgentBackendKind::NativeCodex,
                AgentBackendCapabilities::native_codex(),
                Arc::clone(&runs),
            ),
        )
        .expect("mount alpha");

    assert_eq!(
        registry
            .list()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zulu"]
    );
    let duplicate = registry
        .mount(
            alpha.clone(),
            backend(
                OneShotAgentBackendKind::Acp,
                AgentBackendCapabilities::one_shot_only(),
                Arc::clone(&runs),
            ),
        )
        .expect_err("duplicate fails");
    assert_eq!(
        duplicate,
        AgentBackendRegistryError::Duplicate { id: alpha }
    );

    drop(alpha_mount);
    assert_eq!(
        registry
            .list()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["zulu"]
    );
    assert!(zulu_mount.dispose());
    assert!(registry.list().is_empty());
}

#[test]
fn mount_rejects_backend_without_one_shot_capability() {
    let registry = AgentBackendRegistry::new();
    let id = AgentBackendId::new("resume-only").expect("id");
    let runs = Arc::new(AtomicUsize::new(0));
    let capabilities = AgentBackendCapabilities {
        operations: BTreeSet::from([AgentBackendOperation::Resume]),
        ..AgentBackendCapabilities::default()
    };
    assert_eq!(
        registry
            .mount(
                id.clone(),
                backend(OneShotAgentBackendKind::Acp, capabilities, runs)
            )
            .expect_err("invalid capabilities"),
        AgentBackendRegistryError::InvalidCapabilities { id }
    );
}

#[tokio::test]
async fn unsupported_capability_fails_before_backend_dispatch() {
    let registry = AgentBackendRegistry::new();
    let id = AgentBackendId::new("claude-review").expect("id");
    let runs = Arc::new(AtomicUsize::new(0));
    let _mount = registry
        .mount(
            id.clone(),
            backend(
                OneShotAgentBackendKind::ClaudeCode,
                AgentBackendCapabilities::claude_code(),
                Arc::clone(&runs),
            ),
        )
        .expect("mount");
    let requirements = AgentBackendRequirements {
        service_tier: AgentBackendOptionScope::Profile,
        ..AgentBackendRequirements::one_shot()
    };

    let error = registry
        .dispatch(
            &id,
            &requirements,
            request("must not run"),
            CancellationToken::new(),
        )
        .await
        .expect_err("unsupported capability");
    assert_eq!(runs.load(Ordering::Relaxed), 0);
    assert!(matches!(
        error,
        AgentBackendDispatchError::Registry(AgentBackendRegistryError::Unsupported {
            missing,
            ..
        }) if missing == vec![AgentBackendRequirement::ServiceTier(AgentBackendOptionScope::Profile)]
    ));
}

#[tokio::test]
async fn resolved_generation_survives_unmount_but_new_resolution_fails() {
    let registry = AgentBackendRegistry::new();
    let id = AgentBackendId::new("native-build").expect("id");
    let runs = Arc::new(AtomicUsize::new(0));
    let mounted = registry
        .mount(
            id.clone(),
            backend(
                OneShotAgentBackendKind::NativeCodex,
                AgentBackendCapabilities::native_codex(),
                Arc::clone(&runs),
            ),
        )
        .expect("mount");
    let resolved = registry
        .resolve(&id, &AgentBackendRequirements::one_shot())
        .expect("resolve");
    drop(mounted);

    assert_eq!(
        registry
            .resolve(&id, &AgentBackendRequirements::one_shot())
            .expect_err("new resolution fails"),
        AgentBackendRegistryError::NotFound { id: id.clone() }
    );
    assert_eq!(
        resolved
            .run(request("finish admitted work"), CancellationToken::new())
            .await
            .expect("admitted generation runs"),
        OneShotAgentResult {
            backend: OneShotAgentBackendKind::NativeCodex,
            final_answer: "finish admitted work".to_string(),
            product_session_id: None,
        }
    );
    assert_eq!(runs.load(Ordering::Relaxed), 1);
}

#[test]
fn backend_dispatch_error_keeps_registry_and_runtime_failures_distinct() {
    let registry = AgentBackendDispatchError::Registry(AgentBackendRegistryError::NotFound {
        id: AgentBackendId::new("missing").expect("id"),
    });
    assert!(std::error::Error::source(&registry).is_some());

    let backend = AgentBackendDispatchError::Backend(OneShotAgentError::new(
        OneShotAgentBackendKind::ClaudeCode,
        OneShotAgentFailureStage::Run,
        OneShotAgentFailureCategory::ProductError,
    ));
    assert!(std::error::Error::source(&backend).is_some());
}
