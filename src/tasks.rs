//! This module defines the structures and functions for managing tasks.
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

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

/// returns the path to a specific task's state directory
///
/// # Errors
///
/// Returns an error if the tasks directory path cannot be determined.
pub fn state_dir_for_task(name: &str, environment: &crate::Environment) -> Result<PathBuf, Error> {
    Ok(environment
        .state_dir
        .join("cargo-for-each")
        .join("tasks")
        .join(name))
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

/// Parameters for rewinding a single step of a task
#[derive(Parser, Debug, Clone)]
pub struct RewindSingleStepParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
}

/// Parameters for rewinding a task on a single target
#[derive(Parser, Debug, Clone)]
pub struct RewindSingleTargetParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
}

/// Parameters for rewinding a task on all targets
#[derive(Parser, Debug, Clone)]
pub struct RewindAllTargetsParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
}

/// The `task rewind` subcommand
#[derive(Parser, Debug, Clone)]
pub enum TaskRewindSubCommand {
    /// Rewind a single step of a task
    SingleStep(RewindSingleStepParameters),
    /// Rewind a task on a single target
    SingleTarget(RewindSingleTargetParameters),
    /// Rewind a task on all targets
    AllTargets(RewindAllTargetsParameters),
}

/// Parameters for the `task rewind` subcommand
#[derive(Parser, Debug, Clone)]
pub struct TaskRewindParameters {
    /// the `task rewind` subcommand to run
    #[clap(subcommand)]
    pub sub_command: TaskRewindSubCommand,
}

/// The `task` subcommand
#[derive(Parser, Debug, Clone)]
pub enum TaskSubCommand {
    /// List all tasks
    List,
    /// Create a new task
    Create(CreateTaskParameters),
    /// Remove a task
    Remove(RemoveTaskParameters),
    /// Describe a task
    Describe(DescribeTaskParameters),
    /// Run a task
    Run(TaskRunParameters),
    /// Rewind a task
    Rewind(TaskRewindParameters),
}

/// Parameters for removing a task
#[derive(Parser, Debug, Clone)]
pub struct RemoveTaskParameters {
    /// the name of the task
    #[clap(long)]
    pub name: String,
}

/// Parameters for describing a task
#[derive(Parser, Debug, Clone)]
pub struct DescribeTaskParameters {
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
            if !crate::utils::command_is_executable(command, environment) {
                return Err(crate::error::Error::CommandNotFound(command.to_owned()));
            }

            let command_str = format!(
                "{} {}",
                command,
                args.iter()
                    .map(|arg| format!("\"{}\"", arg.replace('"', "\\\"")))
                    .collect::<Vec<String>>()
                    .join(" ")
            );

            // Write a wrapper script so the real exit code of the inner command
            // is captured independently of asciinema's exit code.
            // asciinema v3 in --headless mode always exits with code 0,
            // so we cannot rely on asciinema's own exit code.
            let wrapper_path = state_dir.join("run_wrapper.sh");
            let exit_status_path = state_dir.join("exit_status");
            let script = format!(
                "#!/bin/sh\n{command_str}\nrc=$?\nprintf '%d' \"$rc\" > \"$CARGO_FOR_EACH_EXIT_STATUS_PATH\"\nexit \"$rc\"\n"
            );
            fs_err::write(&wrapper_path, &script)
                .map_err(|e| Error::CouldNotWriteStateFile(wrapper_path.clone(), e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let perms = std::fs::Permissions::from_mode(0o755);
                fs_err::set_permissions(&wrapper_path, perms).map_err(Error::IoError)?;
            }

            let mut cmd = Command::new("asciinema");
            cmd.arg("record");
            if environment.suppress_subprocess_output {
                cmd.arg("--headless");
            }
            cmd.arg("-q")
                .arg("-c")
                .arg(wrapper_path.to_string_lossy().as_ref())
                .arg(&cast_path);
            cmd.env("CARGO_FOR_EACH_EXIT_STATUS_PATH", &exit_status_path);
            cmd.current_dir(manifest_dir);

            match crate::utils::execute_command(&mut cmd, environment, manifest_dir) {
                Err(e) => {
                    // asciinema failed to launch; write an empty exit_status so
                    // get_step_status sees Failed rather than NotRun.
                    fs_err::write(&exit_status_path, "")
                        .map_err(|we| Error::CouldNotWriteStateFile(exit_status_path, we))?;
                    return Err(e);
                }
                Ok(_output) => {
                    // Read the real exit code written by the wrapper script.
                    // If the file is missing or unparsable (wrapper crashed),
                    // treat as failure (-1).
                    let exit_code: i32 = fs_err::read_to_string(&exit_status_path)
                        .ok()
                        .as_deref()
                        .map(str::trim)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(-1);

                    // Ensure exit_status is always written so get_step_status
                    // never returns NotRun for a step that was executed.
                    if !exit_status_path.exists() {
                        fs_err::write(&exit_status_path, exit_code.to_string())
                            .map_err(|e| Error::CouldNotWriteStateFile(exit_status_path, e))?;
                    }

                    if exit_code != 0 {
                        return Err(Error::CommandFailed(
                            command_str,
                            manifest_dir.to_path_buf(),
                            exit_code,
                        ));
                    }
                }
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

/// The status of a step.
#[derive(Debug, PartialEq, Eq)]
pub enum StepStatus {
    /// The step has been completed successfully.
    Completed,
    /// The step failed.
    Failed,
    /// The step has not yet been run.
    NotRun,
}

/// Determines the status of a given step for a specific target in a task.
fn get_step_status(
    task_name: &str,
    target_number: usize,
    step_number: usize,
    step: &Step,
    environment: &Environment,
) -> StepStatus {
    let state_dir = environment
        .state_dir
        .join("cargo-for-each")
        .join("tasks")
        .join(task_name)
        .join(target_number.to_string())
        .join(step_number.to_string());

    if !state_dir.exists() {
        return StepStatus::NotRun;
    }

    match step {
        Step::RunCommand { .. } => {
            let exit_status_path = state_dir.join("exit_status");
            if let Ok(content) = fs_err::read_to_string(exit_status_path) {
                if content.trim() == "0" {
                    StepStatus::Completed
                } else {
                    StepStatus::Failed
                }
            } else {
                StepStatus::NotRun
            }
        }
        Step::ManualStep { .. } => {
            let manual_step_confirmed_path = state_dir.join("manual_step_confirmed");
            if let Ok(content) = fs_err::read_to_string(manual_step_confirmed_path) {
                if content.trim() == "y" {
                    StepStatus::Completed
                } else {
                    StepStatus::Failed
                }
            } else {
                StepStatus::NotRun
            }
        }
    }
}

/// Checks if a given step for a specific target in a task has been completed.
fn is_step_completed(
    task_name: &str,
    target_number: usize,
    step_number: usize,
    step: &Step,
    environment: &Environment,
) -> bool {
    matches!(
        get_step_status(task_name, target_number, step_number, step, environment),
        StepStatus::Completed
    )
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
    let mut failed_targets = vec![false; resolved_target_set.targets.len()];
    let mut has_errors = false;

    loop {
        let ready_targets: Vec<_> = resolved_target_set
            .targets
            .iter()
            .enumerate()
            .filter(|(target_idx, target)| {
                completed_targets.get(*target_idx).is_none_or(|&c| !c)
                    && !failed_targets.get(*target_idx).is_some_and(|&f| f)
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
                            ) && let Err(e) = run_single_step(
                                step,
                                &target.manifest_dir,
                                &params.name,
                                step_number,
                                target_idx,
                                &environment,
                            )
                            .await
                            {
                                return (target_idx, Err(e));
                            }
                        }
                        (target_idx, Ok(()))
                    }
                }
            })
            .buffer_unordered(jobs)
            .collect::<Vec<(usize, Result<(), Error>)>>()
            .await;
        for (target_idx, result) in batch_results {
            match result {
                Ok(()) => {
                    if let Some(c) = completed_targets.get_mut(target_idx) {
                        *c = true;
                    }
                }
                Err(e) => {
                    if params.keep_going {
                        tracing::error!("A step failed: {}", e);
                        if let Some(f) = failed_targets.get_mut(target_idx) {
                            *f = true;
                        }
                        has_errors = true;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    if has_errors {
        return Err(Error::SomeStepsFailed);
    }

    if !completed_targets.iter().all(|&c| c) {
        return Err(Error::CircularDependency);
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

/// implementation of the task rewind all-targets subcommand
///
/// # Errors
///
/// This command can fail if the task's state directory cannot be determined, or if the directory cannot be removed.
#[instrument]
pub async fn rewind_all_targets_command(
    params: RewindAllTargetsParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let state_dir = state_dir_for_task(&params.name, &environment)?;
    if state_dir.exists() {
        fs_err::remove_dir_all(&state_dir)
            .map_err(|e| Error::CouldNotRemoveTaskStateDir(state_dir.clone(), e))?;
        tracing::info!("Removed state for task '{}' for all targets.", params.name);
    } else {
        tracing::info!(
            "No state found for task '{}' for all targets, nothing to rewind.",
            params.name
        );
    }
    Ok(())
}

/// implementation of the task rewind single-target subcommand
///
/// # Errors
///
/// This command can fail if the task directory cannot be determined, if the resolved target set file cannot be read or parsed, if the state directory for a target cannot be removed.
#[instrument]
pub async fn rewind_single_target_command(
    params: RewindSingleTargetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name, &environment)?;
    if !task_dir.exists() {
        return Err(Error::TaskNotFound(params.name));
    }

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    // Find the last completed target to rewind.
    // We iterate in reverse to find the "most recently completed" target.
    if let Some((target_idx, target)) = resolved_target_set
        .targets
        .iter()
        .enumerate()
        .rev()
        .find(|(target_idx, _)| is_target_completed(&params.name, *target_idx, &plan, &environment))
    {
        let target_state_dir = environment
            .state_dir
            .join("cargo-for-each")
            .join("tasks")
            .join(&params.name)
            .join(target_idx.to_string());

        if target_state_dir.exists() {
            fs_err::remove_dir_all(&target_state_dir)
                .map_err(|e| Error::CouldNotRemoveTaskStateDir(target_state_dir.clone(), e))?;
            tracing::info!(
                "Removed state for target '{}' (index {}) in task '{}'.",
                target.manifest_dir.display(),
                target_idx,
                params.name
            );
        } else {
            tracing::info!(
                "No state found for target '{}' (index {}) in task '{}', nothing to rewind.",
                target.manifest_dir.display(),
                target_idx,
                params.name
            );
        }
    } else {
        tracing::info!(
            "No completed targets found for task '{}', nothing to rewind.",
            params.name
        );
    }

    Ok(())
}

/// implementation of the task rewind single-step subcommand
///
/// # Errors
///
/// This command can fail if the task directory cannot be determined, if the plan or resolved target set files cannot be read or parsed, or if the state directory for a step cannot be removed.
#[instrument]
pub async fn rewind_single_step_command(
    params: RewindSingleStepParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name, &environment)?;
    if !task_dir.exists() {
        return Err(Error::TaskNotFound(params.name));
    }

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    // We need to find the "last" completed step. This is the last step within the last target
    // that has any completed steps. Or, more precisely, the step with the highest step_number
    // within the target with the highest target_number, that is completed.
    let mut last_completed_step_info: Option<(usize, usize)> = None; // (target_idx, step_number)

    for (target_idx, _target) in resolved_target_set.targets.iter().enumerate().rev() {
        for (step_idx, _step) in plan.steps.iter().enumerate().rev() {
            let step_number = step_idx.saturating_add(1);
            if is_step_completed(&params.name, target_idx, step_number, _step, &environment) {
                last_completed_step_info = Some((target_idx, step_number));
                break; // Found the last completed step for this target, move to next target
            }
        }
        if last_completed_step_info.is_some() {
            break; // Found the last completed step overall, stop searching
        }
    }

    if let Some((target_idx, step_number)) = last_completed_step_info {
        let step_state_dir = environment
            .state_dir
            .join("cargo-for-each")
            .join("tasks")
            .join(&params.name)
            .join(target_idx.to_string())
            .join(step_number.to_string());

        if step_state_dir.exists() {
            fs_err::remove_dir_all(&step_state_dir)
                .map_err(|e| Error::CouldNotRemoveTaskStateDir(step_state_dir.clone(), e))?;
            tracing::info!(
                "Removed state for step {} of target index {} in task '{}'.",
                step_number,
                target_idx,
                params.name
            );
        } else {
            tracing::info!(
                "No state found for step {} of target index {} in task '{}', nothing to rewind.",
                step_number,
                target_idx,
                params.name
            );
        }
    } else {
        tracing::info!(
            "No completed steps found for task '{}', nothing to rewind.",
            params.name
        );
    }

    Ok(())
}

/// implementation of the task rewind subcommand
///
/// # Errors
///
/// This command can fail due to errors in its subcommands (rewind single step, rewind single target, rewind all targets).
#[instrument]
pub async fn task_rewind_command(
    params: TaskRewindParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match params.sub_command {
        TaskRewindSubCommand::SingleStep(p) => rewind_single_step_command(p, environment).await,
        TaskRewindSubCommand::SingleTarget(p) => rewind_single_target_command(p, environment).await,
        TaskRewindSubCommand::AllTargets(p) => rewind_all_targets_command(p, environment).await,
    }
}

/// implementation of the task describe subcommand
///
/// # Errors
///
/// This command can fail if the task directory cannot be determined, if the plan or resolved target set files cannot be read or parsed.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn task_describe_command(
    params: DescribeTaskParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name, &environment)?;
    if !task_dir.exists() {
        return Err(Error::TaskNotFound(params.name));
    }

    let plan_path = task_dir.join("plan.toml");
    let plan_content = fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
    let plan: Plan = toml::from_str(&plan_content).map_err(Error::CouldNotParsePlanFile)?;

    let resolved_target_set_path = task_dir.join("resolved-target-set.toml");
    let resolved_target_set_content = fs_err::read_to_string(&resolved_target_set_path)
        .map_err(|e| Error::CouldNotReadResolvedTargetSet(resolved_target_set_path.clone(), e))?;
    let resolved_target_set: ResolvedTargetSet = toml::from_str(&resolved_target_set_content)
        .map_err(|e| Error::CouldNotParseResolvedTargetSet(resolved_target_set_path.clone(), e))?;

    println!("Task: {}", params.name);
    println!("Targets:");

    for (target_idx, target) in resolved_target_set.targets.iter().enumerate() {
        println!("  - Target: {}", target.manifest_dir.display());
        println!("    Path: {}", target.manifest_dir.display());
        println!("    Steps:");
        for (step_idx, step) in plan.steps.iter().enumerate() {
            let step_number = step_idx.saturating_add(1);
            let status = get_step_status(&params.name, target_idx, step_number, step, &environment);
            let status_icon = match status {
                StepStatus::Completed => "\u{2705}", // Green checkmark
                StepStatus::Failed => "\u{274C}",    // Red 'X'
                StepStatus::NotRun => "\u{2B1C}",    // White large square (empty checkbox)
            };
            println!("      {step_number}. {status_icon} {step:?}");
        }
    }

    Ok(())
}

/// implementation of the task list subcommand
///
/// # Errors
///
/// This command can fail if the tasks directory cannot be determined or read.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn task_list_command(environment: crate::Environment) -> Result<(), Error> {
    let tasks_dir = dir_path(&environment)?;

    if !tasks_dir.exists() {
        println!("No tasks found.");
        return Ok(());
    }

    println!("Existing tasks:");
    for entry in fs_err::read_dir(&tasks_dir)
        .map_err(|e| Error::CouldNotReadTasksDir(tasks_dir.clone(), e))?
    {
        let entry = entry.map_err(|e| Error::CouldNotReadTasksDir(tasks_dir.clone(), e))?;
        let path = entry.path();
        if path.is_dir()
            && let Some(task_name) = path.file_name().and_then(|s| s.to_str())
        {
            println!("- {task_name}");
        }
    }
    Ok(())
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
        TaskSubCommand::List => {
            task_list_command(environment).await?;
        }
        TaskSubCommand::Describe(params) => {
            task_describe_command(params, environment).await?;
        }
        TaskSubCommand::Rewind(params) => {
            task_rewind_command(params, environment).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::{StepStatus, find_next_step, get_step_status, run_single_step};
    use crate::Environment;
    use crate::error::Error;
    use crate::plans::{Plan, Step};
    use crate::target_sets::ResolvedTargetSet;
    use crate::targets::Target;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Build a minimal test environment pointing into `temp_dir`.
    fn make_environment(temp_dir: &tempfile::TempDir) -> Environment {
        Environment {
            config_dir: temp_dir.path().join("config"),
            state_dir: temp_dir.path().join("state"),
            paths: vec![],
            suppress_subprocess_output: true,
        }
    }

    /// Build a test environment that includes the real system PATH so that
    /// commands like `true` and `false` can be found by `run_single_step`.
    fn make_environment_with_system_paths(temp_dir: &tempfile::TempDir) -> Environment {
        let system_paths: Vec<PathBuf> = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .map(PathBuf::from)
            .collect();
        Environment {
            config_dir: temp_dir.path().join("config"),
            state_dir: temp_dir.path().join("state"),
            paths: system_paths,
            suppress_subprocess_output: true,
        }
    }

    /// Create the step state directory for `(task, target_idx, step_number)` and return its path.
    fn make_state_dir(
        env: &Environment,
        task_name: &str,
        target_idx: usize,
        step_number: usize,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let dir = env
            .state_dir
            .join("cargo-for-each")
            .join("tasks")
            .join(task_name)
            .join(target_idx.to_string())
            .join(step_number.to_string());
        fs_err::create_dir_all(&dir)?;
        Ok(dir)
    }

    // ── get_step_status: RunCommand ────────────────────────────────────────────

    /// No state directory → NotRun.
    #[test]
    fn test_get_step_status_run_command_not_run_no_dir() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let step = Step::RunCommand {
            command: "echo".to_string(),
            args: vec![],
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::NotRun
        );
        Ok(())
    }

    /// State directory exists but exit_status file is absent → NotRun.
    ///
    /// This is the pre-condition that Bug 2's fix prevents from occurring at
    /// runtime: after the fix, `run_single_step` always writes exit_status
    /// before returning, so the file will never be absent once the dir exists.
    #[test]
    fn test_get_step_status_run_command_not_run_no_file() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        make_state_dir(&env, "task", 0, 1)?; // dir exists, no file
        let step = Step::RunCommand {
            command: "echo".to_string(),
            args: vec![],
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::NotRun
        );
        Ok(())
    }

    /// exit_status = "0" → Completed.
    #[test]
    fn test_get_step_status_run_command_completed() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("exit_status"), "0")?;
        let step = Step::RunCommand {
            command: "echo".to_string(),
            args: vec![],
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::Completed
        );
        Ok(())
    }

    /// exit_status = "1" (command exited non-zero) → Failed.
    #[test]
    fn test_get_step_status_run_command_failed_nonzero() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("exit_status"), "1")?;
        let step = Step::RunCommand {
            command: "echo".to_string(),
            args: vec![],
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::Failed
        );
        Ok(())
    }

    /// exit_status = "" (written by Bug 2 fix when execute_command itself errors) → Failed.
    ///
    /// Regression test for Bug 2: an empty exit_status file (written when the
    /// OS-level command launch fails) must be read as Failed, not as NotRun.
    #[test]
    fn test_get_step_status_run_command_failed_empty_means_failed_not_notrun() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("exit_status"), "")?;
        let step = Step::RunCommand {
            command: "echo".to_string(),
            args: vec![],
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::Failed
        );
        Ok(())
    }

    // ── get_step_status: ManualStep ───────────────────────────────────────────

    /// No state directory → NotRun.
    #[test]
    fn test_get_step_status_manual_step_not_run() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let step = Step::ManualStep {
            title: "t".to_string(),
            instructions: "i".to_string(),
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::NotRun
        );
        Ok(())
    }

    /// manual_step_confirmed = "y" → Completed.
    #[test]
    fn test_get_step_status_manual_step_completed() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("manual_step_confirmed"), "y")?;
        let step = Step::ManualStep {
            title: "t".to_string(),
            instructions: "i".to_string(),
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::Completed
        );
        Ok(())
    }

    /// manual_step_confirmed = "n" → Failed.
    #[test]
    fn test_get_step_status_manual_step_failed() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("manual_step_confirmed"), "n")?;
        let step = Step::ManualStep {
            title: "t".to_string(),
            instructions: "i".to_string(),
        };
        assert_eq!(
            get_step_status("task", 0, 1, &step, &env),
            StepStatus::Failed
        );
        Ok(())
    }

    // ── find_next_step ────────────────────────────────────────────────────────

    /// All steps completed → None returned.
    #[test]
    fn test_find_next_step_returns_none_when_all_completed() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("exit_status"), "0")?;

        let plan = Plan {
            steps: vec![Step::RunCommand {
                command: "echo".to_string(),
                args: vec![],
            }],
        };
        let resolved = ResolvedTargetSet {
            targets: vec![Target {
                manifest_dir: PathBuf::from("/tmp"),
                dependencies: vec![],
            }],
        };
        assert!(find_next_step("task", &plan, &resolved, &env).is_none());
        Ok(())
    }

    /// A failed step (exit_status != "0") is NOT skipped by find_next_step —
    /// it is returned as the next step so it can be retried.
    #[test]
    fn test_find_next_step_returns_failed_step_for_retry() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("exit_status"), "1")?;

        let plan = Plan {
            steps: vec![Step::RunCommand {
                command: "echo".to_string(),
                args: vec![],
            }],
        };
        let resolved = ResolvedTargetSet {
            targets: vec![Target {
                manifest_dir: PathBuf::from("/tmp"),
                dependencies: vec![],
            }],
        };
        let next = find_next_step("task", &plan, &resolved, &env);
        assert!(
            next.is_some(),
            "Failed step should be returned for retry, not skipped"
        );
        let next = next.ok_or("expected Some next step")?;
        assert_eq!(next.step_number, 1);
        Ok(())
    }

    /// No state at all → first step is returned.
    #[test]
    fn test_find_next_step_returns_first_step_when_nothing_run() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);

        let plan = Plan {
            steps: vec![
                Step::RunCommand {
                    command: "echo".to_string(),
                    args: vec!["a".to_string()],
                },
                Step::RunCommand {
                    command: "echo".to_string(),
                    args: vec!["b".to_string()],
                },
            ],
        };
        let resolved = ResolvedTargetSet {
            targets: vec![Target {
                manifest_dir: PathBuf::from("/tmp"),
                dependencies: vec![],
            }],
        };
        let next = find_next_step("task", &plan, &resolved, &env);
        assert!(next.is_some());
        let next = next.ok_or("expected Some next step")?;
        assert_eq!(next.step_number, 1, "Should return step 1 first");
        Ok(())
    }

    /// Step 1 completed, step 2 not yet run → step 2 is returned.
    #[test]
    fn test_find_next_step_skips_completed_steps() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let dir = make_state_dir(&env, "task", 0, 1)?;
        fs_err::write(dir.join("exit_status"), "0")?; // step 1 done

        let plan = Plan {
            steps: vec![
                Step::RunCommand {
                    command: "echo".to_string(),
                    args: vec!["a".to_string()],
                },
                Step::RunCommand {
                    command: "echo".to_string(),
                    args: vec!["b".to_string()],
                },
            ],
        };
        let resolved = ResolvedTargetSet {
            targets: vec![Target {
                manifest_dir: PathBuf::from("/tmp"),
                dependencies: vec![],
            }],
        };
        let next = find_next_step("task", &plan, &resolved, &env);
        assert!(next.is_some());
        let next = next.ok_or("expected Some next step")?;
        assert_eq!(next.step_number, 2, "Step 1 is done; step 2 should be next");
        Ok(())
    }

    // ── run_single_step: wrapper script captures real exit code ───────────────

    /// A successful command (exit code 0) writes "0" to exit_status.
    ///
    /// Regression test for Bug 7: previously asciinema in --headless mode
    /// always exited with code 0, so run_single_step would read asciinema's
    /// exit code instead of the inner command's real exit code.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_single_step_success_writes_zero_exit_status() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment_with_system_paths(&temp);
        let manifest_dir = temp.path();
        let step = Step::RunCommand {
            command: "true".to_string(),
            args: vec![],
        };
        run_single_step(&step, manifest_dir, "test-task", 1, 0, &env).await?;

        let exit_status_path = env
            .state_dir
            .join("cargo-for-each")
            .join("tasks")
            .join("test-task")
            .join("0")
            .join("1")
            .join("exit_status");
        let content = fs_err::read_to_string(&exit_status_path)?;
        assert_eq!(content.trim(), "0");
        Ok(())
    }

    /// A failing command (non-zero exit code) writes a non-zero code to
    /// exit_status and causes run_single_step to return CommandFailed.
    ///
    /// Regression test for Bug 7: without the wrapper script fix, asciinema
    /// in --headless mode would always exit 0 and run_single_step would
    /// incorrectly write "0" to exit_status and return Ok(()).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_single_step_failure_writes_nonzero_exit_status() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment_with_system_paths(&temp);
        let manifest_dir = temp.path();
        let step = Step::RunCommand {
            command: "false".to_string(),
            args: vec![],
        };
        let result = run_single_step(&step, manifest_dir, "test-task", 1, 0, &env).await;

        assert!(result.is_err(), "failing command should return Err");
        assert!(
            matches!(result, Err(Error::CommandFailed(..))),
            "expected CommandFailed error"
        );

        let exit_status_path = env
            .state_dir
            .join("cargo-for-each")
            .join("tasks")
            .join("test-task")
            .join("0")
            .join("1")
            .join("exit_status");
        let content = fs_err::read_to_string(&exit_status_path)?;
        let exit_code: i32 = content.trim().parse()?;
        assert!(exit_code != 0, "exit_status must contain a non-zero code");
        Ok(())
    }
}
