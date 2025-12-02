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
/// Implements functionality for managing targets.
pub mod targets_commands;
/// Implements functionality for managing tasks.
pub mod tasks;

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    pub fn load() -> Result<Self, crate::error::Error> {
        let config_file_path = config_file()?;
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
    pub fn save(&self) -> Result<(), crate::error::Error> {
        let config_file_path = config_file()?;
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
pub fn config_dir_path() -> Result<PathBuf, crate::error::Error> {
    Ok(dirs::config_dir()
        .ok_or(crate::error::Error::CouldNotDetermineUserConfigDir)?
        .join("cargo-for-each"))
}

/// returns the config file path
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn config_file() -> Result<PathBuf, crate::error::Error> {
    Ok(config_dir_path()?.join("cargo-for-each.toml"))
}
