use super::*;
use crate::OneShotAgentBackendKind;
use crate::OneShotAgentFuture;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct FactoryBackend {
    kind: OneShotAgentBackendKind,
    capabilities: AgentBackendCapabilities,
}

impl OneShotAgentBackend for FactoryBackend {
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
            Ok(OneShotAgentResult {
                backend: self.kind,
                final_answer: request.prompt,
                product_session_id: None,
            })
        })
    }
}

struct RecordingFactory {
    advertised_kind: OneShotAgentBackendKind,
    advertised_capabilities: AgentBackendCapabilities,
    revision: String,
    actual_kind: OneShotAgentBackendKind,
    actual_capabilities: AgentBackendCapabilities,
    builds: Arc<AtomicUsize>,
}

impl AgentBackendFactory<usize> for RecordingFactory {
    fn kind(&self) -> OneShotAgentBackendKind {
        self.advertised_kind
    }

    fn capabilities(&self) -> AgentBackendCapabilities {
        self.advertised_capabilities.clone()
    }

    fn revision(&self) -> String {
        self.revision.clone()
    }

    fn build(&self, context: &usize) -> Result<Arc<dyn OneShotAgentBackend>, OneShotAgentError> {
        self.builds.fetch_add(*context, Ordering::Relaxed);
        Ok(Arc::new(FactoryBackend {
            kind: self.actual_kind,
            capabilities: self.actual_capabilities.clone(),
        }))
    }
}

fn factory(
    kind: OneShotAgentBackendKind,
    capabilities: AgentBackendCapabilities,
    builds: Arc<AtomicUsize>,
) -> Arc<dyn AgentBackendFactory<usize>> {
    Arc::new(RecordingFactory {
        advertised_kind: kind,
        advertised_capabilities: capabilities.clone(),
        revision: "test-factory/v1".to_string(),
        actual_kind: kind,
        actual_capabilities: capabilities,
        builds,
    })
}

#[test]
fn factory_requires_a_stable_bounded_revision() {
    let registry = AgentBackendFactoryRegistry::new();
    let id = AgentBackendId::new("unversioned").expect("id");
    let builds = Arc::new(AtomicUsize::new(0));
    let error = registry
        .mount(
            id.clone(),
            Arc::new(RecordingFactory {
                advertised_kind: OneShotAgentBackendKind::NativeCodex,
                advertised_capabilities: AgentBackendCapabilities::native_codex(),
                revision: "  ".to_string(),
                actual_kind: OneShotAgentBackendKind::NativeCodex,
                actual_capabilities: AgentBackendCapabilities::native_codex(),
                builds,
            }),
        )
        .expect_err("blank revision must fail");
    assert_eq!(error, AgentBackendRegistryError::InvalidRevision { id });
}

#[test]
fn factory_inventory_is_sorted_and_mount_is_an_exact_disposer() {
    let registry = AgentBackendFactoryRegistry::new();
    let builds = Arc::new(AtomicUsize::new(0));
    let zulu = AgentBackendId::new("zulu").expect("id");
    let alpha = AgentBackendId::new("alpha").expect("id");
    let zulu_mount = registry
        .mount(
            zulu,
            factory(
                OneShotAgentBackendKind::ClaudeCode,
                AgentBackendCapabilities::claude_code(),
                Arc::clone(&builds),
            ),
        )
        .expect("mount zulu");
    let alpha_mount = registry
        .mount(
            alpha.clone(),
            factory(
                OneShotAgentBackendKind::NativeCodex,
                AgentBackendCapabilities::native_codex(),
                Arc::clone(&builds),
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
    assert_eq!(
        registry
            .mount(
                alpha.clone(),
                factory(
                    OneShotAgentBackendKind::Acp,
                    AgentBackendCapabilities::one_shot_only(),
                    Arc::clone(&builds),
                ),
            )
            .expect_err("duplicate must fail"),
        AgentBackendRegistryError::Duplicate { id: alpha }
    );

    drop(alpha_mount);
    assert_eq!(registry.list().len(), 1);
    assert!(zulu_mount.dispose());
    assert!(registry.list().is_empty());
}

#[test]
fn requirements_fail_before_factory_construction() {
    let registry = AgentBackendFactoryRegistry::new();
    let id = AgentBackendId::new("claude-review").expect("id");
    let builds = Arc::new(AtomicUsize::new(0));
    let _mount = registry
        .mount(
            id.clone(),
            factory(
                OneShotAgentBackendKind::ClaudeCode,
                AgentBackendCapabilities::claude_code(),
                Arc::clone(&builds),
            ),
        )
        .expect("mount");
    let requirements = AgentBackendRequirements {
        service_tier: AgentBackendOptionScope::Profile,
        ..AgentBackendRequirements::one_shot()
    };

    assert!(matches!(
        registry.build(&id, &requirements, &1),
        Err(AgentBackendFactoryError::Registry(
            AgentBackendRegistryError::Unsupported { .. }
        ))
    ));
    assert_eq!(builds.load(Ordering::Relaxed), 0);
}

#[test]
fn resolved_factory_generation_survives_unmount() {
    let registry = AgentBackendFactoryRegistry::new();
    let id = AgentBackendId::new("native-build").expect("id");
    let builds = Arc::new(AtomicUsize::new(0));
    let mount = registry
        .mount(
            id.clone(),
            factory(
                OneShotAgentBackendKind::NativeCodex,
                AgentBackendCapabilities::native_codex(),
                Arc::clone(&builds),
            ),
        )
        .expect("mount");
    let resolved = registry
        .resolve(&id, &AgentBackendRequirements::one_shot())
        .expect("resolve");
    drop(mount);

    assert_eq!(
        registry
            .resolve(&id, &AgentBackendRequirements::one_shot())
            .expect_err("new resolution must fail"),
        AgentBackendRegistryError::NotFound { id }
    );
    let backend = resolved.build(&2).expect("admitted factory can build");
    assert_eq!(backend.kind(), OneShotAgentBackendKind::NativeCodex);
    assert_eq!(builds.load(Ordering::Relaxed), 2);
}

#[test]
fn factory_result_must_match_its_advertised_contract() {
    let registry = AgentBackendFactoryRegistry::new();
    let id = AgentBackendId::new("lying-provider").expect("id");
    let expected = AgentBackendCapabilities::claude_code();
    let actual = AgentBackendCapabilities::one_shot_only();
    let builds = Arc::new(AtomicUsize::new(0));
    let _mount = registry
        .mount(
            id.clone(),
            Arc::new(RecordingFactory {
                advertised_kind: OneShotAgentBackendKind::ClaudeCode,
                advertised_capabilities: expected.clone(),
                revision: "test-factory/v1".to_string(),
                actual_kind: OneShotAgentBackendKind::Acp,
                actual_capabilities: actual.clone(),
                builds: Arc::clone(&builds),
            }),
        )
        .expect("mount");

    let error = match registry.build(&id, &AgentBackendRequirements::one_shot(), &1) {
        Ok(_) => panic!("contract drift must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        AgentBackendFactoryError::ContractMismatch {
            id,
            expected_kind: OneShotAgentBackendKind::ClaudeCode,
            actual_kind: OneShotAgentBackendKind::Acp,
            expected_capabilities: expected,
            actual_capabilities: actual,
        }
    );
    assert_eq!(builds.load(Ordering::Relaxed), 1);
}
