use crate::commands::raw_args::*;
use crate::core::{SylphieEvents, CoreRef};
use crate::errors::*;
use crate::module::Module;
use futures::future::BoxFuture;
use static_events::*;
use std::any::Any;

/// The implementation of a command context.
pub trait CommandCtxImpl: Sync + Send + 'static {
    /// Controls the way the arguments to commands in this context are parsed.
    fn args_parsing_options(&self) -> ArgParsingOptions {
        ArgParsingOptions::default()
    }

    /// Returns the raw message string to parse as a commmand.
    ///
    /// This should return the same value for every call.
    fn raw_message(&self) -> &str;

    /// Responds to the user with a given string.
    fn respond<'a>(
        &'a self, target: &'a Handler<impl Events>, msg: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
}

/// An argument to a command.
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub struct CommandArg<'a> {
    /// The original source span the text originated from.
    pub source_span: (usize, usize),
    /// The original text of the argument.
    pub source_text: &'a str,
    /// The parsed text of the argument.
    pub text: &'a str,
}

/// The context for a given command.
pub struct CommandCtx<E: Events> {
    handle: Handler<E>,
    args: Args,
    ctx_impl: Box<dyn CommandCtxImplWrapper<E>>,
}
impl <R: Module> CommandCtx<SylphieEvents<R>> {
    /// Creates a new command context given an implementation and a [`Handler`].
    pub fn new(core: &CoreRef<R>, ctx_impl: impl CommandCtxImpl) -> Self {
        let args = Args::parse(ctx_impl.args_parsing_options(), ctx_impl.raw_message());
        CommandCtx {
            handle: core.lock(),
            args,
            ctx_impl: Box::new(ctx_impl),
        }
    }
}
impl <E: Events> CommandCtx<E> {
    /// Returns the underlying event handler.
    pub fn handler(&self) -> &Handler<E> {
        &self.handle
    }

    /// Attempts to downcasts the internal [`CommandCtxImpl`] to a reference to the given type.
    ///
    /// This is not generally useful and should usually be wrapped by a context-specific helper.
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.ctx_impl.as_any().downcast_ref::<T>()
    }

    /// Returns the raw text of the command.
    pub fn raw_message(&self) -> &str {
        self.ctx_impl.raw_message()
    }

    /// Returns the number of arguments passed to this function.
    pub fn args_count(&self) -> usize {
        self.args.len()
    }

    /// Returns an argument passed to this function.
    pub fn arg(&self, i: usize) -> CommandArg<'_> {
        self.arg_opt(i).expect("Command index out of bounds.")
    }

    /// Returns an argument passed to this function.
    pub fn arg_opt(&self, i: usize) -> Option<CommandArg<'_>> {
        if i >= self.args_count() {
            None
        } else {
            let source = self.raw_message();
            let source_span = self.args.source_span(i);
            Some(CommandArg {
                source_span,
                source_text: &source[source_span.0..source_span.1],
                text: self.args.arg(source, i),
            })
        }
    }

    /// Responds to the user with a given string.
    pub async fn respond(&self, msg: &str) -> Result<()> {
        self.ctx_impl.respond(&self.handle, msg).await
    }
}

/// An object-safe wrapper around [`CommandCtxImpl`].
trait CommandCtxImplWrapper<E: Events>: Sync + Send + 'static {
    fn as_any(&self) -> &dyn Any;
    fn raw_message(&self) -> &str;

    fn respond<'a>(&'a self, target: &'a Handler<E>, msg: &'a str) -> BoxFuture<'a, Result<()>>;
}
impl <E: Events, T: CommandCtxImpl> CommandCtxImplWrapper<E> for T {
    fn as_any(&self) -> &dyn Any { self }
    fn raw_message(&self) -> &str { self.raw_message() }

    fn respond<'a>(&'a self, target: &'a Handler<E>, msg: &'a str) -> BoxFuture<'a, Result<()>> {
        self.respond(target, msg)
    }
}