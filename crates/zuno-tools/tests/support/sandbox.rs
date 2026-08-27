#![allow(
    dead_code,
    reason = "each integration-test crate imports the shared sandbox fixture independently"
)]

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use zuno_sandbox::{
    NetworkAccess, PrepareRequest, PreparedCommand, SandboxBackend, SandboxCapabilities,
    SandboxError, SandboxMode, SandboxPolicy,
};
use zuno_tools::shell::ShellTool;

#[derive(Debug)]
struct DirectTestSandbox {
    capabilities: SandboxCapabilities,
}

impl DirectTestSandbox {
    fn new() -> Self {
        Self {
            capabilities: SandboxCapabilities {
                backend: "test_direct".to_owned(),
                executable: Some("/bin/sh".into()),
                read_only: true,
                workspace_write: true,
                danger_full_access: true,
                network_isolation: true,
            },
        }
    }
}

impl SandboxBackend for DirectTestSandbox {
    fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }

    fn prepare(&self, request: PrepareRequest) -> Result<PreparedCommand, SandboxError> {
        let program = request.program.clone();
        let arguments = request.arguments.clone();
        let writable_roots = if request.policy.mode() == SandboxMode::WorkspaceWrite {
            vec![request.policy.workspace().to_owned()]
        } else {
            Vec::new()
        };
        Ok(PreparedCommand::from_backend(
            request,
            program,
            arguments,
            &self.capabilities,
            writable_roots,
            Vec::new(),
        ))
    }
}

pub fn shell_tool(workspace: &Path) -> ShellTool {
    configured_shell_tool(workspace, None)
}

pub fn configured_shell_tool(workspace: &Path, shell: Option<&str>) -> ShellTool {
    let policy = SandboxPolicy::new(
        workspace,
        SandboxMode::WorkspaceWrite,
        NetworkAccess::Allowed,
    )
    .expect("test sandbox policy");
    ShellTool::with_sandbox_backend(workspace, shell, Arc::new(DirectTestSandbox::new()), policy)
        .expect("shell tool")
}

pub fn direct_prepared(workspace: &Path, command: &str) -> PreparedCommand {
    let backend = DirectTestSandbox::new();
    backend
        .prepare(PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-c"), OsString::from(command)],
            cwd: workspace.to_owned(),
            environment: std::env::vars_os().collect(),
            policy: SandboxPolicy::new(
                workspace,
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Allowed,
            )
            .expect("test sandbox policy"),
        })
        .expect("prepared test command")
}
