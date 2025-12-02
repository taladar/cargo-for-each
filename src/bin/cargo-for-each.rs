#![doc = include_str!("../../README.md")]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use cargo_for_each::error::Error;
use cargo_for_each::plans::{PlanParameters, plan_command};
use cargo_for_each::target_sets::{TargetSetParameters, target_set_command};
use cargo_for_each::targets::CrateType;
use cargo_for_each::targets_commands::{TargetParameters, target_command};
use cargo_for_each::tasks::{TaskParameters, task_command};

use tracing::instrument;
use tracing_subscriber::{
    EnvFilter, Layer as _, Registry, filter::LevelFilter, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

/// checks if the given path is an executable file
///
/// on unix this checks for the executable bit, on windows it checks
/// for valid extensions and on other platforms it just checks for
/// the presence of a file
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs_err::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// checks if the given path is an executable file
///
/// on unix this checks for the executable bit, on windows it checks
/// for valid extensions and on other platforms it just checks for
/// the presence of a file
#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    // On Windows, executability is determined by file extension.
    // We check against PATHEXT environment variable.
    if path.extension().is_some() && path.is_file() {
        return true;
    }
    if let Some(pathext) = std::env::var_os("PATHEXT") {
        let pathexts = pathext.to_string_lossy();
        for ext in pathexts.split(';').filter(|s| !s.is_empty()) {
            let mut path_with_ext = path.as_os_str().to_owned();
            path_with_ext.push(ext);
            if Path::new(&path_with_ext).is_file() {
                return true;
            }
        }
    }
    path.is_file()
}

/// checks if the given path is an executable file
///
/// on unix this checks for the executable bit, on windows it checks
/// for valid extensions and on other platforms it just checks for
/// the presence of a file
#[cfg(all(not(unix), not(windows)))]
fn is_executable(path: &Path) -> bool {
    // Fallback for non-unix, non-windows systems.
    path.is_file()
}

/// Parameters for exec subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct ExecSubcommand {
    /// The command to execute.
    #[clap(required = true)]
    pub command: String,
    /// The arguments for the command.
    #[clap(last = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parameters for executing commands on crates
#[derive(clap::Parser, Debug, Clone)]
pub struct CrateExecParameters {
    /// only execute on crates of this type
    #[clap(long)]
    pub r#type: Option<CrateType>,

    /// only execute on crates that are standalone or not
    #[clap(long)]
    pub standalone: Option<bool>,

    /// The command to execute
    #[clap(flatten)]
    pub exec_subcommand: ExecSubcommand,
}

/// Parameters for executing commands on workspaces
#[derive(clap::Parser, Debug, Clone)]
pub struct WorkspaceExecParameters {
    /// only execute on multi-crate workspaces
    #[clap(long)]
    pub no_standalone: bool,

    /// The command to execute
    #[clap(flatten)]
    pub exec_subcommand: ExecSubcommand,
}

/// The type of object to execute a command on
#[derive(clap::Parser, Debug, Clone)]
pub enum ExecType {
    /// Execute a command in each workspace directory
    Workspaces(WorkspaceExecParameters),
    /// Execute a command in each crate directory
    Crates(CrateExecParameters),
}

/// Parameters for exec subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct ExecParameters {
    /// The type of object to execute on
    #[clap(subcommand)]
    pub exec_type: ExecType,
}

/// which subcommand to call
#[derive(clap::Parser, Debug)]
pub enum Command {
    /// Manage workspaces and crates (add, remove, list, refresh).
    Target(TargetParameters),
    /// create a new target set
    TargetSet(TargetSetParameters),
    /// manage plans
    Plan(PlanParameters),
    /// manage tasks
    Task(TaskParameters),
    /// Execute a command in each configured directory
    Exec(ExecParameters),
    /// Generate man page
    GenerateManpage {
        /// target dir for man page generation
        #[clap(long)]
        output_dir: PathBuf,
    },
    /// Generate shell completion
    GenerateShellCompletion {
        /// output file for shell completion generation
        #[clap(long)]
        output_file: PathBuf,
        /// which shell
        #[clap(long)]
        shell: clap_complete::aot::Shell,
    },
}

/// The Clap type for all the commandline parameters
#[derive(clap::Parser, Debug)]
#[clap(name = "cargo-for-each",
       about = clap::crate_description!(),
       author = clap::crate_authors!(),
       version = clap::crate_version!(),
       )]
struct Options {
    /// which subcommand to use
    #[clap(subcommand)]
    command: Command,
}

/// implementation of the exec subcommand
///
/// # Errors
///
/// fails if the implementation of exec fails
#[instrument]
async fn exec_command(exec_parameters: ExecParameters) -> Result<(), Error> {
    let config = cargo_for_each::Config::load()?;

    let (exec_type_str, dirs, command, args) = match exec_parameters.exec_type {
        ExecType::Workspaces(params) => {
            let filtered_workspaces = config
                .workspaces
                .into_iter()
                .filter(|w| !params.no_standalone || !w.is_standalone)
                .map(|w| w.manifest_dir)
                .collect::<Vec<_>>();

            let mut description = String::from("workspaces");
            if params.no_standalone {
                write!(&mut description, " that are not standalone")?;
            }

            (
                description,
                filtered_workspaces,
                params.exec_subcommand.command,
                params.exec_subcommand.args,
            )
        }
        ExecType::Crates(crate_params) => {
            let workspace_standalone_map: HashMap<_, _> = config
                .workspaces
                .iter()
                .map(|w| (w.manifest_dir.clone(), w.is_standalone))
                .collect();
            let filtered_crates = config
                .crates
                .into_iter()
                .filter(|krate| {
                    if let Some(t) = &crate_params.r#type
                        && !krate.types.contains(t)
                    {
                        return false;
                    }
                    if let Some(standalone) = crate_params.standalone
                        && workspace_standalone_map
                            .get(&krate.workspace_manifest_dir)
                            .is_none_or(|&is_standalone| is_standalone != standalone)
                    {
                        return false;
                    }
                    true
                })
                .map(|c| c.manifest_dir)
                .collect::<Vec<_>>();

            let mut description = String::from("crates");
            if let Some(crate_type) = &crate_params.r#type {
                write!(&mut description, " of type {crate_type:?}")?;
            }
            if let Some(standalone) = crate_params.standalone {
                write!(&mut description, " with standalone={standalone}",)?;
            }
            (
                description,
                filtered_crates,
                crate_params.exec_subcommand.command,
                crate_params.exec_subcommand.args,
            )
        }
    };

    tracing::debug!(
        "Executing command `{} {:?}` for all {}",
        command,
        args,
        exec_type_str
    );

    // Check if command exists and is executable before iterating
    let command_path = Path::new(&command);
    let command_is_executable = if command_path.is_absolute() {
        is_executable(command_path)
    } else {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths).find(|p| is_executable(&p.join(&command)))
            })
            .is_some()
    };

    if !command_is_executable {
        return Err(Error::CommandNotFound(command));
    }

    for dir in dirs {
        tracing::debug!("Executing `{} {:?}` in {}", command, args, dir.display());
        let mut child = tokio::process::Command::new(&command)
            .args(&args)
            .current_dir(&dir)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| Error::CommandExecutionError {
                manifest_dir: dir.clone(),
                command: vec![command.clone()]
                    .into_iter()
                    .chain(args.clone().into_iter())
                    .collect(),
                source: e,
            })?;

        let status = child
            .wait()
            .await
            .map_err(|e| Error::CommandExecutionError {
                manifest_dir: dir.clone(),
                command: vec![command.clone()]
                    .into_iter()
                    .chain(args.clone().into_iter())
                    .collect(),
                source: e,
            })?;

        if !status.success() {
            tracing::error!(
                "Command `{} {:?}` failed in `{}` with status {}",
                command,
                args,
                dir.display(),
                status
            );
        }
    }

    Ok(())
}

/// The main behaviour of the binary should go here
///
/// # Errors
///
/// fails if the main behavior of the application fails
#[instrument]
async fn do_stuff() -> Result<(), Error> {
    let options = <Options as clap::Parser>::parse();
    tracing::debug!("{:#?}", options);

    // main code either goes here or into the individual subcommands

    match options.command {
        Command::Target(target_parameters) => {
            target_command(target_parameters).await?;
        }
        Command::TargetSet(target_set_parameters) => {
            target_set_command(target_set_parameters).await?;
        }
        Command::Plan(plan_parameters) => {
            plan_command(plan_parameters).await?;
        }
        Command::Task(task_parameters) => {
            task_command(task_parameters).await?;
        }
        Command::Exec(exec_parameters) => {
            exec_command(exec_parameters).await?;
        }
        Command::GenerateManpage { output_dir } => {
            // generate man pages
            clap_mangen::generate_to(<Options as clap::CommandFactory>::command(), output_dir)
                .map_err(Error::GenerateManpageError)?;
        }
        Command::GenerateShellCompletion { output_file, shell } => {
            let mut f =
                std::fs::File::create(output_file).map_err(Error::GenerateShellCompletionError)?;
            let mut c = <Options as clap::CommandFactory>::command();
            clap_complete::generate(shell, &mut c, "cargo-for-each", &mut f);
        }
    }

    Ok(())
}

/// The main function mainly just handles setting up tracing
/// and handling any Err Results.
#[tokio::main]
async fn main() -> Result<(), Error> {
    let terminal_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .parse(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string()))?;
    let file_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .parse(std::env::var("CARGO_FOR_EACH_LOG").unwrap_or_else(|_| "trace".to_string()))?;
    #[cfg(target_os = "linux")]
    let journald_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .parse(
            std::env::var("CARGO_FOR_EACH_JOURNALD_LOG").unwrap_or_else(|_| "info".to_string()),
        )?;
    let registry = Registry::default();
    let registry =
        registry.with(tracing_subscriber::fmt::Layer::default().with_filter(terminal_env_filter));
    let log_dir = std::env::var("CARGO_FOR_EACH_LOG_DIR");
    let file_layer = if let Ok(log_dir) = log_dir {
        let log_file = if let Ok(log_file) = std::env::var("CARGO_FOR_EACH_LOG_FILE") {
            log_file
        } else {
            "cargo_for_each.log".to_string()
        };
        let file_appender = tracing_appender::rolling::never(log_dir, log_file);
        Some(
            tracing_subscriber::fmt::Layer::default()
                .with_writer(file_appender)
                .with_filter(file_env_filter),
        )
    } else {
        None
    };
    let registry = registry.with(file_layer);
    #[cfg(target_os = "linux")]
    let registry = registry.with(
        tracing_journald::layer()
            .map_err(Error::TracingJournaldError)?
            .with_filter(journald_env_filter),
    );
    registry.init();
    log_panics::init();
    #[expect(
        clippy::print_stderr,
        reason = "This is the final print in our error chain and we already log this with tracing above but depending on log level the tracing output is not seen by the user"
    )]
    match do_stuff().await {
        Ok(()) => (),
        Err(e) => {
            tracing::error!("{e}");
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    tracing::debug!("Exiting");
    Ok(())
}

#[cfg(test)]
mod test {
    //use super::*;
    //use pretty_assertions::{assert_eq, assert_ne};
}
