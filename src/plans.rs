//! This module defines the structures and functions for managing plans and their steps.
use std::path::PathBuf;

use tracing::instrument;

use crate::error::Error;
use clap::Parser;

/// represents a single step in a plan
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Parser)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// a step that runs a command
    RunCommand {
        /// the command to execute
        command: String,
        /// the arguments for the command
        #[clap(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// a step that requires manual intervention
    ManualStep {
        /// the title of the manual step
        title: String,
        /// the instructions for the manual step
        instructions: String,
    },
}

/// represents a plan
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    /// the steps of the plan
    pub steps: Vec<Step>,
}

impl Plan {
    /// returns the plans dir path
    ///
    /// # Errors
    ///
    /// Returns an error if the config directory path cannot be determined.
    pub fn dir_path(environment: &crate::Environment) -> Result<PathBuf, Error> {
        Ok(crate::config_dir_path(environment)?.join("plans"))
    }

    /// return the plan file path
    ///
    /// # Errors
    ///
    /// Returns an error if the config directory path cannot be determined.
    pub fn file_path(name: &str, environment: &crate::Environment) -> Result<PathBuf, Error> {
        Ok(Self::dir_path(environment)?.join(format!("{name}.toml")))
    }

    /// loads a plan from a file
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration directory cannot be determined, if the plan file cannot be read, or if its content cannot be parsed.
    pub fn load(name: &str, environment: &crate::Environment) -> Result<Self, Error> {
        let plan_path = Self::file_path(name, environment)?;
        if plan_path.exists() {
            let file_content =
                fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
            toml::from_str(&file_content).map_err(Error::CouldNotParsePlanFile)
        } else {
            Ok(Self::default())
        }
    }

    /// saves a plan to a file
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration directory cannot be determined, if a plan with the given name already exists, if parent directories cannot be created, if the plan cannot be serialized, or if the plan file cannot be written.
    pub fn save(&self, name: &str, environment: &crate::Environment) -> Result<(), Error> {
        let plan_path = Self::file_path(name, environment)?;
        if let Some(plan_dir_path) = plan_path.parent() {
            fs_err::create_dir_all(plan_dir_path)
                .map_err(Error::CouldNotCreatePlanFileParentDirs)?;
        }
        fs_err::write(
            &plan_path,
            toml::to_string(self).map_err(Error::CouldNotSerializePlanFile)?,
        )
        .map_err(Error::CouldNotWritePlanFile)
    }

    /// checks if a plan already exists on disk
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration direction cannot be determined
    pub fn exists(name: &str, environment: &crate::Environment) -> Result<bool, Error> {
        let plan_path = Self::file_path(name, environment)?;
        Ok(plan_path.exists())
    }
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
    /// the type of step to add
    #[clap(subcommand)]
    pub step: Step,
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
    /// the type of step to insert
    #[clap(subcommand)]
    pub step: Step,
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

/// The sub-commands of `plan step`
#[derive(Parser, Debug, Clone)]
pub enum PlanStepSubCommand {
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
    pub sub_command: PlanStepSubCommand,
}

/// The `plan` subcommand
#[derive(Parser, Debug, Clone)]
pub enum PlanSubCommand {
    /// List all plans
    List,
    /// Create a new plan
    Create(CreatePlanParameters),
    /// Delete a plan
    Delete(DeletePlanParameters),
    /// Manage plan steps
    Step(PlanStepParameters),
}

/// Parameters for plan subcommand
#[derive(Parser, Debug, Clone)]
pub struct PlanParameters {
    /// the `plan` subcommand to run
    #[clap(subcommand)]
    pub sub_command: PlanSubCommand,
}

/// implementation of the plan subcommand
///
/// # Errors
///
/// This command can fail due to issues with plan creation (e.g., plan already exists, file system errors), plan deletion (e.g., file system errors), plan step management (delegated to `plan_step_command`), or listing plans (e.g., issues reading the plans directory).
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn plan_command(
    plan_parameters: PlanParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match plan_parameters.sub_command {
        PlanSubCommand::List => {
            let plans_dir = Plan::dir_path(&environment)?;
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
        PlanSubCommand::Create(params) => {
            if Plan::exists(&params.name, &environment)? {
                return Err(Error::AlreadyExists(format!("plan {}", params.name)));
            }
            let plan = Plan::default();
            plan.save(&params.name, &environment)?;
        }
        PlanSubCommand::Delete(params) => {
            let plan_path = Plan::dir_path(&environment)?.join(format!("{}.toml", params.name));
            fs_err::remove_file(plan_path).map_err(Error::CouldNotRemovePlanFile)?;
        }
        PlanSubCommand::Step(params) => {
            plan_step_command(params, environment).await?;
        }
    }
    Ok(())
}

/// implementation of the plan step subcommand
///
/// # Errors
///
/// This command can fail due to issues with loading or saving the plan, if a specified command in a 'run command' step is not found, or if the provided step position is out of bounds for 'add', 'insert', or 'remove' operations.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn plan_step_command(
    plan_step_parameters: PlanStepParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match plan_step_parameters.sub_command {
        PlanStepSubCommand::List(params) => {
            let plan = Plan::load(&params.name, &environment)?;
            for (i, step) in plan.steps.iter().enumerate() {
                match step {
                    Step::RunCommand { command, args } => {
                        println!(
                            "{}: RunCommand - {} {}",
                            i.saturating_add(1),
                            command,
                            args.join(" ")
                        );
                    }
                    Step::ManualStep {
                        title,
                        instructions,
                    } => {
                        println!(
                            "{}: ManualStep - Title: \"{}\", Instructions: \"{}\"",
                            i.saturating_add(1),
                            title,
                            instructions
                        );
                    }
                }
            }
        }
        PlanStepSubCommand::Add(params) => {
            let mut plan = Plan::load(&params.name, &environment)?;
            if let Step::RunCommand { command, .. } = &params.step
                && !crate::utils::command_is_executable(command)
            {
                return Err(crate::error::Error::CommandNotFound(command.to_owned()));
            }
            plan.steps.push(params.step);
            plan.save(&params.name, &environment)?;
        }
        PlanStepSubCommand::Insert(params) => {
            let mut plan = Plan::load(&params.name, &environment)?;
            if params.position > plan.steps.len().saturating_add(1) || params.position == 0 {
                return Err(Error::PlanStepOutOfBounds(
                    params.position,
                    plan.steps.len(),
                ));
            }
            if let Step::RunCommand { command, .. } = &params.step
                && !crate::utils::command_is_executable(command)
            {
                return Err(crate::error::Error::CommandNotFound(command.to_owned()));
            }
            plan.steps
                .insert(params.position.saturating_sub(1), params.step);
            plan.save(&params.name, &environment)?;
        }
        PlanStepSubCommand::Remove(params) => {
            let mut plan = Plan::load(&params.name, &environment)?;
            if params.position > plan.steps.len() || params.position == 0 {
                return Err(Error::PlanStepOutOfBounds(
                    params.position,
                    plan.steps.len(),
                ));
            }
            plan.steps.remove(params.position.saturating_sub(1));
            plan.save(&params.name, &environment)?;
        }
    }
    Ok(())
}
