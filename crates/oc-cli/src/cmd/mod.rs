mod agent;
mod db;
mod db_maint;
mod debug;
mod mcp;
mod models;
mod providers;
mod run;
mod serve;
mod session;
mod session_list;
mod session_prune;

use crate::{
    CommandDispatcher, DispatchArguments, DispatchError, DispatchRequest, PendingCommandDispatcher,
};

#[derive(Debug, Default)]
pub(crate) struct HeadlessCommandDispatcher;

impl CommandDispatcher for HeadlessCommandDispatcher {
    fn dispatch(&mut self, request: DispatchRequest) -> Result<(), DispatchError> {
        if let DispatchArguments::Db(args) = &request.args {
            return db::execute(args)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Session(args) = &request.args {
            return session::execute(args)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Agent(args) = &request.args {
            return agent::execute(args, &request.environment)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Models(args) = &request.args {
            return models::execute(args, &request.environment)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Providers(args) = &request.args {
            return providers::execute(args, &request.environment)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Mcp(args) = &request.args {
            return mcp::execute(args, &request.environment)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Debug(args) = &request.args {
            return debug::execute(args, &request.environment)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Serve(args) = &request.args {
            return serve::execute(args)
                .map_err(|error| DispatchError::command(request.command, error));
        }
        if let DispatchArguments::Run(args) = &request.args {
            return run::execute(args, &request.environment)
                .map_err(|error| DispatchError::command(request.command, error));
        }

        PendingCommandDispatcher.dispatch(request)
    }
}
