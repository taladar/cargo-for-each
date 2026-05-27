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
    /// could not open or acquire the config-directory lock file
    #[error("could not acquire config lock at {0}: {1}")]
    CouldNotAcquireConfigLock(std::path::PathBuf, #[source] std::io::Error),
    /// the specified task was not found
    #[error("the specified task {0} was not found")]
    TaskNotFound(String),
    /// `target remove` was given a manifest path that matched no registered
    /// workspace and no registered crate
    #[error("no registered workspace or crate matches manifest path {0}")]
    TargetNotFound(std::path::PathBuf),
    /// the supplied task name does not satisfy the validation rules
    #[error("invalid task name {0:?}: {1}")]
    InvalidTaskName(String, &'static str),
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
    /// A command exited with a non-zero status. The `i32` is the value the
    /// asciinema wrapper recorded to the `exit_status` state file: on Unix,
    /// signal kills are encoded by the wrapping shell as `128 + signum`
    /// (e.g. `137` for `SIGKILL`), not as `None`. Empty/unparseable contents
    /// surface as [`Error::InvalidRecordedExitStatus`] instead.
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
    /// error reading state file
    #[error("error reading state file {0}: {1}")]
    CouldNotReadStateFile(std::path::PathBuf, #[source] std::io::Error),
    /// the recorded exit_status file contained content that could not be
    /// parsed as an integer exit code
    #[error("recorded exit status is not a valid integer: {0:?}")]
    InvalidRecordedExitStatus(String),
    /// an IO error occurred
    // Intentionally no `#[from]`: this enum has many specific `io::Error`-bearing
    // variants (e.g. `CouldNotReadConfigFile`, `CouldNotWriteStateFile`) that
    // require their own context. A blanket `From<io::Error>` would let callers
    // accidentally drop that context and would conflict with the specific paths.
    #[error("I/O error: {0}")]
    IoError(#[source] std::io::Error),
    /// a git error occurred
    #[error("git error: {0}")]
    GitError(
        #[source]
        #[from]
        git2::Error,
    ),
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
    /// `check` surfaced one or more error-severity findings; the user-facing
    /// messages have already been printed before this variant is returned.
    /// Exists so the binary exits non-zero on errors and so callers can
    /// distinguish check failures from other errors.
    #[error("check found {errors} error(s) and {warnings} warning(s)")]
    CheckFoundIssues {
        /// number of error-severity findings printed
        errors: usize,
        /// number of warning-severity findings printed
        warnings: usize,
    },
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
    /// the env file specified in a `with_env_file` block could not be read
    #[error("could not read env file {0}: {1}")]
    CouldNotReadEnvFile(std::path::PathBuf, #[source] std::io::Error),
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
    /// a cursor string given to `task continue` could not be parsed
    #[error("invalid cursor string {0:?}: {1}")]
    InvalidCursorString(String, String),
    /// the command or one of its args contains a NUL byte, which POSIX `sh`
    /// cannot represent
    #[error("argument to `run` step contains an invalid byte (NUL): {0:?}")]
    InvalidCommandArg(String),
    /// `file_exists` was evaluated against a target whose manifest directory is
    /// not registered in the config (neither as a workspace nor as a crate)
    #[error(
        "`file_exists` evaluated against unregistered target {0}: cannot determine workspace boundary"
    )]
    FileExistsTargetNotRegistered(std::path::PathBuf),
    /// the path passed to `file_exists` resolves outside the enclosing
    /// workspace's manifest directory
    #[error(
        "`file_exists {0:?}` resolves outside the enclosing workspace; only paths within the workspace manifest directory are allowed"
    )]
    FileExistsPathOutsideWorkspace(String),
    /// `task continue` was given a cursor that addresses a real statement
    /// in the task's program, but that statement is not a
    /// `wait_for_continue` barrier.
    #[error("cursor {0:?} addresses a statement, but it is not a wait_for_continue barrier")]
    CursorNotAtBarrier(String),
    /// `task continue` was given a cursor whose path does not match the
    /// task's program structure (e.g. statement index past the end, or a
    /// segment kind that does not fit at that position).
    #[error("cursor {0:?} does not address any statement in this task's program")]
    CursorNotInProgram(String),
    /// the user requested parallel execution (`-j > 1`) of a program that
    /// contains interactive steps (`manual_step` or an `ask_user` condition)
    #[error(
        "cannot run with --jobs > 1 because the program contains interactive steps (manual_step or ask_user); rerun with --jobs 1"
    )]
    InteractiveStepsRequireSingleJob,
    /// a command run as a condition (`if run "..."`) terminated by signal
    /// rather than exiting normally; the boolean result is undefined
    #[error("condition command `{0}` in `{1}` was killed by a signal")]
    ConditionCommandKilledBySignal(String, PathBuf),
}
