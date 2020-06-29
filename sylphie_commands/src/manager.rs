use arc_swap::ArcSwapOption;
use crate::commands::Command;
use crate::ctx::CommandCtx;
use fxhash::{FxHashMap, FxHashSet};
use static_events::prelude_async::*;
use std::sync::Arc;
use sylphie_core::errors::*;

/// The event used to register commands.
#[derive(Debug, Default)]
pub struct RegisterCommandsEvent {
    commands: Vec<Command>,
}
self_event!(RegisterCommandsEvent);
impl RegisterCommandsEvent {
    /// Registers a new command.
    pub fn register_command(&mut self, command: Command) {
        self.commands.push(command);
    }
}

struct StringInterner(FxHashMap<String, Arc<str>>);
impl StringInterner {
    pub fn intern(&mut self, s: String) -> Arc<str> {
        (*self.0.entry(s.clone()).or_insert_with(|| s.into())).clone()
    }
}

#[derive(Clone, Debug)]
struct CommandSet {
    list: Arc<[Command]>,
    // a map of {base command name -> {possible prefix -> [possible commands]}}
    // an unprefixed command looks up an empty prefix
    by_name: FxHashMap<Arc<str>, FxHashMap<Arc<str>, Vec<Command>>>,
}
impl CommandSet {
    fn from_event(event: RegisterCommandsEvent) -> Self {
        let list = event.commands;

        let mut used_full_names = FxHashSet::default();
        let mut commands_for_name = FxHashMap::default();
        let mut root_warning_given = false;
        let mut interner = StringInterner(FxHashMap::default());
        for command in &list {
            let lc_name = command.full_name().to_ascii_lowercase();
            if used_full_names.contains(&lc_name) {
                warn!(
                    "Found duplicated command `{}`. One of the copies will not be accessible.",
                    command.full_name(),
                );
            } else {
                if !root_warning_given && command.module_name() == "__root__" {
                    warn!("Defining commands in the root module is not recommended.");
                    root_warning_given = true;
                }

                used_full_names.insert(lc_name);
                commands_for_name.entry(command.name().to_ascii_lowercase())
                    .or_insert(Vec::new()).push(command);
            }
        }
        let by_name = commands_for_name.into_iter().map(|(name, variants)| {
            let mut map = FxHashMap::default();
            for variant in variants {
                let mod_name = variant.module_name().to_ascii_lowercase();
                map.entry(interner.intern(mod_name.to_string()))
                    .or_insert(Vec::new()).push(variant.clone());
                map.entry(interner.intern(String::new()))
                    .or_insert(Vec::new()).push(variant.clone());
                for (i, _) in mod_name.char_indices().filter(|(_, c)| *c == '.') {
                    let prefix = mod_name[i+1..].to_string();
                    map.entry(interner.intern(prefix))
                        .or_insert(Vec::new()).push(variant.clone());
                }
            }
            (interner.intern(name.to_string()), map)
        }).collect();

        CommandSet { list: list.into(), by_name }
    }
}

/// The result of a command lookup.
pub enum CommandLookupResult {
    /// No matching commands were found.
    NoneFound,
    /// A single unambiguous command was found.
    Found(Command),
    /// An ambiguous set of commands was found.
    Ambigious(Vec<Command>),
}

/// The service used to lookup commands.
#[derive(Clone, Debug)]
pub struct CommandManager {
    null: CommandSet,
    data: ArcSwapOption<CommandSet>,
}
impl CommandManager {
    pub(crate) fn new() -> Self {
        CommandManager {
            null: CommandSet {
                list: Vec::new().into(),
                by_name: Default::default(),
            },
            data: ArcSwapOption::new(None),
        }
    }

    /// Reloads the command manager.
    pub async fn reload(&self, target: &Handler<impl Events>) {
        let new_set = CommandSet::from_event(target.dispatch_async(RegisterCommandsEvent {
            commands: Vec::new(),
        }).await);
        self.data.store(Some(Arc::new(new_set)));
    }

    /// Returns a list of all commands currently registered.
    pub fn command_list(&self) -> Arc<[Command]> {
        self.data.load().as_ref().map_or_else(|| self.null.list.clone(), |x| x.list.clone())
    }

    /// Looks ups a command for a given context.
    pub async fn lookup_command(
        &self, ctx: &CommandCtx<impl Events>, command: &str,
    ) -> Result<CommandLookupResult> {
        let command = command.to_ascii_lowercase();
        let split: Vec<_> = command.split(':').collect();
        let (group, name) = match split.as_slice() {
            &[name] => ("", name),
            &[group, name] => (group, name),
            _ => cmd_error!("No more than one `:` can appear in a command name."),
        };

        let data = self.data.load();
        let data = data.as_ref().map_or(&self.null, |x| &*x);
        Ok(match data.by_name.get(name) {
            Some(x) => match x.get(group) {
                Some(x) => {
                    let mut valid_commands = Vec::new();
                    for command in x {
                        if command.can_access(ctx).await? {
                            valid_commands.push(command.clone());
                        }
                    }
                    if valid_commands.len() == 0 {
                        CommandLookupResult::NoneFound
                    } else if valid_commands.len() == 1 {
                        CommandLookupResult::Found(valid_commands.pop().unwrap())
                    } else {
                        CommandLookupResult::Ambigious(valid_commands)
                    }
                },
                None => CommandLookupResult::NoneFound,
            },
            None => CommandLookupResult::NoneFound,
        })
    }

    /// Executes a command immediately.
    pub async fn execute(&self, ctx: &CommandCtx<impl Events>) -> Result<()> {
        if ctx.args_count() == 0 {
            ctx.respond("Command context contains no arguments?").await?;
        } else {
            let command = self.lookup_command(&ctx, ctx.arg(0).text).await?;
            match command {
                CommandLookupResult::NoneFound => ctx.respond("No such command found.").await?,
                CommandLookupResult::Found(cmd) => {
                    match cmd.execute(ctx).await {
                        Ok(()) => { }
                        Err(e) => {
                            // split to avoid saving a `&ErrorKind` which is !Send
                            let maybe_respond = match e.error_kind() {
                                ErrorKind::CommandError(e) => Some(e),
                                _ => { // TODO: Do something extensible
                                    e.report_error();
                                    None
                                },
                            };
                            if let Some(e) = maybe_respond {
                                ctx.respond(e).await?;
                            }
                        },
                    }
                }
                CommandLookupResult::Ambigious(cmds) => {
                    let mut str = String::new();
                    for cmd in cmds {
                        str.push_str(&format!("{}, ", cmd.full_name()));
                    }
                    ctx.respond(&format!("Command is ambiguous: {}", str)).await?;
                }
            }
        }
        Ok(())
    }
}