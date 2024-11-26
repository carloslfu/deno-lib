mod args;
mod auth_tokens;
mod cache;
mod cdp;
mod emit;
mod errors;
mod factory;
mod file_fetcher;
mod graph_container;
mod graph_util;
mod http_util;
mod js;
mod jsr;
mod lsp;
mod module_loader;
mod node;
mod npm;
mod ops;
mod resolver;
mod shared;
mod standalone;
mod task_runner;
mod tools;
mod tsc;
mod util;
mod version;
mod worker;

use crate::args::flags_from_vec;
use crate::args::DenoSubcommand;
use crate::args::Flags;
use crate::util::display;
use crate::util::v8::get_v8_flags_from_env;
use crate::util::v8::init_v8_flags;

use args::TaskFlags;
use deno_resolver::npm::ByonmResolvePkgFolderFromDenoReqError;
use deno_runtime::WorkerExecutionMode;
pub use deno_runtime::UNSTABLE_GRANULAR_FLAGS;
use npm::ResolvePkgFolderFromDenoReqError;

use deno_core::error::AnyError;
use deno_core::error::JsError;
use deno_core::futures::FutureExt;
use deno_core::unsync::JoinHandle;
use deno_npm::resolution::SnapshotFromLockfileError;
use deno_runtime::fmt_errors::format_js_error;
use deno_runtime::tokio_util::create_and_run_current_thread_with_maybe_metrics;
use deno_terminal::colors;
use factory::CliFactory;
use standalone::MODULE_NOT_FOUND;
use standalone::UNSUPPORTED_SCHEME;
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;

pub use deno_runtime;

pub fn run(cmd: &str) -> String {
    let args: Vec<_> = vec!["deno", "run", cmd]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect();

    let future = async move {
        // NOTE(lucacasonato): due to new PKU feature introduced in V8 11.6 we need to
        // initialize the V8 platform on a parent thread of all threads that will spawn
        // V8 isolates.
        let flags = resolve_flags_and_init(args)?;

        println!("flags: {:?}", flags);

        run_script(Arc::new(flags)).await
    };

    let result = create_and_run_current_thread_with_maybe_metrics(future);

    #[cfg(feature = "dhat-heap")]
    drop(profiler);

    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(err) => exit_for_error(err),
    }
}

pub async fn run_script(flags: Arc<Flags>) -> Result<i32, AnyError> {
    let handle = match flags.subcommand.clone() {
        DenoSubcommand::Run(run_flags) => spawn_subcommand(async move {
            let result =
                tools::run::run_script(WorkerExecutionMode::Run, flags.clone(), run_flags.watch)
                    .await;

            match result {
                Ok(v) => Ok(v),
                Err(script_err) => {
                    if let Some(ResolvePkgFolderFromDenoReqError::Byonm(
                        ByonmResolvePkgFolderFromDenoReqError::UnmatchedReq(_),
                    )) = script_err.downcast_ref::<ResolvePkgFolderFromDenoReqError>()
                    {
                        if flags.node_modules_dir.is_none() {
                            let mut flags = flags.deref().clone();
                            let watch = match &flags.subcommand {
                                DenoSubcommand::Run(run_flags) => run_flags.watch.clone(),
                                _ => unreachable!(),
                            };
                            flags.node_modules_dir =
                                Some(deno_config::deno_json::NodeModulesDirMode::None);
                            // use the current lockfile, but don't write it out
                            if flags.frozen_lockfile.is_none() {
                                flags.internal.lockfile_skip_write = true;
                            }
                            return tools::run::run_script(
                                WorkerExecutionMode::Run,
                                Arc::new(flags),
                                watch,
                            )
                            .await;
                        }
                    }
                    let script_err_msg = script_err.to_string();
                    if script_err_msg.starts_with(MODULE_NOT_FOUND)
                        || script_err_msg.starts_with(UNSUPPORTED_SCHEME)
                    {
                        if run_flags.bare {
                            let mut cmd = args::clap_root();
                            cmd.build();
                            let command_names = cmd
                                .get_subcommands()
                                .map(|command| command.get_name())
                                .collect::<Vec<_>>();
                            let suggestions = args::did_you_mean(&run_flags.script, command_names);
                            if !suggestions.is_empty() {
                                let mut error =
                                    clap::error::Error::<clap::error::DefaultFormatter>::new(
                                        clap::error::ErrorKind::InvalidSubcommand,
                                    )
                                    .with_cmd(&cmd);
                                error.insert(
                                    clap::error::ContextKind::SuggestedSubcommand,
                                    clap::error::ContextValue::Strings(suggestions),
                                );

                                Err(error.into())
                            } else {
                                Err(script_err)
                            }
                        } else {
                            let mut new_flags = flags.deref().clone();
                            let task_flags = TaskFlags {
                                cwd: None,
                                task: Some(run_flags.script.clone()),
                                is_run: true,
                            };
                            new_flags.subcommand = DenoSubcommand::Task(task_flags.clone());
                            let result = tools::task::execute_script(
                                Arc::new(new_flags),
                                task_flags.clone(),
                            )
                            .await;
                            match result {
                                Ok(v) => Ok(v),
                                Err(_) => {
                                    // Return script error for backwards compatibility.
                                    Err(script_err)
                                }
                            }
                        }
                    } else {
                        Err(script_err)
                    }
                }
            }
        }),
        _ => unreachable!(),
    };

    handle.await?
}

fn resolve_flags_and_init(args: Vec<std::ffi::OsString>) -> Result<Flags, AnyError> {
    let flags = match flags_from_vec(args) {
        Ok(flags) => flags,
        Err(err @ clap::Error { .. }) if err.kind() == clap::error::ErrorKind::DisplayVersion => {
            // Ignore results to avoid BrokenPipe errors.
            util::logger::init(None);
            let _ = err.print();
            std::process::exit(0);
        }
        Err(err) => {
            util::logger::init(None);
            exit_for_error(AnyError::from(err))
        }
    };

    util::logger::init(flags.log_level);

    // TODO(bartlomieju): remove in Deno v2.5 and hard error then.
    if flags.unstable_config.legacy_flag_enabled {
        log::warn!(
      "⚠️  {}",
      colors::yellow(
        "The `--unstable` flag has been removed in Deno 2.0. Use granular `--unstable-*` flags instead.\nLearn more at: https://docs.deno.com/runtime/manual/tools/unstable_flags"
      )
    );
    }

    let default_v8_flags = match flags.subcommand {
        // Using same default as VSCode:
        // https://github.com/microsoft/vscode/blob/48d4ba271686e8072fc6674137415bc80d936bc7/extensions/typescript-language-features/src/configuration/configuration.ts#L213-L214
        DenoSubcommand::Lsp => vec!["--max-old-space-size=3072".to_string()],
        _ => {
            // TODO(bartlomieju): I think this can be removed as it's handled by `deno_core`
            // and its settings.
            // deno_ast removes TypeScript `assert` keywords, so this flag only affects JavaScript
            // TODO(petamoriken): Need to check TypeScript `assert` keywords in deno_ast
            vec!["--no-harmony-import-assertions".to_string()]
        }
    };

    init_v8_flags(&default_v8_flags, &flags.v8_flags, get_v8_flags_from_env());
    // TODO(bartlomieju): remove last argument once Deploy no longer needs it
    deno_core::JsRuntime::init_platform(None, /* import assertions enabled */ false);

    Ok(flags)
}

fn exit_for_error(error: AnyError) -> ! {
    let mut error_string = format!("{error:?}");
    let mut error_code = 1;

    if let Some(e) = error.downcast_ref::<JsError>() {
        error_string = format_js_error(e);
    } else if let Some(SnapshotFromLockfileError::IntegrityCheckFailed(e)) =
        error.downcast_ref::<SnapshotFromLockfileError>()
    {
        error_string = e.to_string();
        error_code = 10;
    }

    exit_with_message(&error_string, error_code);
}

/// Ensure that the subcommand runs in a task, rather than being directly executed. Since some of these
/// futures are very large, this prevents the stack from getting blown out from passing them by value up
/// the callchain (especially in debug mode when Rust doesn't have a chance to elide copies!).
#[inline(always)]
fn spawn_subcommand<F: Future<Output = T> + 'static, T: SubcommandOutput>(
    f: F,
) -> JoinHandle<Result<i32, AnyError>> {
    // the boxed_local() is important in order to get windows to not blow the stack in debug
    deno_core::unsync::spawn(async move { f.map(|r| r.output()).await }.boxed_local())
}

fn exit_with_message(message: &str, code: i32) -> ! {
    log::error!(
        "{}: {}",
        colors::red_bold("error"),
        message.trim_start_matches("error: ")
    );
    std::process::exit(code);
}

/// Ensures that all subcommands return an i32 exit code and an [`AnyError`] error type.
trait SubcommandOutput {
    fn output(self) -> Result<i32, AnyError>;
}

impl SubcommandOutput for Result<i32, AnyError> {
    fn output(self) -> Result<i32, AnyError> {
        self
    }
}

impl SubcommandOutput for Result<(), AnyError> {
    fn output(self) -> Result<i32, AnyError> {
        self.map(|_| 0)
    }
}

impl SubcommandOutput for Result<(), std::io::Error> {
    fn output(self) -> Result<i32, AnyError> {
        self.map(|_| 0).map_err(|e| e.into())
    }
}

pub(crate) fn unstable_exit_cb(feature: &str, api_name: &str) {
    log::error!(
        "Unstable API '{api_name}'. The `--unstable-{}` flag must be provided.",
        feature
    );
    std::process::exit(70);
}
