//! Builtins that yield to the async runtime before running. brush never
//! suspends inside a loop made only of builtins (`while :; do :; done`), so
//! without this nosh could not notice Ctrl-C or an agent timeout there.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use brush_core::extensions::DefaultShellExtensions;
use brush_core::{CommandArg, ExecutionContext, ExecutionExitCode, ExecutionResult};

type Registration = brush_core::builtins::Registration<DefaultShellExtensions>;
type ExecFn = brush_core::builtins::CommandExecuteFunc<DefaultShellExtensions>;
type BuiltinFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ExecutionResult, brush_core::Error>> + Send + 'a>>;

static ORIGINAL: OnceLock<HashMap<String, ExecFn>> = OnceLock::new();

/// Replaces each builtin's entry point with one that yields first.
pub fn wrap(builtins: &mut HashMap<String, Registration>) {
    let original = ORIGINAL.get_or_init(|| {
        builtins
            .iter()
            .map(|(name, reg)| (name.clone(), reg.execute_func))
            .collect()
    });
    for (name, reg) in builtins.iter_mut() {
        if original.contains_key(name) {
            reg.execute_func = run_yielding;
        }
    }
}

fn run_yielding(ctx: ExecutionContext<'_>, args: Vec<CommandArg>) -> BuiltinFuture<'_> {
    Box::pin(async move {
        tokio::task::yield_now().await;
        match ORIGINAL
            .get()
            .and_then(|m| m.get(ctx.command_name.as_str()))
        {
            Some(f) => f(ctx, args).await,
            None => Ok(ExecutionExitCode::GeneralError.into()),
        }
    })
}
