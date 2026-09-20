use std::path::PathBuf;

use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use pretty_assertions::assert_eq;

// Lives in its own test binary: the override is process-wide and must not leak
// into the resolver tests inside the library.
#[test]
fn override_wins_over_every_resolver_and_only_installs_once() {
    let context = InstallContext {
        method: InstallMethod::Other,
        package_layout: None,
    };
    let first = PathBuf::from("/opt/zuno/alias/codex-code-mode-host");
    assert!(InstallContext::set_code_mode_host_program_override(
        first.clone()
    ));
    assert_eq!(context.code_mode_host_program(), first);

    assert!(!InstallContext::set_code_mode_host_program_override(
        PathBuf::from("/elsewhere/codex-code-mode-host")
    ));
    assert_eq!(context.code_mode_host_program(), first);
}
