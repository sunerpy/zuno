mod agent;
pub(crate) mod child_turn;
mod completion;
mod db;
mod db_maint;
mod debug;
mod export;
mod mcp;
mod mcp_runtime;
mod models;
mod plugin;
mod product_agent;
mod providers;
mod run;
mod self_update;
mod serve;
mod session;
mod session_list;
mod session_prune;
mod terminal_prompt;
mod tool_runtime;
mod tui;
mod tui_lsp;
mod tui_permission;
mod tui_question;
mod tui_reference;
mod tui_replay;
mod turn;

use crate::{CommandDispatcher, DispatchArguments, DispatchError, DispatchRequest};

#[derive(Debug, Default)]
pub(crate) struct HeadlessCommandDispatcher;

impl CommandDispatcher for HeadlessCommandDispatcher {
    /// Route one request to its handler.
    ///
    /// This is one exhaustive `match` rather than a chain of `if let` probes so
    /// that adding a [`DispatchArguments`] variant without a handler fails to
    /// compile. There is no fallback handler: a command cannot be registered unless
    /// this match names its concrete implementation.
    fn dispatch(&mut self, request: DispatchRequest) -> Result<(), DispatchError> {
        let command = request.command;
        let to_error = |error: String| DispatchError::command(command, error);
        match &request.args {
            DispatchArguments::Db(args) => db::execute(args).map_err(to_error),
            DispatchArguments::Session(args) => session::execute(args).map_err(to_error),
            DispatchArguments::Agent(args) => {
                agent::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Models(args) => {
                models::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Providers(args) => {
                providers::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Mcp(args) => {
                mcp::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Plugin(args) => {
                plugin::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Debug(args) => {
                debug::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Completion(args) => completion::execute(args).map_err(to_error),
            DispatchArguments::SelfUpdate(args) => self_update::execute(args).map_err(to_error),
            DispatchArguments::Serve(args) => {
                serve::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Run(args) => {
                run::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Tui(args) => {
                tui::execute(args, &request.environment).map_err(to_error)
            }
            DispatchArguments::Export(args) => export::export(args).map_err(to_error),
            DispatchArguments::Import(args) => export::import(args).map_err(to_error),
        }
    }
}
