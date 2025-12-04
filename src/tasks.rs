//! This module defines the structures and functions for managing tasks.
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::{self, StreamExt as _};

use tracing::instrument;

use crate::error::Error;
use crate::plans::Step;
use crate::target_sets::load_target_set;

use crate::{Config, Environment};
use clap::Parser;

/// returns the tasks dir path
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn dir_path(environment: &crate::Environment) -> Result<PathBuf, Error> {
    Ok(crate::config_dir_path(environment)?.join("tasks"))
}

/// returns the path to a specific task directory
///
/// # Errors
///
/// Returns an error if the tasks directory path cannot be determined.
pub fn named_dir_path(name: &str, environment: &crate::Environment) -> Result<PathBuf, Error> {
    Ok(dir_path(environment)?.join(name))
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
pub enum TaskRunSubCommand {
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
    pub sub_command: TaskRunSubCommand,
}

/// The `task` subcommand
#[derive(Parser, Debug, Clone)]
pub enum TaskSubCommand {
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
    pub sub_command: TaskSubCommand,
}

/// implementation of the task create subcommand
///
/// # Errors
///
/// This command can fail if the configuration directory cannot be determined, if the specified plan or target set is not found, if the global configuration cannot be loaded, if there are issues resolving the target set (e.g., cargo metadata errors, package not found, path errors), if the task directory cannot be created (e.g., task already exists, file system errors), if plan or target set files cannot be copied, or if the resolved target set cannot be serialized or written.
#[instrument]
pub async fn task_create_command(
    params: CreateTaskParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    // 1. Validate plan and target-set existence.
    let plan_file_path =
        crate::plans::Plan::dir_path(&environment)?.join(format!("{}.toml", params.plan));
    if !plan_file_path.exists() {
        return Err(Error::PlanNotFound(params.plan));
    }

    let target_set_file_path =
        crate::target_sets::dir_path(&environment)?.join(format!("{}.toml", params.target_set));
    if !target_set_file_path.exists() {
        return Err(Error::TargetSetNotFound(params.target_set));
    }

    // 2. Load the Config
    let config = Config::load(&environment)?;

    // 3. Load the TargetSet.
    let target_set = load_target_set(&params.target_set, &environment)?;

    // 4. Resolve the TargetSet.
    let resolved_target_set = crate::target_sets::resolve_target_set(&target_set, &config)?;

    // 5. Create the task directory.
    let task_dir = named_dir_path(&params.name, &environment)?;
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
/// Returns an error if the state directory cannot be determined or created, if a 'run command' step's command is not found or fails to execute, if the command's exit status cannot be written, if the shell's output cannot be flushed or input read during a 'manual step', if the manual step's confirmation cannot be written, or if the manual step is not confirmed by the user.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_step(
    step: &Step,
    manifest_dir: &Path,
    task_name: &str,
    step_number: usize,
    target_number: usize,
    environment: &Environment,
) -> Result<(), Error> {
    let state_dir = environment
        .state_dir
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
            cmd.arg("record");
            if environment.suppress_subprocess_output {
                cmd.arg("--headless");
            }
            cmd.arg("-q").arg("-c").arg(&command_str).arg(&cast_path);
            cmd.current_dir(manifest_dir);

            let status = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?.status;

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
            cmd.arg("record");
            if environment.suppress_subprocess_output {
                cmd.arg("--headless");
            }
            cmd.arg("-q").arg(&cast_path);
            cmd.current_dir(manifest_dir);

            let status = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?.status;

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

use std::collections::HashMap;

use crate::plans::Plan;
use crate::target_sets::ResolvedTargetSet;
use crate::targets::Target;

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
    environment: &Environment,
) -> bool {
    let state_dir = environment
        .state_dir
        .join("cargo-for-each")
        .join("tasks")
        .join(task_name)
        .join(target_number.to_string())
        .join(step_number.to_string());

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

/// Checks if a given target has completed all steps in a plan.
fn is_target_completed(
    task_name: &str,
    target_idx: usize,
    plan: &Plan,
    environment: &Environment,
) -> bool {
    plan.steps.iter().enumerate().all(|(step_idx, step)| {
        is_step_completed(
            task_name,
            target_idx,
            step_idx.saturating_add(1),
            step,
            environment,
        )
    })
}

/// Checks if all dependencies of a given target have completed all steps in a plan.
fn are_target_dependencies_completed(
    task_name: &str,
    target: &Target,
    plan: &Plan,
    target_map: &HashMap<PathBuf, usize>,
    environment: &Environment,
) -> bool {
    target.dependencies.iter().all(|dep_path| {
        if let Some(&dep_target_idx) = target_map.get(dep_path) {
            is_target_completed(task_name, dep_target_idx, plan, environment)
        } else {
            // This case should ideally not happen if the resolved_target_set is consistent.
            // Assuming that if a dependency isn't in the target map, it's not part of the
            // current task, so we don't need to check its status.
            true
        }
    })
}

#[must_use]
/// Determines the first step that has not already been completed successfully
/// in the first target that has such a step and whose dependencies are all completed.
///
/// Returns the values required to call the `run_single_step()` utility function on it.
pub fn find_next_step<'a>(
    task_name: &'a str,
    plan: &'a Plan,
    resolved_target_set: &'a ResolvedTargetSet,
    environment: &Environment,
) -> Option<NextStep<'a>> {
    let target_map: HashMap<PathBuf, usize> = resolved_target_set
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| (t.manifest_dir.clone(), i))
        .collect();

    for (target_idx, target) in resolved_target_set.targets.iter().enumerate() {
        if are_target_dependencies_completed(task_name, target, plan, &target_map, environment) {
            for (step_idx, step) in plan.steps.iter().enumerate() {
                let step_number = step_idx.saturating_add(1);
                if !is_step_completed(task_name, target_idx, step_number, step, environment) {
                    return Some(NextStep {
                        step,
                        manifest_dir: &target.manifest_dir,
                        task_name,
                        step_number,
                        target_number: target_idx,
                    });
                }
            }
        }
    }
    None
}

/// implementation of the task run single-step subcommand
///
/// # Errors
///
/// This command can fail if the task directory cannot be determined, if the plan or resolved target set files cannot be read or parsed, or if an individual step fails during execution.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_step_command(
    params: RunSingleStepParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name, &environment)?;

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    if let Some(next_step) = find_next_step(&params.name, &plan, &resolved_target_set, &environment)
    {
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
            &environment,
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
/// This command can fail if the task directory cannot be determined, if the plan or resolved target set files cannot be read or parsed, or if any of the individual steps for the selected target fail during execution.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_target_command(
    params: RunSingleTargetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name, &environment)?;

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    let target_map: HashMap<PathBuf, usize> = resolved_target_set
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| (t.manifest_dir.clone(), i))
        .collect();

    // Find the first target with at least one incomplete step whose dependencies are met.
    let Some((target_idx, target)) =
        resolved_target_set
            .targets
            .iter()
            .enumerate()
            .find(|(target_idx, target)| {
                !is_target_completed(&params.name, *target_idx, &plan, &environment)
                    && are_target_dependencies_completed(
                        &params.name,
                        target,
                        &plan,
                        &target_map,
                        &environment,
                    )
            })
    else {
        println!(
            "All steps for all targets completed successfully or no targets are ready to be processed."
        );
        return Ok(());
    };

    println!(
        "Found ready target with incomplete steps: {}, running all remaining steps for it.",
        target.manifest_dir.display()
    );

    for (step_idx, step) in plan.steps.iter().enumerate() {
        let step_number = step_idx.saturating_add(1);
        if !is_step_completed(&params.name, target_idx, step_number, step, &environment) {
            run_single_step(
                step,
                &target.manifest_dir,
                &params.name,
                step_number,
                target_idx,
                &environment,
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
/// This command can fail if the task directory cannot be determined, if the plan or resolved target set files cannot be read or parsed, if any individual step fails during execution (unless 'keep_going' is enabled), if a circular dependency is detected between targets, or if 'keep_going' is enabled and some steps failed.
#[instrument]
pub async fn run_all_targets_command(
    params: RunAllTargetsParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name, &environment)?;

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
    let resolved_target_set = Arc::new(resolved_target_set);

    let target_map: Arc<HashMap<PathBuf, usize>> = Arc::new(
        resolved_target_set
            .targets
            .iter()
            .enumerate()
            .map(|(i, t)| (t.manifest_dir.clone(), i))
            .collect(),
    );

    let mut completed_targets = vec![false; resolved_target_set.targets.len()];
    let has_errors = Arc::new(AtomicBool::new(false));

    loop {
        let ready_targets: Vec<_> = resolved_target_set
            .targets
            .iter()
            .enumerate()
            .filter(|(target_idx, target)| {
                completed_targets.get(*target_idx).is_none_or(|&c| !c)
                    && are_target_dependencies_completed(
                        &params.name,
                        target,
                        &plan,
                        &target_map,
                        &environment,
                    )
            })
            .collect();

        if ready_targets.is_empty() {
            break;
        }

        let batch_results = stream::iter(ready_targets)
            .map(|(target_idx, target)| {
                let plan = Arc::clone(&plan);
                let params = params.clone();
                {
                    let environment = environment.clone();
                    async move {
                        for (step_idx, step) in plan.steps.iter().enumerate() {
                            let step_number = step_idx.saturating_add(1);
                            if !is_step_completed(
                                &params.name,
                                target_idx,
                                step_number,
                                step,
                                &environment,
                            ) {
                                run_single_step(
                                    step,
                                    &target.manifest_dir,
                                    &params.name,
                                    step_number,
                                    target_idx,
                                    &environment,
                                )
                                .await?;
                            }
                        }
                        Ok(target_idx)
                    }
                }
            })
            .buffer_unordered(jobs)
            .collect::<Vec<_>>()
            .await;
        for result in batch_results {
            match result {
                Ok(target_idx) => {
                    if let Some(c) = completed_targets.get_mut(target_idx) {
                        *c = true;
                    }
                }
                Err(e) => {
                    if params.keep_going {
                        tracing::error!("A step failed: {}", e);
                        has_errors.store(true, Ordering::SeqCst);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    if !completed_targets.iter().all(|&c| c) {
        return Err(Error::CircularDependency);
    }

    if has_errors.load(Ordering::SeqCst) {
        return Err(Error::SomeStepsFailed);
    }

    Ok(())
}

/// implementation of the task run subcommand
///
/// # Errors
///
/// This command can fail due to errors in its subcommands (run single step, run single target, run all targets).
#[instrument]
pub async fn task_run_command(
    params: TaskRunParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match params.sub_command {
        TaskRunSubCommand::SingleStep(p) => run_single_step_command(p, environment).await,
        TaskRunSubCommand::SingleTarget(p) => run_single_target_command(p, environment).await,
        TaskRunSubCommand::AllTargets(p) => run_all_targets_command(p, environment).await,
    }
}

/// implementation of the task subcommand
///
/// # Errors
///
/// This command can fail due to errors in its subcommands (create, remove, run), such as issues with task creation, removal (e.g., file system errors), or execution.
#[instrument]
pub async fn task_command(
    task_parameters: TaskParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match task_parameters.sub_command {
        TaskSubCommand::Create(params) => {
            task_create_command(params, environment).await?;
        }
        TaskSubCommand::Remove(params) => {
            let task_dir = named_dir_path(&params.name, &environment)?;
            fs_err::remove_dir_all(&task_dir)
                .map_err(|e| Error::CouldNotRemoveTaskDir(task_dir.clone(), e))?;
        }
        TaskSubCommand::Run(params) => {
            task_run_command(params, environment).await?;
        }
    }
    Ok(())
}
