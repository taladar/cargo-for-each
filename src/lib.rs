//! `cargo-for-each` is a tool to run commands on multiple cargo projects.
//!
//! This library provides the core logic for managing workspaces, crates,
//! target sets, plans, and tasks for the `cargo-for-each` CLI.

/// Handles application-specific errors.
pub mod error;
/// Implements functionality for managing plans and their steps.
pub mod plans;
/// Implements functionality for managing target sets.
pub mod target_sets;
/// Defines target-related structures and resolution logic.
pub mod targets;
/// Implements functionality for managing tasks.
pub mod tasks;
/// Implements utility functions.
pub mod utils;

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// which subcommand to call
#[derive(clap::Parser, Debug)]
pub enum Command {
    /// Manage workspaces and crates (add, remove, list, refresh).
    Target(crate::targets::TargetParameters),
    /// create a new target set
    TargetSet(crate::target_sets::TargetSetParameters),
    /// manage plans
    Plan(crate::plans::PlanParameters),
    /// manage tasks
    Task(crate::tasks::TaskParameters),

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
pub struct Options {
    /// which subcommand to use
    #[clap(subcommand)]
    command: Command,
}

/// stores the information we get from environment variables
/// so we can easily mock them for testing
#[derive(Debug, Clone)]
pub struct Environment {
    /// user config dir (XDG\_CONFIG\_DIR)
    pub config_dir: std::path::PathBuf,
    /// user state dir (XDG\_STATE\_DIR)
    pub state_dir: std::path::PathBuf,
    /// paths from PATH
    pub paths: Vec<std::path::PathBuf>,
}

impl Environment {
    /// create an environment for production use
    ///
    /// # Errors
    ///
    /// fails if we can not retrieve the information from the environment
    pub fn new() -> Result<Self, crate::error::Error> {
        Ok(Self {
            config_dir: dirs::config_dir()
                .ok_or(crate::error::Error::CouldNotDetermineUserConfigDir)?,
            state_dir: dirs::state_dir().ok_or(crate::error::Error::CouldNotDetermineStateDir)?,
            paths: std::env::var("PATH")?
                .split(':')
                .map(std::path::PathBuf::from)
                .collect(),
        })
    }

    /// create a mock environment for testing
    ///
    /// # Errors
    ///
    /// fails if creating the temporary directories fails
    #[cfg(test)]
    pub fn mock(temp_dir: &tempfile::TempDir) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_path = temp_dir.path();

        // Create 'bin', 'config', and 'state' subdirectories
        let config_dir = temp_path.join("config");
        let state_dir = temp_path.join("state");
        let bin_dir = temp_path.join("bin");

        fs_err::create_dir_all(&config_dir)?;
        fs_err::create_dir_all(&state_dir)?;
        fs_err::create_dir_all(&bin_dir)?;

        Ok(Self {
            config_dir,
            state_dir,
            paths: vec![bin_dir], // Add the bin_dir to the paths
        })
    }
}

/// the main function of the app
///
/// # Errors
///
/// fails if the main app fails
pub async fn run_app(
    options: Options,
    environment: Environment,
) -> Result<(), crate::error::Error> {
    match options.command {
        Command::Target(target_parameters) => {
            crate::targets::target_command(target_parameters, environment).await?;
        }
        Command::TargetSet(target_set_parameters) => {
            crate::target_sets::target_set_command(target_set_parameters, environment).await?;
        }
        Command::Plan(plan_parameters) => {
            crate::plans::plan_command(plan_parameters, environment).await?;
        }
        Command::Task(task_parameters) => {
            crate::tasks::task_command(task_parameters, environment).await?;
        }

        Command::GenerateManpage { output_dir } => {
            // generate man pages
            clap_mangen::generate_to(<Options as clap::CommandFactory>::command(), output_dir)
                .map_err(crate::error::Error::GenerateManpageError)?;
        }
        Command::GenerateShellCompletion { output_file, shell } => {
            let mut f = std::fs::File::create(output_file)
                .map_err(crate::error::Error::GenerateShellCompletionError)?;
            let mut c = <Options as clap::CommandFactory>::command();
            clap_complete::generate(shell, &mut c, "cargo-for-each", &mut f);
        }
    }

    Ok(())
}

/// represents a Rust workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// the directory that contains the workspace Cargo.toml file
    pub manifest_dir: PathBuf,
    /// is this a standalone crate workspace
    pub is_standalone: bool,
}

/// represents a Rust crate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crate {
    /// the directory that contains the crate Cargo.toml file
    pub manifest_dir: PathBuf,
    /// the directory that contains the workspace Cargo.toml file for this crate
    pub workspace_manifest_dir: PathBuf,
    /// the types of this crate (only bin and lib can be combined so this should have at most two members)
    pub types: BTreeSet<crate::targets::CrateType>,
}

/// represents the cargo-for-each configuration file
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// represents all the workspaces we know about
    pub workspaces: Vec<Workspace>,
    /// presents all the crates we know about
    pub crates: Vec<Crate>,
}

impl Config {
    /// adds a workspace to the config if it is not already present
    pub fn add_workspace(&mut self, workspace: Workspace) {
        if self
            .workspaces
            .iter()
            .any(|w| w.manifest_dir == workspace.manifest_dir)
        {
            tracing::debug!(
                "Workspace at {} already exists, not adding.",
                workspace.manifest_dir.display()
            );
        } else {
            tracing::debug!(
                "Adding new workspace at {}",
                workspace.manifest_dir.display()
            );
            self.workspaces.push(workspace);
        }
    }

    /// adds a crate to the config, ignoring the new one if one with the same manifest directory already exists
    pub fn add_crate(&mut self, krate: Crate) {
        if self
            .crates
            .iter()
            .any(|c| c.manifest_dir == krate.manifest_dir)
        {
            tracing::debug!(
                "Crate at {} already exists, not adding.",
                krate.manifest_dir.display()
            );
        } else {
            tracing::debug!("Adding new crate at {}", krate.manifest_dir.display());
            self.crates.push(krate);
        }
    }

    /// Load the config file
    ///
    /// # Errors
    ///
    /// Returns an error if the config file path cannot be determined,
    /// if the file cannot be read, or if its content cannot be parsed.
    pub fn load(environment: &Environment) -> Result<Self, crate::error::Error> {
        let config_file_path = config_file(environment)?;
        if fs_err::exists(&config_file_path).map_err(crate::error::Error::CouldNotReadConfigFile)? {
            let file_content = fs_err::read_to_string(&config_file_path)
                .map_err(crate::error::Error::CouldNotReadConfigFile)?;
            toml::from_str(&file_content).map_err(crate::error::Error::CouldNotParseConfigFile)
        } else {
            Ok(Self::default())
        }
    }

    /// Save the config file
    ///
    /// # Errors
    ///
    /// Returns an error if the config file path cannot be determined,
    /// if parent directories cannot be created, if the config cannot be serialized,
    /// or if the config file cannot be written.
    pub fn save(&self, environment: &Environment) -> Result<(), crate::error::Error> {
        let config_file_path = config_file(environment)?;
        if let Some(config_dir_path) = config_file_path.parent() {
            fs_err::create_dir_all(config_dir_path)
                .map_err(crate::error::Error::CouldNotCreateConfigFileParentDirs)?;
        }
        fs_err::write(
            &config_file_path,
            toml::to_string(self).map_err(crate::error::Error::CouldNotSerializeConfigFile)?,
        )
        .map_err(crate::error::Error::CouldNotWriteConfigFile)
    }
}

/// returns the config dir path
///
/// # Errors
///
/// Returns an error if the user's config directory cannot be determined.
pub fn config_dir_path(environment: &Environment) -> Result<PathBuf, crate::error::Error> {
    Ok(environment.config_dir.join("cargo-for-each"))
}

/// returns the config file path
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn config_file(environment: &Environment) -> Result<PathBuf, crate::error::Error> {
    Ok(config_dir_path(environment)?.join("cargo-for-each.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{ListParameters, ListType, TargetParameters, TargetType};

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_target_list() -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory for the test environment
        // needs to be done here since it cleans up when it goes
        // out of scope
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;

        // Create Options for the "targets list" command
        let options = Options {
            command: Command::Target(TargetParameters {
                target_type: TargetType::List(ListParameters {
                    list_type: ListType::Workspaces(
                        crate::targets::WorkspaceListParameters::default(),
                    ),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment).await;
        assert!(
            result.is_ok(),
            "run_app failed with error: {:?}",
            result.err()
        );

        Ok(())
    }
}
