//! Command-line entry point and subcommand dispatch.

mod allocator;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> std::process::ExitCode {
    if let Some(code) = zuno_sandbox::run_helper_from_args() {
        return code;
    }
    if let Some(code) = allocator::ensure_tuned_allocator() {
        return code;
    }
    if let Some(code) = zuno_process::run_guard_from_args() {
        return code;
    }
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("failed to locate zuno for child-process containment: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(error) = zuno_process::activate_guard_executable(executable) {
        eprintln!("failed to activate child-process containment: {error}");
        return std::process::ExitCode::FAILURE;
    }
    zuno_cli::run_process()
}
