mod agent;
mod child_turn;
mod db;
mod db_maint;
mod debug;
mod export;
mod mcp;
mod mcp_runtime;
mod models;
mod providers;
mod run;
mod serve;
mod session;
mod session_list;
mod session_prune;
mod tool_runtime;
mod tui;
mod tui_lsp;
mod tui_permission;
mod tui_question;
mod tui_reference;
mod tui_replay;
mod turn;

use crate::{
    CommandDispatcher, DispatchArguments, DispatchError, DispatchRequest, PendingCommandDispatcher,
};

#[derive(Debug, Default)]
pub(crate) struct HeadlessCommandDispatcher;

impl CommandDispatcher for HeadlessCommandDispatcher {
    /// Route one request to its handler.
    ///
    /// This is one exhaustive `match` rather than a chain of `if let` probes so
    /// that adding a [`DispatchArguments`] variant without a handler fails to
    /// compile. The previous chain fell through to [`PendingCommandDispatcher`]
    /// for anything it did not recognise, which is how `export` came to be
    /// registered, documented and advertised as implemented while every
    /// invocation exited 1.
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
            DispatchArguments::Debug(args) => {
                debug::execute(args, &request.environment).map_err(to_error)
            }
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
            DispatchArguments::Pending(_, _) => PendingCommandDispatcher.dispatch(request),
        }
    }
}
