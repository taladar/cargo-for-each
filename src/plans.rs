//! This module defines the structures and functions for managing plans and their steps.
use std::path::PathBuf;

use tracing::instrument;

use crate::error::Error;
use clap::Parser;

/// represents a single step in a plan
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Step {
    /// the command to execute
    pub command: String,
    /// the arguments for the command
    pub args: Vec<String>,
}

/// represents a plan
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    /// the steps of the plan
    pub steps: Vec<Step>,
}

/// returns the plans dir path
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn dir_path() -> Result<PathBuf, Error> {
    Ok(crate::config_dir_path()?.join("plans"))
}

/// loads a plan from a file
///
/// # Errors
///
/// Returns an error if the plan file cannot be read, parsed, or if the plan is not found.
pub fn load_plan(name: &str) -> Result<Plan, Error> {
    let plan_path = dir_path()?.join(format!("{name}.toml"));
    if plan_path.exists() {
        let file_content =
            fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
        toml::from_str(&file_content).map_err(Error::CouldNotParsePlanFile)
    } else {
        Ok(Plan::default())
    }
}

/// saves a plan to a file
///
/// # Errors
///
/// Returns an error if the plan of the given name already exists,
/// if parent directories cannot be created, if the plan cannot be serialized,
/// or if the plan file cannot be written.
pub fn save_plan(name: &str, plan: &Plan) -> Result<(), Error> {
    let plan_path = dir_path()?.join(format!("{name}.toml"));
    if plan_path.exists() {
        return Err(Error::AlreadyExists(format!("plan {name}")));
    }
    if let Some(plan_dir_path) = plan_path.parent() {
        fs_err::create_dir_all(plan_dir_path).map_err(Error::CouldNotCreatePlanFileParentDirs)?;
    }
    fs_err::write(
        &plan_path,
        toml::to_string(plan).map_err(Error::CouldNotSerializePlanFile)?,
    )
    .map_err(Error::CouldNotWritePlanFile)
}

/// Parameters for creating a new plan
#[derive(Parser, Debug, Clone)]
pub struct CreatePlanParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
}

/// Parameters for adding a step to a plan
#[derive(Parser, Debug, Clone)]
pub struct AddStepParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
    /// The command to execute.
    #[clap(required = true)]
    pub command: String,
    /// The arguments for the command.
    #[clap(last = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parameters for inserting a step into a plan
#[derive(Parser, Debug, Clone)]
pub struct InsertStepParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
    /// the 1-based position to insert the step at (e.g., 1 to insert before the first step, N to insert before the Nth step)
    #[clap(long)]
    pub position: usize,
    /// The command to execute.
    #[clap(required = true)]
    pub command: String,
    /// The arguments for the command.
    #[clap(last = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parameters for removing a step from a plan
#[derive(Parser, Debug, Clone)]
pub struct RemoveStepParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
    /// the position of the step to delete
    #[clap(long)]
    pub position: usize,
}

/// Parameters for deleting a plan
#[derive(Parser, Debug, Clone)]
pub struct DeletePlanParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
}

/// Parameters for listing the steps of a plan
#[derive(Parser, Debug, Clone)]
pub struct ListStepsParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
}

/// The `plan step` subcommand
#[derive(Parser, Debug, Clone)]
pub enum PlanStepCommand {
    /// Add a step to a plan
    Add(AddStepParameters),
    /// Insert a step into a plan
    Insert(InsertStepParameters),
    /// Remove a step from a plan
    Remove(RemoveStepParameters),
    /// List the steps of a plan
    List(ListStepsParameters),
}

/// Parameters for plan step subcommand
#[derive(Parser, Debug, Clone)]
pub struct PlanStepParameters {
    /// the `plan step` subcommand to run
    #[clap(subcommand)]
    pub command: PlanStepCommand,
}

/// The `plan` subcommand
#[derive(Parser, Debug, Clone)]
pub enum PlanCommand {
    /// Create a new plan
    Create(CreatePlanParameters),
    /// Delete a plan
    Delete(DeletePlanParameters),
    /// Manage plan steps
    Step(PlanStepParameters),
    /// List all plans
    List,
}

/// Parameters for plan subcommand
#[derive(Parser, Debug, Clone)]
pub struct PlanParameters {
    /// the `plan` subcommand to run
    #[clap(subcommand)]
    pub command: PlanCommand,
}

/// implementation of the plan subcommand
///
/// # Errors
///
/// fails if the implementation of plan fails
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn plan_command(plan_parameters: PlanParameters) -> Result<(), Error> {
    match plan_parameters.command {
        PlanCommand::Create(params) => {
            let plan = Plan::default();
            save_plan(&params.name, &plan)?;
        }
        PlanCommand::Delete(params) => {
            let plan_path = dir_path()?.join(format!("{}.toml", params.name));
            fs_err::remove_file(plan_path).map_err(Error::CouldNotRemovePlanFile)?;
        }
        PlanCommand::Step(params) => {
            plan_step_command(params).await?;
        }
        PlanCommand::List => {
            let plans_dir = dir_path()?;
            if !plans_dir.exists() {
                return Ok(());
            }
            for entry in fs_err::read_dir(plans_dir).map_err(Error::CouldNotReadTargetSetsDir)? {
                let entry = entry.map_err(Error::CouldNotReadTargetSetsDir)?;
                let path = entry.path();
                if path.is_file()
                    && let Some(extension) = path.extension()
                    && extension == "toml"
                    && let Some(name) = path.file_stem()
                {
                    println!("{}", name.to_string_lossy());
                }
            }
        }
    }
    Ok(())
}

/// implementation of the plan subcommand
///
/// # Errors
///
/// fails if the implementation of plan fails
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn plan_step_command(plan_step_parameters: PlanStepParameters) -> Result<(), Error> {
    match plan_step_parameters.command {
        PlanStepCommand::Add(params) => {
            let mut plan = load_plan(&params.name)?;
            plan.steps.push(Step {
                command: params.command,
                args: params.args,
            });
            save_plan(&params.name, &plan)?;
        }
        PlanStepCommand::Insert(params) => {
            let mut plan = load_plan(&params.name)?;
            if params.position > plan.steps.len().saturating_add(1) || params.position == 0 {
                return Err(Error::PlanStepOutOfBounds(
                    params.position,
                    plan.steps.len(),
                ));
            }
            plan.steps.insert(
                params.position.saturating_sub(1),
                Step {
                    command: params.command,
                    args: params.args,
                },
            );
            save_plan(&params.name, &plan)?;
        }
        PlanStepCommand::Remove(params) => {
            let mut plan = load_plan(&params.name)?;
            if params.position > plan.steps.len() || params.position == 0 {
                return Err(Error::PlanStepOutOfBounds(
                    params.position,
                    plan.steps.len(),
                ));
            }
            plan.steps.remove(params.position.saturating_sub(1));
            save_plan(&params.name, &plan)?;
        }
        PlanStepCommand::List(params) => {
            let plan = load_plan(&params.name)?;
            for (i, step) in plan.steps.iter().enumerate() {
                println!(
                    "{}: {} {}",
                    i.saturating_add(1),
                    step.command,
                    step.args.join(" ")
                );
            }
        }
    }
    Ok(())
}
