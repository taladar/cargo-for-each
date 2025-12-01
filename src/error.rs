//! This module defines the error types used throughout the `cargo-for-each` library.

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
    /// could not check target set existence
    #[error("could not check target set existence for {0}: {1}")]
    CouldNotCheckTargetSetExistence(std::path::PathBuf, #[source] std::io::Error),
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
    /// error removing target set file
    #[error("error removing target set file: {0}")]
    CouldNotRemoveTargetSetFile(#[source] std::io::Error),
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
    /// error removing plan file
    #[error("error removing plan file: {0}")]
    CouldNotRemovePlanFile(#[source] std::io::Error),
    /// error reading plan file
    #[error("error reading plan file: {0}")]
    CouldNotReadPlanFile(#[source] std::io::Error),
    /// error parsing plan file
    #[error("error parsing plan file: {0}")]
    CouldNotParsePlanFile(#[source] toml::de::Error),
    /// plan step is out of bounds
    #[error("plan step {0} is out of bounds for plan with {1} steps (valid range is 1 to {1})")]
    PlanStepOutOfBounds(usize, usize),
    /// the specified task was not found
    #[error("the specified task {0} was not found")]
    TaskNotFound(String),
    /// the specified plan was not found
    #[error("the specified plan {0} was not found")]
    PlanNotFound(String),
    /// the specified target set was not found
    #[error("the specified target set {0} was not found")]
    TargetSetNotFound(String),
    /// could not create task directory
    #[error("could not create task directory {0}: {1}")]
    CouldNotCreateTaskDir(std::path::PathBuf, #[source] std::io::Error),
    /// could not copy file
    #[error("could not copy file from {0} to {1}: {2}")]
    CouldNotCopyFile(
        std::path::PathBuf,
        std::path::PathBuf,
        #[source] std::io::Error,
    ),
    /// could not remove task directory
    #[error("could not remove task directory {0}: {1}")]
    CouldNotRemoveTaskDir(std::path::PathBuf, #[source] std::io::Error),
    /// could not read resolved target set file
    #[error("could not read resolved target set file {0}: {1}")]
    CouldNotReadResolvedTargetSet(std::path::PathBuf, #[source] std::io::Error),
    /// could not parse resolved target set file
    #[error("could not parse resolved target set file {0}: {1}")]
    CouldNotParseResolvedTargetSet(std::path::PathBuf, #[source] toml::de::Error),
    /// error serializing resolved target set file
    #[error("error serializing resolved target set file: {0}")]
    CouldNotSerializeResolvedTargetSet(#[source] toml::ser::Error),
    /// could not create parent directories for resolved target set file
    #[error("could not create parent directories for resolved target set file: {0}")]
    CouldNotCreateResolvedTargetSetParentDirs(#[source] std::io::Error),
    /// error writing resolved target set file
    #[error("error writing resolved target set file: {0}")]
    CouldNotWriteResolvedTargetSet(#[source] std::io::Error),
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
    /// the target set/plan/task of the given name already exists
    #[error("{0} already exists")]
    AlreadyExists(String),
    /// we called cargo metadata on a directory with a Cargo.toml
    /// but the output did not contain a package with the manifest_path
    /// pointing to that Cargo.toml
    #[error(
        "found no package with manifest_path matching local Cargo.toml in cargo metadata output: {0}"
    )]
    FoundNoPackageInCargoMetadataWithCurrentManifestPath(std::path::PathBuf),
    /// we called cargo metadata for a given manifest_path
    /// but the output did not contain a package with the manifest_path
    /// pointing to that Cargo.toml
    #[error("found no package with manifest_path matching {0} in cargo metadata output")]
    FoundNoPackageInCargoMetadataWithGivenManifestPath(std::path::PathBuf),
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
