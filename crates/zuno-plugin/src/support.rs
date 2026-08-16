use crate::HookName;

/// One advertised hook and the production lifecycle boundary that dispatches it.
///
/// This is the shared source for runtime coverage tests and generated authoring
/// documentation. The hook list comes from [`HookName::ALL`]; [`production_trigger`]
/// is deliberately exhaustive so adding a hook without assigning a real boundary is
/// a compile error rather than a stale documentation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookSupport {
    pub hook: HookName,
    pub production_trigger: &'static str,
}

/// Every advertised hook paired with its production lifecycle boundary.
pub fn hook_support() -> impl ExactSizeIterator<Item = HookSupport> {
    HookName::ALL.into_iter().map(|hook| HookSupport {
        hook,
        production_trigger: production_trigger(hook),
    })
}

/// The production lifecycle boundary for one hook.
#[must_use]
pub const fn production_trigger(hook: HookName) -> &'static str {
    match hook {
        HookName::Dispose => "runtime shutdown, before the plugin host is torn down",
        HookName::Event => "each event published on the real turn event stream",
        HookName::Config => "configuration finalization before turn composition",
        HookName::Tool => "executable tool-registry assembly",
        HookName::Auth => "provider-catalog credential enrichment",
        HookName::Provider => "provider-catalog model enrichment",
        HookName::ChatMessage => "user-message construction before persistence",
        HookName::ChatParams => "provider request preparation after model resolution",
        HookName::ChatHeaders => "provider request preparation after model resolution",
        HookName::PermissionAsk => "tool permission decision before interactive approval",
        HookName::CommandExecuteBefore => "command expansion before generated parts are persisted",
        HookName::ToolExecuteBefore => "tool dispatch before validation, permission, and execution",
        HookName::ShellEnv => "shell child-process environment construction",
        HookName::ToolExecuteAfter => "tool completion before result persistence",
        HookName::ChatMessagesTransform => "history projection before provider request preparation",
        HookName::ChatSystemTransform => {
            "system-prompt assembly before provider request preparation"
        }
        HookName::ProviderSmallModel => "internal-agent model resolution",
        HookName::SessionCompacting => "compaction request assembly before summary generation",
        HookName::CompactionAutocontinue => "overflow decision before automatic compaction",
        HookName::TextComplete => "completed text part before its final checkpoint",
        HookName::ToolDefinition => "tool-definition snapshot before provider request preparation",
    }
}
