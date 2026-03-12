//! This module defines the structures and functions for managing plans and their steps.
use std::path::PathBuf;

use tracing::instrument;

use crate::error::Error;
use crate::step_position::StepPosition;
use clap::Parser;

/// A single conditional branch in an `IfElseIf` step.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Branch {
    /// The condition that must be true for this branch to execute.
    pub condition: crate::condition::Condition,
    /// The steps to execute when this branch is chosen.
    pub steps: Vec<Step>,
}

/// represents a single step in a plan
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// a step that runs a command
    RunCommand {
        /// the command to execute
        command: String,
        /// the arguments for the command
        args: Vec<String>,
    },
    /// a step that requires manual intervention
    ManualStep {
        /// the title of the manual step
        title: String,
        /// the instructions for the manual step
        instructions: String,
    },
    /// A step that evaluates conditions and executes the first matching branch's steps.
    IfElseIf {
        /// Ordered list of conditional branches (if / elsif).
        branches: Vec<Branch>,
        /// Steps to execute when no branch condition matches (else). Empty means no else.
        #[serde(default)]
        else_steps: Vec<Self>,
    },
}

/// A CLI-only step type that can be constructed via clap. Does not include `IfElseIf`
/// because its `Vec<Branch>` fields are not clap-parseable.
#[derive(Debug, Clone, Parser)]
pub enum CliStep {
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

impl From<CliStep> for Step {
    fn from(cli: CliStep) -> Self {
        match cli {
            CliStep::RunCommand { command, args } => Self::RunCommand { command, args },
            CliStep::ManualStep {
                title,
                instructions,
            } => Self::ManualStep {
                title,
                instructions,
            },
        }
    }
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
    pub step: CliStep,
}

/// Parameters for inserting a step into a plan
#[derive(Parser, Debug, Clone)]
pub struct InsertStepParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
    /// the 1-based position to insert the step at (e.g., 1 to insert before the first step, N to insert before the Nth step)
    #[clap(long)]
    pub position: StepPosition,
    /// the type of step to insert
    #[clap(subcommand)]
    pub step: CliStep,
}

/// Parameters for removing a step from a plan
#[derive(Parser, Debug, Clone)]
pub struct RemoveStepParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
    /// the position of the step to delete
    #[clap(long)]
    pub position: StepPosition,
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
            if !Plan::exists(&params.name, &environment)? {
                return Err(Error::PlanNotFound(params.name));
            }
            let plan = Plan::load(&params.name, &environment)?;
            for (i, step) in plan.steps.iter().enumerate() {
                let pos =
                    StepPosition::from_step_index(i).ok_or(Error::FmtError(std::fmt::Error))?;
                match step {
                    Step::RunCommand { command, args } => {
                        println!("{pos}: RunCommand - {} {}", command, args.join(" "));
                    }
                    Step::ManualStep {
                        title,
                        instructions,
                    } => {
                        println!(
                            "{pos}: ManualStep - Title: \"{title}\", Instructions: \"{instructions}\""
                        );
                    }
                    Step::IfElseIf {
                        branches,
                        else_steps,
                    } => {
                        println!(
                            "{pos}: IfElseIf - {} branch(es){}",
                            branches.len(),
                            if else_steps.is_empty() {
                                ""
                            } else {
                                ", with else"
                            }
                        );
                    }
                }
            }
        }
        PlanStepSubCommand::Add(params) => {
            if !Plan::exists(&params.name, &environment)? {
                return Err(Error::PlanNotFound(params.name));
            }
            let mut plan = Plan::load(&params.name, &environment)?;
            if let CliStep::RunCommand { command, .. } = &params.step
                && !crate::utils::command_is_executable(command, &environment)
            {
                return Err(crate::error::Error::CommandNotFound(command.to_owned()));
            }
            plan.steps.push(params.step.into());
            plan.save(&params.name, &environment)?;
        }
        PlanStepSubCommand::Insert(params) => {
            if !Plan::exists(&params.name, &environment)? {
                return Err(Error::PlanNotFound(params.name));
            }
            let mut plan = Plan::load(&params.name, &environment)?;
            let insert_idx = params.position.to_top_level_index().ok_or_else(|| {
                Error::PlanStepOutOfBounds(params.position.clone(), plan.steps.len())
            })?;
            if insert_idx > plan.steps.len() {
                return Err(Error::PlanStepOutOfBounds(
                    params.position,
                    plan.steps.len(),
                ));
            }
            if let CliStep::RunCommand { command, .. } = &params.step
                && !crate::utils::command_is_executable(command, &environment)
            {
                return Err(crate::error::Error::CommandNotFound(command.to_owned()));
            }
            plan.steps.insert(insert_idx, params.step.into());
            plan.save(&params.name, &environment)?;
        }
        PlanStepSubCommand::Remove(params) => {
            if !Plan::exists(&params.name, &environment)? {
                return Err(Error::PlanNotFound(params.name));
            }
            let mut plan = Plan::load(&params.name, &environment)?;
            let remove_idx = params.position.to_top_level_index().ok_or_else(|| {
                Error::PlanStepOutOfBounds(params.position.clone(), plan.steps.len())
            })?;
            if remove_idx >= plan.steps.len() {
                return Err(Error::PlanStepOutOfBounds(
                    params.position,
                    plan.steps.len(),
                ));
            }
            plan.steps.remove(remove_idx);
            plan.save(&params.name, &environment)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::condition::Condition;
    use tempfile::tempdir;

    /// All four `plan step` sub-commands must return `PlanNotFound` when the
    /// named plan does not exist on disk, rather than silently creating it.
    ///
    /// Regression tests for Bug 6: previously `Plan::load` returned an empty
    /// default plan when the file was absent, so `plan step add` would create a
    /// brand-new plan file instead of reporting an error.

    #[tokio::test]
    async fn test_plan_step_list_missing_plan_returns_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let environment = crate::Environment::mock(&temp)?;
        let result = plan_step_command(
            PlanStepParameters {
                sub_command: PlanStepSubCommand::List(ListStepsParameters {
                    name: "nonexistent-plan".to_string(),
                }),
            },
            environment,
        )
        .await;
        assert!(
            matches!(result, Err(crate::error::Error::PlanNotFound(_))),
            "expected PlanNotFound, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_plan_step_add_missing_plan_returns_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let environment = crate::Environment::mock(&temp)?;
        let result = plan_step_command(
            PlanStepParameters {
                sub_command: PlanStepSubCommand::Add(AddStepParameters {
                    name: "nonexistent-plan".to_string(),
                    step: CliStep::ManualStep {
                        title: "t".to_string(),
                        instructions: "i".to_string(),
                    },
                }),
            },
            environment,
        )
        .await;
        assert!(
            matches!(result, Err(crate::error::Error::PlanNotFound(_))),
            "expected PlanNotFound, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_plan_step_insert_missing_plan_returns_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let environment = crate::Environment::mock(&temp)?;
        let result = plan_step_command(
            PlanStepParameters {
                sub_command: PlanStepSubCommand::Insert(InsertStepParameters {
                    name: "nonexistent-plan".to_string(),
                    position: StepPosition::from_one_based(1)
                        .ok_or("step position 1 is always valid")?,
                    step: CliStep::ManualStep {
                        title: "t".to_string(),
                        instructions: "i".to_string(),
                    },
                }),
            },
            environment,
        )
        .await;
        assert!(
            matches!(result, Err(crate::error::Error::PlanNotFound(_))),
            "expected PlanNotFound, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_plan_step_remove_missing_plan_returns_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let environment = crate::Environment::mock(&temp)?;
        let result = plan_step_command(
            PlanStepParameters {
                sub_command: PlanStepSubCommand::Remove(RemoveStepParameters {
                    name: "nonexistent-plan".to_string(),
                    position: StepPosition::from_one_based(1)
                        .ok_or("step position 1 is always valid")?,
                }),
            },
            environment,
        )
        .await;
        assert!(
            matches!(result, Err(crate::error::Error::PlanNotFound(_))),
            "expected PlanNotFound, got {result:?}"
        );
        Ok(())
    }

    /// TOML round-trip for a `Branch`.
    #[test]
    fn test_branch_toml_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let branch = Branch {
            condition: Condition::IsBinaryCrate {},
            steps: vec![Step::RunCommand {
                command: "cargo".to_string(),
                args: vec!["build".to_string()],
            }],
        };
        let serialized = toml::to_string(&branch)?;
        let deserialized: Branch = toml::from_str(&serialized)?;
        assert!(matches!(
            deserialized.condition,
            Condition::IsBinaryCrate {}
        ));
        assert_eq!(deserialized.steps.len(), 1);
        Ok(())
    }

    /// TOML round-trip for a plan containing an `IfElseIf` step.
    #[test]
    fn test_plan_with_if_else_if_toml_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let plan = Plan {
            steps: vec![
                Step::RunCommand {
                    command: "cargo".to_string(),
                    args: vec!["check".to_string()],
                },
                Step::IfElseIf {
                    branches: vec![Branch {
                        condition: Condition::IsBinaryCrate {},
                        steps: vec![Step::RunCommand {
                            command: "cargo".to_string(),
                            args: vec!["build".to_string()],
                        }],
                    }],
                    else_steps: vec![Step::ManualStep {
                        title: "Manual".to_string(),
                        instructions: "Do it".to_string(),
                    }],
                },
            ],
        };
        let serialized = toml::to_string(&plan)?;
        let deserialized: Plan = toml::from_str(&serialized)?;
        assert_eq!(deserialized.steps.len(), 2);
        assert!(matches!(
            deserialized.steps.first(),
            Some(Step::RunCommand { .. })
        ));
        match deserialized.steps.get(1) {
            Some(Step::IfElseIf {
                branches,
                else_steps,
            }) => {
                assert_eq!(branches.len(), 1);
                assert_eq!(else_steps.len(), 1);
            }
            other => return Err(format!("expected IfElseIf at index 1, got {other:?}").into()),
        }
        Ok(())
    }

    /// `plan step add` on an existing plan must NOT return PlanNotFound.
    #[tokio::test]
    async fn test_plan_step_add_existing_plan_succeeds() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let environment = crate::Environment::mock(&temp)?;
        // Create the plan first.
        Plan::default().save("my-plan", &environment)?;
        let result = plan_step_command(
            PlanStepParameters {
                sub_command: PlanStepSubCommand::Add(AddStepParameters {
                    name: "my-plan".to_string(),
                    step: CliStep::ManualStep {
                        title: "step".to_string(),
                        instructions: "do something".to_string(),
                    },
                }),
            },
            environment,
        )
        .await;
        assert!(
            result.is_ok(),
            "adding to an existing plan should succeed: {result:?}"
        );
        Ok(())
    }
}
