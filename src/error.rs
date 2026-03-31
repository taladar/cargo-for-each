//! This module defines the error types used throughout the `cargo-for-each` library.
use std::path::PathBuf;

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
    /// could not create parent directories for config file
    #[error("could not create parent directories for config file: {0}")]
    CouldNotCreateConfigFileParentDirs(#[source] std::io::Error),
    /// error writing config file
    #[error("error writing config file: {0}")]
    CouldNotWriteConfigFile(#[source] std::io::Error),
    /// the specified task was not found
    #[error("the specified task {0} was not found")]
    TaskNotFound(String),
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
    /// could not remove task state directory
    #[error("could not remove task state directory {0}: {1}")]
    CouldNotRemoveTaskStateDir(std::path::PathBuf, #[source] std::io::Error),
    /// could not read tasks directory
    #[error("could not read tasks directory {0}: {1}")]
    CouldNotReadTasksDir(std::path::PathBuf, #[source] std::io::Error),
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
    /// the task of the given name already exists
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
    #[error("error executing command `{0}` in `{1}`: {2}")]
    CommandExecutionFailed(String, PathBuf, #[source] std::io::Error),
    /// A command failed to execute
    #[error("command `{0}` failed in `{1}` with exit code {2}")]
    CommandFailed(String, PathBuf, i32),
    /// The specified command was not found in PATH
    #[error("command not found: {0}")]
    CommandNotFound(String),
    /// error formatting a string
    #[error("error formatting a string: {0}")]
    FmtError(#[from] std::fmt::Error),
    /// error determining user state dir
    #[error("error determining user state dir")]
    CouldNotDetermineStateDir,
    /// could not create state directory
    #[error("could not create state directory {0}: {1}")]
    CouldNotCreateStateDir(std::path::PathBuf, #[source] std::io::Error),
    /// error writing state file
    #[error("error writing state file {0}: {1}")]
    CouldNotWriteStateFile(std::path::PathBuf, #[source] std::io::Error),
    /// an IO error occurred
    #[error("I/O error: {0}")]
    IoError(#[source] std::io::Error),
    /// the user did not confirm the manual step
    #[error("manual step not confirmed")]
    ManualStepNotConfirmed,
    /// a condition result state file contained an unexpected value
    #[error("invalid condition result: {0:?}")]
    InvalidConditionResult(String),
    /// The chosen_branch state file contains an unrecognized value.
    #[error("invalid chosen branch value: {0:?}")]
    InvalidChosenBranch(String),
    /// some steps failed
    #[error("some steps failed")]
    SomeStepsFailed,
    /// circular dependency or deadlock detected
    #[error("circular dependency or deadlock detected")]
    CircularDependency,
    /// error serializing cargo metadata snapshot to JSON
    #[error("error serializing cargo metadata snapshot: {0}")]
    CouldNotSerializeMetadataSnapshot(#[source] serde_json::Error),
    /// error deserializing a cargo metadata snapshot from JSON
    #[error("error deserializing cargo metadata snapshot: {0}")]
    CouldNotDeserializeMetadataSnapshot(#[source] serde_json::Error),
    /// a snapshot with the given name was not found
    #[error("snapshot '{0}' not found; was `snapshot_metadata \"{0}\"` executed before this step?")]
    SnapshotNotFound(String),
    /// the current crate's package was not found in the named snapshot
    #[error("package for {1} not found in snapshot '{0}'")]
    SnapshotPackageNotFound(String, std::path::PathBuf),
    /// the given field path was not found in the package JSON
    #[error("field '{1}' not found in package for snapshot '{0}'")]
    SnapshotFieldNotFound(String, String),
    /// a `${{...}}` interpolation reference is malformed
    #[error(
        "invalid interpolation reference '{0}': must be '${{name.field}}' with at least one field after the name"
    )]
    InvalidInterpolation(String),
    /// the specified program file was not found
    #[error("program file not found: {0}")]
    ProgramNotFound(std::path::PathBuf),
    /// error reading program file
    #[error("error reading program file: {0}")]
    CouldNotReadProgramFile(#[source] std::io::Error),
    /// one or more parse errors in the program file
    #[error("program parse errors:\n{0}")]
    ProgramParseErrors(String),
    /// error serializing resolved program snapshot
    #[error("error serializing resolved program snapshot: {0}")]
    CouldNotSerializeResolvedProgram(#[source] toml::ser::Error),
    /// error writing resolved program snapshot file
    #[error("error writing resolved program snapshot file: {0}")]
    CouldNotWriteResolvedProgram(#[source] std::io::Error),
    /// error reading resolved program snapshot file
    #[error("error reading resolved program snapshot file {0}: {1}")]
    CouldNotReadResolvedProgram(std::path::PathBuf, #[source] std::io::Error),
    /// error parsing resolved program snapshot file
    #[error("error parsing resolved program snapshot file {0}: {1}")]
    CouldNotParseResolvedProgram(std::path::PathBuf, #[source] toml::de::Error),
}
