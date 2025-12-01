//! This module defines the structures and functions for managing tasks.
use std::path::PathBuf;

use tracing::instrument;

use crate::error::Error;
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

/// The `task` subcommand
#[derive(Parser, Debug, Clone)]
pub enum TaskCommand {
    /// Create a new task
    Create(CreateTaskParameters),
    /// Remove a task
    Remove(RemoveTaskParameters),
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
    }
    Ok(())
}
