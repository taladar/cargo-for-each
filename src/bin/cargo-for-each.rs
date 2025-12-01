#![doc = include_str!("../../README.md")]

use std::collections::BTreeSet;
use std::fmt::Write as _; // Required for write! macro

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
fn is_executable(path: &std::path::Path) -> bool {
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
fn is_executable(path: &std::path::Path) -> bool {
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
            if std::path::Path::new(&path_with_ext).is_file() {
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
fn is_executable(path: &std::path::Path) -> bool {
    // Fallback for non-unix, non-windows systems.
    path.is_file()
}

/// Error enum for the application
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// error reading environment variable
    #[error("error when retrieving environment variable: {0}")]
    EnvVarError(
        #[source]
        #[from]
        std::env::VarError,
    ),
    /// error in clap
    #[error("error in CLI option parsing: {0}")]
    ClapError(
        #[source]
        #[from]
        clap::Error,
    ),
    /// error parsing log filter
    #[error("error parsing log filter: {0}")]
    LogFilterParseError(
        #[source]
        #[from]
        tracing_subscriber::filter::ParseError,
    ),
    /// error joining task
    #[error("error joining task: {0}")]
    JoinError(
        #[source]
        #[from]
        tokio::task::JoinError,
    ),
    /// error constructing tracing-journald layer
    #[cfg(target_os = "linux")]
    #[error("error constructing tracing-journald layer: {0}")]
    TracingJournaldError(#[source] std::io::Error),
    /// error generating man pages
    #[error("error generating man pages: {0}")]
    GenerateManpageError(#[source] std::io::Error),
    /// error generating shell completion
    #[error("error generating shell completion: {0}")]
    GenerateShellCompletionError(#[source] std::io::Error),
    /// error determining user config dir
    #[error("error determining user config dir")]
    CouldNotDetermineUserConfigDir,
    /// error reading config file
    #[error("error reading config file: {0}")]
    CouldNotReadConfigFile(#[source] std::io::Error),
    /// error parsing config file
    #[error("error parsing config file: {0}")]
    CouldNotParseConfigFile(#[source] toml::de::Error),
    /// error serializing config file
    #[error("error serializing config file: {0}")]
    CouldNotSerializeConfigFile(#[source] toml::ser::Error),
    /// error serializing target set file
    #[error("error serializing target set file: {0}")]
    CouldNotSerializeTargetSetFile(#[source] toml::ser::Error),
    /// could not create parent directories for config file
    #[error("could not create parent directories for config file: {0}")]
    CouldNotCreateConfigFileParentDirs(#[source] std::io::Error),
    /// could not create parent directories for target set file
    #[error("could not create parent directories for target set file: {0}")]
    CouldNotCreateTargetSetFileParentDirs(#[source] std::io::Error),
    /// error writing config file
    #[error("error writing config file: {0}")]
    CouldNotWriteConfigFile(#[source] std::io::Error),
    /// error writing target set file
    #[error("error writing target set file: {0}")]
    CouldNotWriteTargetSetFile(#[source] std::io::Error),
    /// error deleting target set file
    #[error("error deleting target set file: {0}")]
    CouldNotDeleteTargetSetFile(#[source] std::io::Error),
    /// error reading target sets dir
    #[error("error reading target sets dir: {0}")]
    CouldNotReadTargetSetsDir(#[source] std::io::Error),
    /// error serializing plan file
    #[error("error serializing plan file: {0}")]
    CouldNotSerializePlanFile(#[source] toml::ser::Error),
    /// could not create parent directories for plan file
    #[error("could not create parent directories for plan file: {0}")]
    CouldNotCreatePlanFileParentDirs(#[source] std::io::Error),
    /// error writing plan file
    #[error("error writing plan file: {0}")]
    CouldNotWritePlanFile(#[source] std::io::Error),
    /// error deleting plan file
    #[error("error deleting plan file: {0}")]
    CouldNotDeletePlanFile(#[source] std::io::Error),
    /// error reading plan file
    #[error("error reading plan file: {0}")]
    CouldNotReadPlanFile(#[source] std::io::Error),
    /// error parsing plan file
    #[error("error parsing plan file: {0}")]
    CouldNotParsePlanFile(#[source] toml::de::Error),
    /// plan step is out of bounds
    #[error("plan step {0} is out of bounds for plan with {1} steps (valid range is 1 to {1})")]
    PlanStepOutOfBounds(usize, usize),
    /// error running cargo-metadata
    #[error("error running cargo-metadata for {0}: {1}")]
    CargoMetadataError(std::path::PathBuf, #[source] cargo_metadata::Error),
    /// error turning a relative manifest path into an absolute one
    #[error("error turning the relative manifest path {0} into an absolute one: {1}")]
    CouldNotDetermineAbsoluteManifestPath(std::path::PathBuf, #[source] std::io::Error),
    /// error turning a absolute manifest path into a canonical one
    #[error("error turning the absolute manifest path {0} into a canonical one: {1}")]
    CouldNotDetermineCanonicalManifestPath(std::path::PathBuf, #[source] std::io::Error),
    /// the given manifest path has no parent directory
    #[error("the given manifest path {0} has no parent directory")]
    ManifestPathHasNoParentDir(std::path::PathBuf),
    /// we called cargo metadata on a directory with a Cargo.toml
    /// but the output did not contain a package with the manifest_path
    /// pointing to that Cargo.toml
    #[error(
        "found no package with manifest_path matching local Cargo.toml in cargo metadata output: {0}"
    )]
    FoundNoPackageInCargoMetadataWithCurrentManifestPath(std::path::PathBuf),
    /// metadata did not include a package with the given package id
    #[error("cargo metadata did not include a package with the package id {0}")]
    FoundNoPackageInCargoMetadataWithPackageId(cargo_metadata::PackageId),
    /// error executing a command
    #[error("error executing command `{command:?}` in `{manifest_dir}`: {source}")]
    CommandExecutionError {
        /// The directory in which the command was attempted to be executed.
        manifest_dir: std::path::PathBuf,
        /// The command and its arguments that were attempted to be executed.
        command: Vec<String>,
        /// The underlying I/O error that occurred.
        #[source]
        source: std::io::Error,
    },
    /// The specified command was not found in PATH
    #[error("command not found: {0}")]
    CommandNotFound(String),
    /// error formatting a string
    #[error("error formatting a string: {0}")]
    FmtError(#[from] std::fmt::Error),
}

/// an extension trait on Cargo Metadata that allows easy retrieval
/// of a few pieces of information we need regularly
pub trait CargoMetadataExt {
    /// allows retrieval of a package by the manifest_path of its Cargo.toml
    ///
    /// this is usually required to get our own package in a workspace Metadata
    /// object that includes multiple packages
    ///
    /// # Errors
    ///
    /// fails if there is no package like that
    fn get_package_by_manifest_path(
        &self,
        manifest_path: &std::path::Path,
    ) -> Result<&cargo_metadata::Package, Error>;

    /// allows retrieval of a package by the package id
    ///
    /// this is usually required to retrieve the package object
    /// for package ids mentioned in e.g. workspace members
    ///
    /// # Errors
    ///
    /// fails if there is no package like that
    fn get_package_by_id(
        &self,
        package_id: &cargo_metadata::PackageId,
    ) -> Result<&cargo_metadata::Package, Error>;
}

impl CargoMetadataExt for cargo_metadata::Metadata {
    fn get_package_by_manifest_path(
        &self,
        manifest_path: &std::path::Path,
    ) -> Result<&cargo_metadata::Package, Error> {
        let Some(package) = self
            .packages
            .iter()
            .find(|p| p.manifest_path == manifest_path)
        else {
            return Err(Error::FoundNoPackageInCargoMetadataWithCurrentManifestPath(
                manifest_path.to_owned(),
            ));
        };
        Ok(package)
    }

    fn get_package_by_id(
        &self,
        package_id: &cargo_metadata::PackageId,
    ) -> Result<&cargo_metadata::Package, Error> {
        let Some(package) = self.packages.iter().find(|p| p.id == *package_id) else {
            return Err(Error::FoundNoPackageInCargoMetadataWithPackageId(
                package_id.to_owned(),
            ));
        };
        Ok(package)
    }
}

/// an extension trait on Cargo Metadata Packages that allows easy retrieval of a
/// few pieces of information we need regularly
pub trait CargoPackageExt {
    /// allows checking if this package has at least one target of the specified kind
    #[must_use]
    fn has_target(&self, target_kind: &cargo_metadata::TargetKind) -> bool;
}

impl CargoPackageExt for cargo_metadata::Package {
    fn has_target(&self, target_kind: &cargo_metadata::TargetKind) -> bool {
        self.targets.iter().any(|t| t.kind.contains(target_kind))
    }
}

/// represents a Rust workspace
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    /// the directory that contains the workspace Cargo.toml file
    manifest_dir: std::path::PathBuf,
    /// is this a standalone crate workspace
    is_standalone: bool,
}

/// represents the type of Rust crate
#[derive(
    Debug,
    Clone,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
pub enum CrateType {
    /// a binary crate
    Bin,
    /// a library crate
    Lib,
    /// a proc-macro crate
    ProcMacro,
}

impl CrateType {
    /// determine the set of `CrateType` for a given package
    #[must_use]
    pub fn from_package(package: &cargo_metadata::Package) -> BTreeSet<Self> {
        let mut crate_types = BTreeSet::new();
        if package.has_target(&cargo_metadata::TargetKind::Bin) {
            crate_types.insert(Self::Bin);
        }
        if package.has_target(&cargo_metadata::TargetKind::Lib) {
            crate_types.insert(Self::Lib);
        }
        if package.has_target(&cargo_metadata::TargetKind::ProcMacro) {
            crate_types.insert(Self::ProcMacro);
        }
        crate_types
    }
}

/// represents a Rust crate
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Crate {
    /// the directory that contains the crate Cargo.toml file
    manifest_dir: std::path::PathBuf,
    /// the directory that contains the workspace Cargo.toml file for this crate
    workspace_manifest_dir: std::path::PathBuf,
    /// the types of this crate (only bin and lib can be combined so this should have at most two members)
    types: std::collections::BTreeSet<CrateType>,
}

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

/// represents the cargo-for-each configuration file
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// represents all the workspaces we know about
    workspaces: Vec<Workspace>,
    /// presents all the crates we know about
    crates: Vec<Crate>,
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
    fn load() -> Result<Self, Error> {
        let config_file_path = config_file()?;
        if fs_err::exists(&config_file_path).map_err(Error::CouldNotReadConfigFile)? {
            let file_content =
                fs_err::read_to_string(&config_file_path).map_err(Error::CouldNotReadConfigFile)?;
            toml::from_str(&file_content).map_err(Error::CouldNotParseConfigFile)
        } else {
            Ok(Self::default())
        }
    }

    /// Save the config file
    fn save(&self) -> Result<(), Error> {
        let config_file_path = config_file()?;
        if let Some(config_dir_path) = config_file_path.parent() {
            fs_err::create_dir_all(config_dir_path)
                .map_err(Error::CouldNotCreateConfigFileParentDirs)?;
        }
        fs_err::write(
            &config_file_path,
            toml::to_string(self).map_err(Error::CouldNotSerializeConfigFile)?,
        )
        .map_err(Error::CouldNotWriteConfigFile)
    }
}

/// returns the config dir path
fn config_dir_path() -> Result<std::path::PathBuf, Error> {
    Ok(dirs::config_dir()
        .ok_or(Error::CouldNotDetermineUserConfigDir)?
        .join("cargo-for-each"))
}

/// returns the config file path
fn config_file() -> Result<std::path::PathBuf, Error> {
    Ok(config_dir_path()?.join("cargo-for-each.toml"))
}

/// returns the target sets dir path
fn target_sets_dir_path() -> Result<std::path::PathBuf, Error> {
    Ok(config_dir_path()?.join("target-sets"))
}

/// returns the plans dir path
fn plans_dir_path() -> Result<std::path::PathBuf, Error> {
    Ok(config_dir_path()?.join("plans"))
}

/// loads a plan from a file
fn load_plan(name: &str) -> Result<Plan, Error> {
    let plan_path = plans_dir_path()?.join(format!("{name}.toml"));
    if plan_path.exists() {
        let file_content =
            fs_err::read_to_string(plan_path).map_err(Error::CouldNotReadPlanFile)?;
        toml::from_str(&file_content).map_err(Error::CouldNotParsePlanFile)
    } else {
        Ok(Plan::default())
    }
}

/// saves a plan to a file
fn save_plan(name: &str, plan: &Plan) -> Result<(), Error> {
    let plan_path = plans_dir_path()?.join(format!("{name}.toml"));
    if let Some(plan_dir_path) = plan_path.parent() {
        fs_err::create_dir_all(plan_dir_path).map_err(Error::CouldNotCreatePlanFileParentDirs)?;
    }
    fs_err::write(
        &plan_path,
        toml::to_string(plan).map_err(Error::CouldNotSerializePlanFile)?,
    )
    .map_err(Error::CouldNotWritePlanFile)
}

/// implementation of the target-set subcommand
///
/// # Errors
///
/// fails if the implementation of target-set fails
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
async fn target_set_command(
    target_set_parameters: TargetSetParameters,
) -> Result<(), crate::Error> {
    match target_set_parameters.command {
        TargetSetCommand::Create(params) => {
            let target_set = match params.target_set_type {
                TargetSetType::Crates(params) => TargetSet::Crates(params),
                TargetSetType::Workspaces(params) => TargetSet::Workspaces(params),
            };
            let target_set_path = target_sets_dir_path()?.join(format!("{}.toml", params.name));
            if let Some(target_set_dir_path) = target_set_path.parent() {
                fs_err::create_dir_all(target_set_dir_path)
                    .map_err(Error::CouldNotCreateTargetSetFileParentDirs)?;
            }
            fs_err::write(
                &target_set_path,
                toml::to_string(&target_set).map_err(Error::CouldNotSerializeTargetSetFile)?,
            )
            .map_err(Error::CouldNotWriteTargetSetFile)?;
        }
        TargetSetCommand::Delete(params) => {
            let target_set_path = target_sets_dir_path()?.join(format!("{}.toml", params.name));
            fs_err::remove_file(target_set_path).map_err(Error::CouldNotDeleteTargetSetFile)?;
        }
        TargetSetCommand::List => {
            let target_sets_dir = target_sets_dir_path()?;
            if !target_sets_dir.exists() {
                return Ok(());
            }
            for entry in
                fs_err::read_dir(target_sets_dir).map_err(Error::CouldNotReadTargetSetsDir)?
            {
                let entry = entry.map_err(Error::CouldNotReadTargetSetsDir)?;
                let path = entry.path();
                if path.is_file()
                    && let Some(extension) = path.extension()
                    && extension == "toml"
                    && let Some(name) = path.file_stem()
                {
                    println!("{}", name.to_string_lossy());
                    let content =
                        fs_err::read_to_string(&path).map_err(Error::CouldNotReadConfigFile)?;
                    println!("{content}");
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
async fn plan_step_command(plan_step_parameters: PlanStepParameters) -> Result<(), crate::Error> {
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
        PlanStepCommand::Rm(params) => {
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

/// implementation of the plan subcommand
///
/// # Errors
///
/// fails if the implementation of plan fails
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
async fn plan_command(plan_parameters: PlanParameters) -> Result<(), crate::Error> {
    match plan_parameters.command {
        PlanCommand::Create(params) => {
            let plan = Plan::default();
            save_plan(&params.name, &plan)?;
        }
        PlanCommand::Delete(params) => {
            let plan_path = plans_dir_path()?.join(format!("{}.toml", params.name));
            fs_err::remove_file(plan_path).map_err(Error::CouldNotDeletePlanFile)?;
        }
        PlanCommand::Step(params) => {
            plan_step_command(params).await?;
        }
        PlanCommand::List => {
            let plans_dir = plans_dir_path()?;
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

/// Parameters for listing crates
#[derive(clap::Parser, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrateListParameters {
    /// only list crates of this type
    #[clap(long)]
    pub r#type: Option<CrateType>,
    /// only list crates that are standalone or not
    #[clap(long)]
    pub standalone: Option<bool>,
}

/// Parameters for listing workspaces
#[derive(clap::Parser, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceListParameters {
    /// only list multi-crate workspaces
    #[clap(long)]
    pub no_standalone: bool,
}

/// The type of object to list
#[derive(clap::Parser, Debug, Clone)]
pub enum ListType {
    /// list workspaces
    Workspaces(WorkspaceListParameters),
    /// list crates
    Crates(CrateListParameters),
}

/// Parameters for list subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct ListParameters {
    /// the type of object to list
    #[clap(subcommand)]
    pub list_type: ListType,
}

/// an enum that describes a set of targets
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TargetSet {
    /// a set of crates
    Crates(CrateListParameters),
    /// a set of workspaces
    Workspaces(WorkspaceListParameters),
}

/// The type of target set to create
#[derive(clap::Parser, Debug, Clone)]
pub enum TargetSetType {
    /// a set of workspaces
    Workspaces(WorkspaceListParameters),
    /// a set of crates
    Crates(CrateListParameters),
}

/// Parameters for target-set subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct CreateTargetSetParameters {
    /// the name of the target set
    #[clap(long)]
    pub name: String,
    /// the type of target set to create
    #[clap(subcommand)]
    pub target_set_type: TargetSetType,
}

/// Parameters for deleting a target set
#[derive(clap::Parser, Debug, Clone)]
pub struct DeleteTargetSetParameters {
    /// the name of the target set
    #[clap(long)]
    pub name: String,
}

/// The `target-set` subcommand
#[derive(clap::Parser, Debug, Clone)]
pub enum TargetSetCommand {
    /// Create a new target set
    Create(CreateTargetSetParameters),
    /// Delete a target set
    Delete(DeleteTargetSetParameters),
    /// List existing target sets
    List,
}

/// Parameters for target-set subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct TargetSetParameters {
    /// the `target-set` subcommand to run
    #[clap(subcommand)]
    pub command: TargetSetCommand,
}

/// Parameters for creating a new plan
#[derive(clap::Parser, Debug, Clone)]
pub struct CreatePlanParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
}

/// Parameters for adding a step to a plan
#[derive(clap::Parser, Debug, Clone)]
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
#[derive(clap::Parser, Debug, Clone)]
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
#[derive(clap::Parser, Debug, Clone)]
pub struct RmStepParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
    /// the position of the step to delete
    #[clap(long)]
    pub position: usize,
}

/// Parameters for deleting a plan
#[derive(clap::Parser, Debug, Clone)]
pub struct DeletePlanParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
}

/// Parameters for listing the steps of a plan
#[derive(clap::Parser, Debug, Clone)]
pub struct ListStepsParameters {
    /// the name of the plan
    #[clap(long)]
    pub name: String,
}

/// The `plan step` subcommand
#[derive(clap::Parser, Debug, Clone)]
pub enum PlanStepCommand {
    /// Add a step to a plan
    Add(AddStepParameters),
    /// Insert a step into a plan
    Insert(InsertStepParameters),
    /// Remove a step from a plan
    Rm(RmStepParameters),
    /// List the steps of a plan
    List(ListStepsParameters),
}

/// Parameters for plan step subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct PlanStepParameters {
    /// the `plan step` subcommand to run
    #[clap(subcommand)]
    pub command: PlanStepCommand,
}

/// The `plan` subcommand
#[derive(clap::Parser, Debug, Clone)]
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
#[derive(clap::Parser, Debug, Clone)]
pub struct PlanParameters {
    /// the `plan` subcommand to run
    #[clap(subcommand)]
    pub command: PlanCommand,
}

/// Parameters for add subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct AddParameters {
    /// the manifest file to add, if it refers to a workspace manifest all crates in the workspace are added too
    #[clap(long)]
    pub manifest_path: std::path::PathBuf,
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
    /// Call list subcommand
    List(ListParameters),
    /// Call add subcommand
    Add(AddParameters),
    /// create a new target set
    TargetSet(TargetSetParameters),
    /// manage plans
    Plan(PlanParameters),
    /// refresh the config, removing old entries and adding new ones
    Refresh,
    /// Execute a command in each configured directory
    Exec(ExecParameters),
    /// Generate man page
    GenerateManpage {
        /// target dir for man page generation
        #[clap(long)]
        output_dir: std::path::PathBuf,
    },
    /// Generate shell completion
    GenerateShellCompletion {
        /// output file for shell completion generation
        #[clap(long)]
        output_file: std::path::PathBuf,
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

/// implementation of the list subcommand
///
/// # Errors
///
/// fails if the implementation of list fails
#[instrument]
async fn list_command(list_parameters: ListParameters) -> Result<(), crate::Error> {
    #[expect(clippy::print_stderr, reason = "This is part of the UI, not logging")]
    let Ok(config) = Config::load() else {
        eprintln!("No config file found, nothing to list");
        return Ok(());
    };
    #[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
    match list_parameters.list_type {
        ListType::Workspaces(params) => {
            for workspace in config.workspaces {
                if params.no_standalone && workspace.is_standalone {
                    continue;
                }
                println!(
                    "{} (standalone: {})",
                    workspace.manifest_dir.display(),
                    workspace.is_standalone
                );
            }
        }
        ListType::Crates(params) => {
            let workspace_standalone_map: std::collections::HashMap<_, _> = config
                .workspaces
                .iter()
                .map(|w| (w.manifest_dir.clone(), w.is_standalone))
                .collect();

            for krate in config.crates {
                if let Some(crate_type) = &params.r#type
                    && !krate.types.contains(crate_type)
                {
                    continue;
                }
                if let Some(standalone) = params.standalone
                    && workspace_standalone_map
                        .get(&krate.workspace_manifest_dir)
                        .is_none_or(|&is_standalone| is_standalone != standalone)
                {
                    continue;
                }
                if krate.manifest_dir == krate.workspace_manifest_dir {
                    println!(
                        "{} (types: {:?})",
                        krate.manifest_dir.display(),
                        krate.types
                    );
                } else {
                    println!(
                        "{} (workspace: {}, types: {:?})",
                        krate.manifest_dir.display(),
                        krate.workspace_manifest_dir.display(),
                        krate.types
                    );
                }
            }
        }
    }
    Ok(())
}

/// implementation of the add subcommand
///
/// # Errors
///
/// fails if the implementation of add fails
#[instrument]
async fn add_command(add_parameters: AddParameters) -> Result<(), crate::Error> {
    let mut config = Config::load()?;
    let manifest_path =
        std::path::absolute(add_parameters.manifest_path.clone()).map_err(|err| {
            Error::CouldNotDetermineAbsoluteManifestPath(add_parameters.manifest_path, err)
        })?;
    let manifest_path = fs_err::canonicalize(manifest_path.clone())
        .map_err(|err| Error::CouldNotDetermineCanonicalManifestPath(manifest_path, err))?;

    // first call to metadata to find the workspace root
    let initial_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&manifest_path)
        .exec()
        .map_err(|err| Error::CargoMetadataError(manifest_path.clone(), err))?; // manifest_path here is already std::path::PathBuf
    let workspace_manifest_path_camino = initial_metadata.workspace_root.join("Cargo.toml");

    let Some(workspace_manifest_dir_camino) = workspace_manifest_path_camino.parent() else {
        return Err(Error::ManifestPathHasNoParentDir(
            workspace_manifest_path_camino.into_std_path_buf(),
        ));
    };
    let workspace_manifest_dir_camino = workspace_manifest_dir_camino.to_path_buf();

    // second call to metadata to get all packages in the workspace
    let workspace_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&workspace_manifest_path_camino)
        .exec()
        .map_err(|err| {
            Error::CargoMetadataError(
                workspace_manifest_path_camino.clone().into_std_path_buf(),
                err,
            )
        })?;

    let is_standalone = if let [package_id] = workspace_metadata.workspace_members.as_slice() {
        let package = workspace_metadata.get_package_by_id(package_id)?;
        package.manifest_path == workspace_manifest_path_camino
    } else {
        false
    };

    if is_standalone {
        tracing::debug!("Identified Cargo.toml as standalone crate");
        let package = workspace_metadata
            .get_package_by_manifest_path(&workspace_manifest_path_camino.into_std_path_buf())?; // Convert for comparison
        let crate_types = CrateType::from_package(package);
        config.add_workspace(Workspace {
            manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
            is_standalone: true,
        });
        config.add_crate(Crate {
            manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
            workspace_manifest_dir: workspace_manifest_dir_camino.into_std_path_buf(),
            types: crate_types,
        });
    } else {
        tracing::debug!("Identified Cargo.toml as workspace");
        config.add_workspace(Workspace {
            manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
            is_standalone: false,
        });
        for package_id in workspace_metadata.workspace_members.clone() {
            let package = workspace_metadata.get_package_by_id(&package_id)?;
            let package_manifest_path = package.manifest_path.to_owned().into_std_path_buf();
            let Some(package_manifest_dir) = package_manifest_path.parent() else {
                return Err(Error::ManifestPathHasNoParentDir(package_manifest_path));
            };
            let crate_types = CrateType::from_package(package);
            config.add_crate(Crate {
                manifest_dir: package_manifest_dir.to_path_buf(),
                workspace_manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
                types: crate_types,
            });
        }
    }

    config.save()?;

    Ok(())
}

/// implementation of the refresh subcommand
///
/// # Errors
///
/// fails if the implementation of refresh fails
#[instrument]
async fn refresh_command() -> Result<(), crate::Error> {
    let mut config = Config::load()?;

    // 1. Remove workspaces that no longer exist.
    let (retained_workspaces, removed_workspaces): (Vec<_>, Vec<_>) = config
        .workspaces
        .drain(..)
        .partition(|w| w.manifest_dir.join("Cargo.toml").is_file());
    for r in &removed_workspaces {
        tracing::debug!(
            "Removing workspace at {} because Cargo.toml is gone.",
            r.manifest_dir.display()
        );
    }
    config.workspaces = retained_workspaces;

    // 2. Remove crates that no longer exist.
    let (retained_crates, removed_crates): (Vec<_>, Vec<_>) = config
        .crates
        .drain(..)
        .partition(|c| c.manifest_dir.join("Cargo.toml").is_file());
    for r in &removed_crates {
        tracing::debug!(
            "Removing crate at {} because Cargo.toml is gone.",
            r.manifest_dir.display()
        );
    }
    config.crates = retained_crates;

    // 3. For all existing workspaces, discover and add new member crates.
    //    We don't need to update existing crates found here, as the next step will do it.
    let workspaces_to_scan = config.workspaces.clone();
    for workspace in &workspaces_to_scan {
        let manifest_path = workspace.manifest_dir.join("Cargo.toml");
        let cargo_metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .exec()
            .map_err(|err| Error::CargoMetadataError(manifest_path, err))?;

        for package_id in &cargo_metadata.workspace_members {
            let package = cargo_metadata.get_package_by_id(package_id)?;
            let pkg_manifest_path = package.manifest_path.to_owned().into_std_path_buf();
            if let Some(manifest_dir) = pkg_manifest_path.parent() {
                let manifest_dir = manifest_dir.to_path_buf();

                // Only add if it doesn't exist. `add_crate` does this.
                if !config.crates.iter().any(|c| c.manifest_dir == manifest_dir) {
                    let crate_types = CrateType::from_package(package);
                    config.add_crate(Crate {
                        manifest_dir,
                        workspace_manifest_dir: workspace.manifest_dir.clone(),
                        types: crate_types,
                    });
                }
            }
        }
    }

    // 4. Update crate_types for all existing crates.
    for krate in &mut config.crates {
        let manifest_path = krate.manifest_dir.join("Cargo.toml");

        let cargo_metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .no_deps()
            .exec()
            .map_err(|err| Error::CargoMetadataError(manifest_path.clone(), err))?;

        // We need the package object to determine the crate type.
        // Using get_package_by_manifest_path is correct for single crates/workspace members.
        if let Ok(package) = cargo_metadata.get_package_by_manifest_path(&manifest_path) {
            let new_crate_types = CrateType::from_package(package);
            if krate.types != new_crate_types {
                tracing::debug!(
                    "Updating types for {} from {:?} to {:?}",
                    krate.manifest_dir.display(),
                    krate.types,
                    new_crate_types
                );
                krate.types = new_crate_types;
            }
        } else {
            tracing::warn!(
                "Could not find package for manifest path {} during refresh.",
                manifest_path.display()
            );
        }
    }

    config.save()?;
    Ok(())
}

/// implementation of the exec subcommand
///
/// # Errors
///
/// fails if the implementation of exec fails
#[instrument]
async fn exec_command(exec_parameters: ExecParameters) -> Result<(), crate::Error> {
    let config = Config::load()?;

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
            let workspace_standalone_map: std::collections::HashMap<_, _> = config
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
    let command_path = std::path::Path::new(&command);
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
async fn do_stuff() -> Result<(), crate::Error> {
    let options = <Options as clap::Parser>::parse();
    tracing::debug!("{:#?}", options);

    // main code either goes here or into the individual subcommands

    match options.command {
        Command::List(list_parameters) => {
            list_command(list_parameters).await?;
        }
        Command::Add(add_parameters) => {
            add_command(add_parameters).await?;
        }
        Command::TargetSet(target_set_parameters) => {
            target_set_command(target_set_parameters).await?;
        }
        Command::Plan(plan_parameters) => {
            plan_command(plan_parameters).await?;
        }
        Command::Refresh => {
            refresh_command().await?;
        }
        Command::Exec(exec_parameters) => {
            exec_command(exec_parameters).await?;
        }
        Command::GenerateManpage { output_dir } => {
            // generate man pages
            clap_mangen::generate_to(<Options as clap::CommandFactory>::command(), output_dir)
                .map_err(crate::Error::GenerateManpageError)?;
        }
        Command::GenerateShellCompletion { output_file, shell } => {
            let mut f = std::fs::File::create(output_file)
                .map_err(crate::Error::GenerateShellCompletionError)?;
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
            .map_err(crate::Error::TracingJournaldError)?
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
