//! This module defines the structures and functions for managing tasks.
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::{self, StreamExt as _, TryStreamExt as _};

use tracing::instrument;

use crate::error::Error;
use crate::plans::Step;
use crate::target_sets::load_target_set;

use crate::Config;
use clap::Parser;

/// returns the tasks dir path
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn dir_path() -> Result<PathBuf, Error> {
    Ok(crate::config_dir_path()?.join("tasks"))
}

/// returns the path to a specific task directory
///
/// # Errors
///
/// Returns an error if the tasks directory path cannot be determined.
pub fn named_dir_path(name: &str) -> Result<PathBuf, Error> {
    Ok(dir_path()?.join(name))
}

/// Parameters for creating a new task
#[derive(Parser, Debug, Clone)]
pub struct CreateTaskParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
    /// the name of the plan to use for the task
    #[clap(long)]
    pub plan: String,
    /// the name of the target set to use for the task
    #[clap(long)]
    pub target_set: String,
}

/// Parameters for running a single step of a task
#[derive(Parser, Debug, Clone)]
pub struct RunSingleStepParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
}

/// Parameters for running a task on a single target
#[derive(Parser, Debug, Clone)]
pub struct RunSingleTargetParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
}

/// Parameters for running a task on all targets
#[derive(Parser, Debug, Clone)]
pub struct RunAllTargetsParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
    /// Number of parallel jobs to run (similar to make -j)
    #[clap(short = 'j', long)]
    pub jobs: Option<usize>,
    /// Keep going when some targets fail (similar to make -k)
    #[clap(short = 'k', long)]
    pub keep_going: bool,
}

/// The `task run` subcommand
#[derive(Parser, Debug, Clone)]
pub enum TaskRunCommand {
    /// Run a single step of a task
    SingleStep(RunSingleStepParameters),
    /// Run a task on a single target
    SingleTarget(RunSingleTargetParameters),
    /// Run a task on all targets
    AllTargets(RunAllTargetsParameters),
}

/// Parameters for the `task run` subcommand
#[derive(Parser, Debug, Clone)]
pub struct TaskRunParameters {
    /// the `task run` subcommand to run
    #[clap(subcommand)]
    pub command: TaskRunCommand,
}

/// The `task` subcommand
#[derive(Parser, Debug, Clone)]
pub enum TaskCommand {
    /// Create a new task
    Create(CreateTaskParameters),
    /// Remove a task
    Remove(RemoveTaskParameters),
    /// Run a task
    Run(TaskRunParameters),
}

/// Parameters for removing a task
#[derive(Parser, Debug, Clone)]
pub struct RemoveTaskParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
}

/// Parameters for task subcommand
#[derive(Parser, Debug, Clone)]
pub struct TaskParameters {
    /// the `task` subcommand to run
    #[clap(subcommand)]
    pub command: TaskCommand,
}

/// implementation of the task create subcommand
///
/// # Errors
///
/// fails if the implementation of task create fails
#[instrument]
pub async fn task_create_command(params: CreateTaskParameters) -> Result<(), Error> {
    // 1. Validate plan and target-set existence.
    let plan_file_path = crate::plans::dir_path()?.join(format!("{}.toml", params.plan));
    if !plan_file_path.exists() {
        return Err(Error::PlanNotFound(params.plan));
    }

    let target_set_file_path =
        crate::target_sets::dir_path()?.join(format!("{}.toml", params.target_set));
    if !target_set_file_path.exists() {
        return Err(Error::TargetSetNotFound(params.target_set));
    }

    // 2. Load the Config
    let config = Config::load()?;

    // 3. Load the TargetSet.
    let target_set = load_target_set(&params.target_set)?;

    // 4. Resolve the TargetSet.
    let resolved_target_set = crate::target_sets::resolve_target_set(&target_set, &config)?;

    // 5. Create the task directory.
    let task_dir = named_dir_path(&params.name)?;
    if task_dir.exists() {
        return Err(Error::AlreadyExists(format!("task {}", params.name)));
    }
    fs_err::create_dir_all(&task_dir)
        .map_err(|e| Error::CouldNotCreateTaskDir(task_dir.clone(), e))?;

    // 6. Copy plan and target-set files.
    fs_err::copy(&plan_file_path, task_dir.join("plan.toml")).map_err(|e| {
        Error::CouldNotCopyFile(plan_file_path.clone(), task_dir.join("plan.toml"), e)
    })?;

    fs_err::copy(&target_set_file_path, task_dir.join("target-set.toml")).map_err(|e| {
        Error::CouldNotCopyFile(
            target_set_file_path.clone(),
            task_dir.join("target-set.toml"),
            e,
        )
    })?;

    // 7. Save the resolved target set to `resolved-target-set.toml`.
    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    fs_err::write(
        &resolved_target_set_path,
        toml::to_string(&resolved_target_set).map_err(Error::CouldNotSerializeResolvedTargetSet)?,
    )
    .map_err(Error::CouldNotWriteResolvedTargetSet)?;

    Ok(())
}

/// Runs a single step.
///
/// # Errors
///
/// Returns an error if the step fails to execute.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_step(
    step: &Step,
    manifest_dir: &Path,
    task_name: &str,
    step_number: usize,
    target_number: usize,
) -> Result<(), Error> {
    let state_dir = dirs::state_dir()
        .ok_or(Error::CouldNotDetermineStateDir)?
        .join("cargo-for-each")
        .join("tasks")
        .join(task_name)
        .join(target_number.to_string())
        .join(step_number.to_string());

    fs_err::create_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;

    let cast_path = state_dir.join("asciinema.cast");

    match step {
        Step::RunCommand { command, args } => {
            let mut cmd = Command::new("asciinema");

            if !crate::utils::command_is_executable(command) {
                return Err(crate::error::Error::CommandNotFound(command.to_owned()));
            }

            let command_str = format!("{} {}", command, args.join(" "));
            cmd.arg("record")
                .arg("-c")
                .arg(&command_str)
                .arg(&cast_path);
            cmd.current_dir(manifest_dir);

            let status = cmd.status().map_err(|e| {
                Error::CommandExecutionFailed(
                    "asciinema".to_string(),
                    manifest_dir.to_path_buf(),
                    e,
                )
            })?;

            let exit_status_path = state_dir.join("exit_status");
            fs_err::write(
                &exit_status_path,
                status
                    .code()
                    .map_or_else(|| "".to_string(), |c| c.to_string()),
            )
            .map_err(|e| Error::CouldNotWriteStateFile(exit_status_path, e))?;

            if !status.success() {
                return Err(Error::CommandFailed(
                    command_str,
                    manifest_dir.to_path_buf(),
                    status,
                ));
            }
        }
        Step::ManualStep {
            title,
            instructions,
        } => {
            println!("--- Manual Step: {title} ---");
            println!("{instructions}");
            println!(
                "Starting a recording shell in {}. Press Ctrl+D or type `exit` to continue.",
                manifest_dir.display()
            );

            let mut cmd = Command::new("asciinema");
            cmd.arg("record").arg(&cast_path);
            cmd.current_dir(manifest_dir);

            let status = cmd.status().map_err(|e| {
                Error::CommandExecutionFailed(
                    "asciinema".to_string(),
                    manifest_dir.to_path_buf(),
                    e,
                )
            })?;

            if !status.success() {
                println!("Shell exited with a non-zero status code: {status}");
            }

            print!("Was the manual step completed successfully? (y/N) ");
            io::stdout().flush().map_err(Error::IoError)?;
            let mut confirmation = String::new();
            io::stdin()
                .read_line(&mut confirmation)
                .map_err(Error::IoError)?;

            let manual_step_confirmed_path = state_dir.join("manual_step_confirmed");
            let confirmed = confirmation.trim().to_lowercase() == "y"
                || confirmation.trim().to_lowercase() == "yes";

            fs_err::write(
                &manual_step_confirmed_path,
                if confirmed { "y" } else { "n" },
            )
            .map_err(|e| Error::CouldNotWriteStateFile(manual_step_confirmed_path, e))?;

            if !confirmed {
                return Err(Error::ManualStepNotConfirmed);
            }
        }
    }
    Ok(())
}

use crate::plans::Plan;
use crate::target_sets::ResolvedTargetSet;

/// Represents the next uncompleted step in a task for a specific target.
#[derive(Debug)]
pub struct NextStep<'a> {
    /// The step to be executed.
    pub step: &'a Step,
    /// The manifest directory of the target.
    pub manifest_dir: &'a Path,
    /// The name of the task.
    pub task_name: &'a str,
    /// The 1-based index of the step within the plan.
    pub step_number: usize,
    /// The 0-based index of the target within the resolved target set.
    pub target_number: usize,
}

/// Checks if a given step for a specific target in a task has been completed.
fn is_step_completed(
    task_name: &str,
    target_number: usize,
    step_number: usize,
    step: &Step,
) -> bool {
    let state_dir_option = dirs::state_dir().map(|path| {
        path.join("cargo-for-each")
            .join("tasks")
            .join(task_name)
            .join(target_number.to_string())
            .join(step_number.to_string())
    });

    let Some(state_dir) = state_dir_option else {
        // If we can't determine the state dir, assume not completed to be safe.
        return false;
    };

    if !state_dir.exists() {
        return false;
    }

    match step {
        Step::RunCommand { .. } => {
            let exit_status_path = state_dir.join("exit_status");
            if let Ok(content) = fs_err::read_to_string(exit_status_path) {
                content.trim() == "0"
            } else {
                false
            }
        }
        Step::ManualStep { .. } => {
            let manual_step_confirmed_path = state_dir.join("manual_step_confirmed");
            if let Ok(content) = fs_err::read_to_string(manual_step_confirmed_path) {
                content.trim() == "y"
            } else {
                false
            }
        }
    }
}

#[must_use]
/// Determines the first step that has not already been completed successfully
/// in the first target that has such a step.
///
/// Returns the values required to call the `run_single_step()` utility function on it.
pub fn find_next_step<'a>(
    task_name: &'a str,
    plan: &'a Plan,
    resolved_target_set: &'a ResolvedTargetSet,
) -> Option<NextStep<'a>> {
    for (target_idx, target) in resolved_target_set.targets.iter().enumerate() {
        for (step_idx, step) in plan.steps.iter().enumerate() {
            // step_number should be 1-based for the state directory path.
            if !is_step_completed(task_name, target_idx, step_idx.saturating_add(1), step) {
                return Some(NextStep {
                    step,
                    manifest_dir: &target.manifest_dir,
                    task_name,
                    step_number: step_idx.saturating_add(1),
                    target_number: target_idx,
                });
            }
        }
    }
    None
}

/// implementation of the task run single-step subcommand
///
/// # Errors
///
/// fails if the implementation of task run single-step fails
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_step_command(params: RunSingleStepParameters) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name)?;

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    if let Some(next_step) = find_next_step(&params.name, &plan, &resolved_target_set) {
        println!(
            "Running step {} for target {}",
            next_step.step_number,
            next_step.manifest_dir.display()
        );
        run_single_step(
            next_step.step,
            next_step.manifest_dir,
            next_step.task_name,
            next_step.step_number,
            next_step.target_number,
        )
        .await
    } else {
        println!("All steps for all targets completed successfully.");
        Ok(())
    }
}

/// implementation of the task run single-target subcommand
///
/// # Errors
///
/// fails if the implementation of task run single-target fails
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_target_command(params: RunSingleTargetParameters) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name)?;

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    // Find the first target with at least one incomplete step.
    let Some((target_idx, target)) =
        resolved_target_set
            .targets
            .iter()
            .enumerate()
            .find(|(target_idx, _)| {
                plan.steps.iter().enumerate().any(|(step_idx, step)| {
                    !is_step_completed(&params.name, *target_idx, step_idx.saturating_add(1), step)
                })
            })
    else {
        println!("All steps for all targets completed successfully.");
        return Ok(());
    };

    println!(
        "Found incomplete steps for target {}, running all remaining steps for it.",
        target.manifest_dir.display()
    );

    for (step_idx, step) in plan.steps.iter().enumerate() {
        let step_number = step_idx.saturating_add(1);
        if !is_step_completed(&params.name, target_idx, step_number, step) {
            run_single_step(
                step,
                &target.manifest_dir,
                &params.name,
                step_number,
                target_idx,
            )
            .await?;
        }
    }

    Ok(())
}

/// implementation of the task run all-targets subcommand
///
/// # Errors
///
/// fails if the implementation of task run all-targets fails
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_all_targets_command(params: RunAllTargetsParameters) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name)?;

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    let jobs = params.jobs.unwrap_or(1);
    let plan = Arc::new(plan);

    if params.keep_going {
        let has_errors = Arc::new(AtomicBool::new(false));
        stream::iter(resolved_target_set.targets.into_iter().enumerate())
            .for_each_concurrent(jobs, |(target_idx, target)| {
                let plan = Arc::clone(&plan);
                let params = params.clone();
                let has_errors = Arc::clone(&has_errors);
                async move {
                    for (step_idx, step) in plan.steps.iter().enumerate() {
                        let step_number = step_idx.saturating_add(1);
                        if !is_step_completed(&params.name, target_idx, step_number, step) {
                            println!(
                                "Running step {} for target {}",
                                step_number,
                                target.manifest_dir.display()
                            );
                            if let Err(e) = run_single_step(
                                step,
                                &target.manifest_dir,
                                &params.name,
                                step_number,
                                target_idx,
                            )
                            .await
                            {
                                tracing::error!(
                                    "Error running step {} for target {}: {}",
                                    step_number,
                                    target.manifest_dir.display(),
                                    e
                                );
                                has_errors.store(true, Ordering::SeqCst);
                                break; // Stop processing this target
                            }
                        }
                    }
                }
            })
            .await;

        if has_errors.load(Ordering::SeqCst) {
            return Err(Error::SomeStepsFailed);
        }
    } else {
        stream::iter(resolved_target_set.targets.into_iter().enumerate())
            .map(Ok)
            .try_for_each_concurrent(jobs, |(target_idx, target)| {
                let plan = Arc::clone(&plan);
                let params = params.clone();
                async move {
                    for (step_idx, step) in plan.steps.iter().enumerate() {
                        let step_number = step_idx.saturating_add(1);
                        if !is_step_completed(&params.name, target_idx, step_number, step) {
                            println!(
                                "Running step {} for target {}",
                                step_number,
                                target.manifest_dir.display()
                            );
                            run_single_step(
                                step,
                                &target.manifest_dir,
                                &params.name,
                                step_number,
                                target_idx,
                            )
                            .await?;
                        }
                    }
                    Ok::<(), Error>(())
                }
            })
            .await?;
    }

    Ok(())
}

/// implementation of the task run subcommand
///
/// # Errors
///
/// fails if the implementation of task run fails
#[instrument]
pub async fn task_run_command(params: TaskRunParameters) -> Result<(), Error> {
    match params.command {
        TaskRunCommand::SingleStep(p) => run_single_step_command(p).await,
        TaskRunCommand::SingleTarget(p) => run_single_target_command(p).await,
        TaskRunCommand::AllTargets(p) => run_all_targets_command(p).await,
    }
}

/// implementation of the task subcommand
///
/// # Errors
///
/// fails if the implementation of task fails
#[instrument]
pub async fn task_command(task_parameters: TaskParameters) -> Result<(), Error> {
    match task_parameters.command {
        TaskCommand::Create(params) => {
            task_create_command(params).await?;
        }
        TaskCommand::Remove(params) => {
            let task_dir = named_dir_path(&params.name)?;
            fs_err::remove_dir_all(&task_dir)
                .map_err(|e| Error::CouldNotRemoveTaskDir(task_dir.clone(), e))?;
        }
        TaskCommand::Run(params) => {
            task_run_command(params).await?;
        }
    }
    Ok(())
}
