use crate::CompletionArgs;

pub(super) fn execute(args: &CompletionArgs) -> Result<(), String> {
    let mut command = crate::clap_command();
    clap_complete::generate(args.shell, &mut command, "zuno", &mut std::io::stdout());
    Ok(())
}
