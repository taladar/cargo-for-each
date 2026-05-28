//! Task management: creation, execution, and state tracking.
//!
//! Tasks are created from `.cfe` program files, which describe the steps to
//! run for each workspace and crate.  This module handles task creation,
//! execution (sequential and parallel), rewinding, and status display.

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use cargo_metadata::MetadataCommand;
use futures::stream::{self, StreamExt as _};
use tracing::instrument;

use crate::error::Error;
use crate::program::ast::common::{
    CommonCondition, ManualStepNode, RunStep, SnapshotMetadataNode, WaitForContinueNode,
};
use crate::program::ast::crate_ctx::{CrateCondition, CrateIfBlock, CrateStatement};
use crate::program::ast::workspace_ctx::{
    ForCrateInWorkspaceBlock, WorkspaceCondition, WorkspaceIfBlock, WorkspaceStatement,
};
use crate::program::cursor::{CursorSegment, ProgramCursor};
use crate::program::evaluate::{
    crate_condition_runtime_detail, evaluate_crate_condition, evaluate_workspace_condition,
    workspace_condition_runtime_detail,
};
use crate::program::resolve::{
    ResolvedCrateExecution, ResolvedProgram, ResolvedWorkspaceExecution,
};
use crate::program::{GlobalStatement, Program};
use crate::{Config, Environment};
use clap::Parser;

// ── Path helpers ───────────────────────────────────────────────────────────────

/// Validates a user-supplied task name so it can be joined safely into
/// `<config_dir>/cargo-for-each/tasks/` and the equivalent state-dir path
/// without escaping.
///
/// Rejects:
/// - empty names,
/// - names with leading/trailing whitespace (avoids confusion between
///   `"foo"` and `" foo "` directory entries on case-insensitive filesystems),
/// - names containing `/`, `\`, or NUL bytes (path separators on at least
///   one supported platform, even if not on the current one),
/// - names that don't reduce to exactly one `Component::Normal` (catches
///   `..`, `.`, absolute paths, and anything else with implicit path
///   semantics).
fn validate_task_name(name: &str) -> Result<(), Error> {
    use std::ffi::OsStr;
    use std::path::Component;

    let bad = |reason: &'static str| Error::InvalidTaskName(name.to_owned(), reason);

    if name.is_empty() {
        return Err(bad("must not be empty"));
    }
    if name.trim() != name {
        return Err(bad("must not have leading or trailing whitespace"));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(bad("must not contain path separators or NUL bytes"));
    }
    let mut components = Path::new(name).components();
    let Some(Component::Normal(only)) = components.next() else {
        return Err(bad(
            "must be a single non-empty path component (no '..', '.', or absolute paths)",
        ));
    };
    if only != OsStr::new(name) || components.next().is_some() {
        return Err(bad("must be a single non-empty path component"));
    }
    Ok(())
}

/// Returns the directory under which all task configuration lives:
/// `<config_dir>/cargo-for-each/tasks/`.
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn dir_path(environment: &crate::Environment) -> Result<PathBuf, Error> {
    Ok(crate::config_dir_path(environment).join("tasks"))
}

/// Returns a single task's configuration directory:
/// `<config_dir>/cargo-for-each/tasks/<name>/`.
///
/// # Errors
///
/// Returns an error if `name` fails `validate_task_name` or if the tasks
/// directory path cannot be determined.
pub fn named_dir_path(name: &str, environment: &crate::Environment) -> Result<PathBuf, Error> {
    validate_task_name(name)?;
    Ok(dir_path(environment)?.join(name))
}

/// Returns a single task's *execution state* directory:
/// `<state_dir>/cargo-for-each/tasks/<name>/`.  Distinct from
/// [`named_dir_path`]: configuration lives under the config dir; execution
/// state (cursor markers, exit-status files, snapshots, barrier markers,
/// asciicasts) lives here.
///
/// # Errors
///
/// Returns an error if `name` fails `validate_task_name` or if the state
/// directory path cannot be determined.
pub fn state_dir_for_task(name: &str, environment: &crate::Environment) -> Result<PathBuf, Error> {
    validate_task_name(name)?;
    Ok(environment
        .state_dir
        .join("cargo-for-each")
        .join("tasks")
        .join(name))
}

// ── Env file helpers ───────────────────────────────────────────────────────────

/// Parses a `.env`-format string into a list of `(key, value)` pairs.
///
/// Supports:
/// - `KEY=VALUE` lines (bare or with `export ` prefix)
/// - Lines starting with `#` are treated as comments and ignored
/// - Blank lines are ignored
/// - Values optionally wrapped in single or double quotes (quotes are stripped)
fn parse_env_file_content(content: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_owned();
        let value = value.trim();
        let value = if let Some(inner) = value
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        {
            inner.to_owned()
        } else {
            value.to_owned()
        };
        if !key.is_empty() {
            vars.push((key, value));
        }
    }
    vars
}

/// Reads and parses an env file at `path`, returning the key-value pairs.
///
/// # Errors
///
/// Returns [`Error::CouldNotReadEnvFile`] if the file cannot be read.
fn load_env_file(path: &Path) -> Result<Vec<(String, String)>, Error> {
    let content = fs_err::read_to_string(path)
        .map_err(|e| Error::CouldNotReadEnvFile(path.to_path_buf(), e))?;
    Ok(parse_env_file_content(&content))
}

/// Loads and combines env vars from a sequence of env file paths (relative to `manifest_dir`).
///
/// Files are applied in order; later files override earlier ones for the same key.
///
/// # Errors
///
/// Returns an error if any env file cannot be read.
fn load_env_vars_from_files(
    env_file_paths: &[String],
    manifest_dir: &Path,
) -> Result<Vec<(String, String)>, Error> {
    let mut vars = Vec::new();
    for path_str in env_file_paths {
        let path = manifest_dir.join(path_str);
        vars.extend(load_env_file(&path)?);
    }
    Ok(vars)
}

// ── CLI parameter structs ──────────────────────────────────────────────────────

/// Parameters for creating a new task.
#[derive(Parser, Debug, Clone)]
pub struct CreateTaskParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
    /// Path to the `.cfe` program file that defines the task steps.
    #[clap(long)]
    pub program: PathBuf,
    /// Explicit workspace directory paths to run the task against.
    ///
    /// When provided, these paths override the `select workspaces` statement(s)
    /// in the program.  Dependency ordering among the given workspaces is still
    /// computed automatically.  May be specified multiple times.
    #[clap(long = "workspace", value_name = "PATH")]
    pub workspaces: Vec<PathBuf>,
    /// Explicit crate directory paths to run the task against.
    ///
    /// When provided, these paths override the `select crates` statement(s)
    /// in the program.  Dependency ordering among the given crates is still
    /// computed automatically.  May be specified multiple times.
    #[clap(long = "crate", value_name = "PATH")]
    pub crates: Vec<PathBuf>,
}

/// Parameters for running the next single uncompleted statement of a task.
#[derive(Parser, Debug, Clone)]
pub struct RunSingleStepParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
}

/// Parameters for running all remaining statements for the first ready target.
#[derive(Parser, Debug, Clone)]
pub struct RunSingleTargetParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
}

/// Parameters for running a task across all targets in dependency order.
#[derive(Parser, Debug, Clone)]
pub struct RunAllTargetsParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
    /// Number of parallel jobs (similar to `make -j`). Defaults to 1.
    #[clap(short = 'j', long)]
    pub jobs: Option<usize>,
    /// Continue running even when some targets fail (similar to `make -k`).
    #[clap(short = 'k', long)]
    pub keep_going: bool,
}

/// The `task run` subcommand.
#[derive(Parser, Debug, Clone)]
pub enum TaskRunSubCommand {
    /// Run the next single uncompleted statement of the task.
    SingleStep(RunSingleStepParameters),
    /// Run all remaining statements for the first ready target.
    SingleTarget(RunSingleTargetParameters),
    /// Run all targets in dependency order.
    AllTargets(RunAllTargetsParameters),
}

/// Parameters for the `task run` subcommand.
#[derive(Parser, Debug, Clone)]
pub struct TaskRunParameters {
    /// The `task run` subcommand to run.
    #[clap(subcommand)]
    pub sub_command: TaskRunSubCommand,
}

/// Parameters for rewinding (undoing) the last completed statement of a task.
#[derive(Parser, Debug, Clone)]
pub struct RewindSingleStepParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
}

/// Parameters for rewinding the last completed target of a task.
#[derive(Parser, Debug, Clone)]
pub struct RewindSingleTargetParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
}

/// Parameters for rewinding all execution state of a task.
#[derive(Parser, Debug, Clone)]
pub struct RewindAllTargetsParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
}

/// The `task rewind` subcommand.
#[derive(Parser, Debug, Clone)]
pub enum TaskRewindSubCommand {
    /// Rewind the last completed statement.
    SingleStep(RewindSingleStepParameters),
    /// Rewind the last completed target.
    SingleTarget(RewindSingleTargetParameters),
    /// Rewind all execution state.
    AllTargets(RewindAllTargetsParameters),
}

/// Parameters for the `task rewind` subcommand.
#[derive(Parser, Debug, Clone)]
pub struct TaskRewindParameters {
    /// The `task rewind` subcommand to run.
    #[clap(subcommand)]
    pub sub_command: TaskRewindSubCommand,
}

/// Parameters for releasing a wait barrier in a task.
#[derive(Parser, Debug, Clone)]
pub struct ContinueBarrierParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
    /// Cursor path of the wait barrier to release (e.g. `w0/s2/`).
    #[clap(long)]
    pub cursor: String,
}

/// The `task` subcommand.
#[derive(Parser, Debug, Clone)]
pub enum TaskSubCommand {
    /// List all tasks.
    List,
    /// Create a new task.
    Create(CreateTaskParameters),
    /// Remove a task.
    Remove(RemoveTaskParameters),
    /// Describe a task and its current execution status.
    Describe(DescribeTaskParameters),
    /// Run a task.
    Run(TaskRunParameters),
    /// Rewind a task.
    Rewind(TaskRewindParameters),
    /// Release a wait barrier so execution can continue past it.
    Continue(ContinueBarrierParameters),
}

/// Parameters for removing a task.
#[derive(Parser, Debug, Clone)]
pub struct RemoveTaskParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
}

/// Parameters for describing a task and its current execution status.
#[derive(Parser, Debug, Clone)]
pub struct DescribeTaskParameters {
    /// The name of the task.
    #[clap(long)]
    pub name: String,
}

/// Parameters for the `task` top-level subcommand.
#[derive(Parser, Debug, Clone)]
pub struct TaskParameters {
    /// The `task` subcommand to run.
    #[clap(subcommand)]
    pub sub_command: TaskSubCommand,
}

// ── Program statement helpers ──────────────────────────────────────────────────

/// Returns the workspace statement slice from the first `for workspace` block
/// in the program, or an empty slice if there is none.
fn first_workspace_stmts(program: &Program) -> &[WorkspaceStatement] {
    program
        .statements
        .iter()
        .find_map(|s| {
            if let GlobalStatement::ForWorkspace(b) = s {
                Some(b.statements.as_slice())
            } else {
                None
            }
        })
        .unwrap_or(&[])
}

/// Returns the crate statement slice from the first `for crate` block
/// in the program, or an empty slice if there is none.
fn first_crate_stmts(program: &Program) -> &[CrateStatement] {
    program
        .statements
        .iter()
        .find_map(|s| {
            if let GlobalStatement::ForCrate(b) = s {
                Some(b.statements.as_slice())
            } else {
                None
            }
        })
        .unwrap_or(&[])
}

// ── Statement completion checks ────────────────────────────────────────────────

/// Sentinel written to `exit_status` by [`execute_run_step`] when the
/// asciinema wrapper itself failed to launch (binary missing, fork
/// failure, etc.).  The wrapper script never gets a chance to write its
/// own exit code, so this explicit marker distinguishes "failed before
/// running" from a real non-zero exit.
const EXEC_FAILED_MARKER: &str = "exec failed";

/// Classification of a `run` step's `exit_status` file contents.
#[derive(Debug, PartialEq, Eq)]
enum RunOutcome {
    /// No `state_dir` on disk, or no `exit_status` file inside it.
    NotStarted,
    /// `exit_status` trimmed equals `"0"`.
    Zero,
    /// `exit_status` trimmed parses as a non-zero integer.
    NonZero(i32),
    /// `exit_status` trimmed equals [`EXEC_FAILED_MARKER`].
    ExecFailed,
}

/// Read and classify the `exit_status` file at `state_dir`.
///
/// # Errors
///
/// Returns [`Error::CouldNotReadStateFile`] if the file exists but cannot be
/// read (permission errors, etc.) and [`Error::InvalidRecordedExitStatus`]
/// if the trimmed contents are neither `"0"`, a valid `i32`, nor
/// [`EXEC_FAILED_MARKER`].  A missing `state_dir` or a missing
/// `exit_status` file inside an existing `state_dir` is **not** an error
/// — those cases return `Ok(RunOutcome::NotStarted)`.
fn read_run_outcome(state_dir: &Path) -> Result<RunOutcome, Error> {
    if !state_dir.exists() {
        return Ok(RunOutcome::NotStarted);
    }
    let exit_status_path = state_dir.join("exit_status");
    let raw = match fs_err::read_to_string(&exit_status_path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RunOutcome::NotStarted);
        }
        Err(err) => return Err(Error::CouldNotReadStateFile(exit_status_path, err)),
    };
    let trimmed = raw.trim();
    if trimmed == "0" {
        return Ok(RunOutcome::Zero);
    }
    if trimmed == EXEC_FAILED_MARKER {
        return Ok(RunOutcome::ExecFailed);
    }
    match trimmed.parse::<i32>() {
        Ok(code) => Ok(RunOutcome::NonZero(code)),
        Err(_) => Err(Error::InvalidRecordedExitStatus(raw)),
    }
}

/// Returns `Ok(true)` if the `run` statement recorded at `state_dir`
/// succeeded (its `exit_status` file contains `"0"`).
///
/// Returns `Ok(false)` for every other "clean" classification — step not
/// started, step ran but exited non-zero, step's wrapper failed to
/// launch.  Returns `Err` only when the `exit_status` file exists but
/// its contents are unparsable (see [`read_run_outcome`]).
fn is_run_completed(state_dir: &Path) -> Result<bool, Error> {
    Ok(matches!(read_run_outcome(state_dir)?, RunOutcome::Zero))
}

/// Returns `Ok(true)` if the `run` step at `state_dir` has a recorded
/// failure: either a non-zero parsed integer or the
/// [`EXEC_FAILED_MARKER`] sentinel written when the wrapper itself
/// could not launch.
///
/// Returns `Ok(false)` for "succeeded" (`"0"`) and for "never started"
/// (missing state dir or missing `exit_status` file).  Returns `Err`
/// only when the `exit_status` file exists but its contents are
/// unparsable (see [`read_run_outcome`]).
fn is_run_failed(state_dir: &Path) -> Result<bool, Error> {
    Ok(matches!(
        read_run_outcome(state_dir)?,
        RunOutcome::NonZero(_) | RunOutcome::ExecFailed,
    ))
}

/// Returns `true` if the `manual_step` at `state_dir` was confirmed by the user.
fn is_manual_completed(state_dir: &Path) -> bool {
    if !state_dir.exists() {
        return false;
    }
    fs_err::read_to_string(state_dir.join("manual_step_confirmed"))
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("y")
}

/// Returns `true` if the `snapshot_metadata` step at `state_dir` has completed.
fn is_snapshot_metadata_completed(state_dir: &Path) -> bool {
    state_dir.exists() && state_dir.join("snapshot_metadata_completed").exists()
}

/// A `wait_for_continue` barrier has three on-disk states, distinguished by
/// the presence of `state_dir` and the `barrier_released` marker inside it:
///
/// - **pending**: `state_dir` does not exist yet. The barrier has not been
///   reached during execution.
/// - **waiting**: `state_dir` exists but `barrier_released` does not. The
///   executor has reached the barrier and is now blocked, waiting for
///   `task continue` to release it.
/// - **released**: `barrier_released` exists. The user has run
///   `task continue` and the executor may proceed past this barrier.
///
/// Returns `true` only for the *waiting* state.
fn is_wait_barrier_waiting(state_dir: &Path) -> bool {
    state_dir.exists() && !state_dir.join("barrier_released").exists()
}

/// Returns `true` if the `wait_for_continue` barrier at `state_dir` is in the
/// *released* state (see [`is_wait_barrier_waiting`] for the full tri-state
/// description).
fn is_wait_barrier_released(state_dir: &Path) -> bool {
    state_dir.join("barrier_released").exists()
}

/// Reads the `chosen_branch` marker for the given `if`-block state directory.
///
/// Returns `Ok(None)` only for the *NotFound* case — the marker has not been
/// written yet, so no branch was chosen. Any other I/O error (permission
/// denied, ENOTDIR, invalid UTF-8, etc.) propagates as
/// [`Error::CouldNotReadStateFile`]. Distinguishing the two prevents a
/// transient glitch from silently re-running the if-block's branch
/// evaluation, which can pick a different branch than originally chosen
/// (because `ask_user` and `run`-style conditions are not deterministic) and
/// overwrite the on-disk `chosen_branch` file.
fn read_chosen_branch(state_dir: &Path) -> Result<Option<String>, Error> {
    let path = state_dir.join("chosen_branch");
    match fs_err::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::CouldNotReadStateFile(path, e)),
    }
}

/// Returns `Ok(true)` if all crate statements in `stmts` under `prefix` are
/// completed; `Ok(false)` if any are incomplete; `Err(...)` only on a real
/// I/O failure reading state files (a missing marker is not an error — it
/// just means "not completed").
fn is_crate_stmts_completed(
    stmts: &[CrateStatement],
    prefix: &ProgramCursor,
    state_base: &Path,
) -> Result<bool, Error> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        if !is_crate_stmt_completed(stmt, &cursor, state_base)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Returns `Ok(true)` if the given crate statement at `cursor` is completed;
/// `Ok(false)` if not; `Err(...)` only on a real I/O failure (not on a
/// missing state marker).
fn is_crate_stmt_completed(
    stmt: &CrateStatement,
    cursor: &ProgramCursor,
    state_base: &Path,
) -> Result<bool, Error> {
    let state_dir = state_base.join(cursor.to_path());
    match stmt {
        CrateStatement::Run(_) => is_run_completed(&state_dir),
        CrateStatement::ManualStep(_) => Ok(is_manual_completed(&state_dir)),
        CrateStatement::SnapshotMetadata(_) => Ok(is_snapshot_metadata_completed(&state_dir)),
        CrateStatement::If(block) => {
            let Some(chosen) = read_chosen_branch(&state_dir)? else {
                return Ok(false);
            };
            match chosen.trim() {
                "none" => Ok(true),
                "else" => {
                    let p = cursor.clone().with(CursorSegment::ElseBranch);
                    is_crate_stmts_completed(&block.else_statements, &p, state_base)
                }
                s => match s.parse::<usize>() {
                    Ok(n) => match block.branches.get(n) {
                        Some(branch) => {
                            let p = cursor.clone().with(CursorSegment::IfBranch(n));
                            is_crate_stmts_completed(&branch.statements, &p, state_base)
                        }
                        None => Ok(false),
                    },
                    Err(_) => Ok(false),
                },
            }
        }
        CrateStatement::WithEnvFile(block) => {
            let p = cursor.clone().with(CursorSegment::WithEnvFile);
            is_crate_stmts_completed(&block.statements, &p, state_base)
        }
        CrateStatement::WaitForContinue(_) => Ok(is_wait_barrier_released(&state_dir)),
    }
}

/// Returns `Ok(true)` if all workspace statements in `stmts` under `prefix`
/// are completed; `Ok(false)` if any are incomplete; `Err(...)` only on a
/// real I/O failure.
///
/// `member_crates` is required to evaluate `ForCrateInWorkspace` blocks.
fn is_workspace_stmts_completed(
    stmts: &[WorkspaceStatement],
    prefix: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
) -> Result<bool, Error> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        if !is_workspace_stmt_completed(stmt, &cursor, member_crates, state_base)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Returns `Ok(true)` if the given workspace statement at `cursor` is
/// completed; `Ok(false)` if not; `Err(...)` only on a real I/O failure.
fn is_workspace_stmt_completed(
    stmt: &WorkspaceStatement,
    cursor: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
) -> Result<bool, Error> {
    let state_dir = state_base.join(cursor.to_path());
    match stmt {
        WorkspaceStatement::Run(_) => is_run_completed(&state_dir),
        WorkspaceStatement::ManualStep(_) => Ok(is_manual_completed(&state_dir)),
        WorkspaceStatement::SnapshotMetadata(_) => Ok(is_snapshot_metadata_completed(&state_dir)),
        WorkspaceStatement::If(block) => {
            let Some(chosen) = read_chosen_branch(&state_dir)? else {
                return Ok(false);
            };
            match chosen.trim() {
                "none" => Ok(true),
                "else" => {
                    let p = cursor.clone().with(CursorSegment::ElseBranch);
                    is_workspace_stmts_completed(
                        &block.else_statements,
                        &p,
                        member_crates,
                        state_base,
                    )
                }
                s => match s.parse::<usize>() {
                    Ok(n) => match block.branches.get(n) {
                        Some(branch) => {
                            let p = cursor.clone().with(CursorSegment::IfBranch(n));
                            is_workspace_stmts_completed(
                                &branch.statements,
                                &p,
                                member_crates,
                                state_base,
                            )
                        }
                        None => Ok(false),
                    },
                    Err(_) => Ok(false),
                },
            }
        }
        WorkspaceStatement::WithEnvFile(block) => {
            let p = cursor.clone().with(CursorSegment::WithEnvFile);
            is_workspace_stmts_completed(&block.statements, &p, member_crates, state_base)
        }
        WorkspaceStatement::ForCrateInWorkspace(block) => {
            for (c_idx, _) in member_crates.iter().enumerate() {
                let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
                if !is_crate_stmts_completed(&block.statements, &c_prefix, state_base)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        WorkspaceStatement::WaitForContinue(_) => Ok(is_wait_barrier_released(&state_dir)),
    }
}

/// Returns `Ok(true)` if all workspace statements for `ws_idx` are completed.
fn is_workspace_completed(
    ws_idx: usize,
    ws_exec: &ResolvedWorkspaceExecution,
    ws_stmts: &[WorkspaceStatement],
    state_base: &Path,
) -> Result<bool, Error> {
    let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
    is_workspace_stmts_completed(ws_stmts, &prefix, &ws_exec.member_crates, state_base)
}

/// Returns `Ok(true)` if all statements for standalone crate `c_idx` are completed.
fn is_standalone_crate_completed(
    c_idx: usize,
    crate_stmts: &[CrateStatement],
    state_base: &Path,
) -> Result<bool, Error> {
    let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
    is_crate_stmts_completed(crate_stmts, &prefix, state_base)
}

/// Returns `Ok(true)` if all inter-workspace dependencies of `ws_exec` are completed.
fn are_workspace_deps_completed(
    ws_exec: &ResolvedWorkspaceExecution,
    ws_map: &HashMap<PathBuf, usize>,
    ws_stmts: &[WorkspaceStatement],
    resolved: &ResolvedProgram,
    state_base: &Path,
) -> Result<bool, Error> {
    for dep_path in &ws_exec.dependencies {
        let Some(&dep_idx) = ws_map.get(dep_path) else {
            continue; // Dep not in selected set — treat as satisfied.
        };
        let Some(dep_exec) = resolved.workspace_executions.get(dep_idx) else {
            continue;
        };
        if !is_workspace_completed(dep_idx, dep_exec, ws_stmts, state_base)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Returns `Ok(true)` if all dependencies of a standalone crate have completed.
///
/// `crate_map` indexes only the standalone crates in this program's
/// `for crate { … }` set; a `dep_path` that isn't in the map points
/// outside that set (typically a workspace member, or a crate that the
/// program's `select crates` filter excluded). Such out-of-set deps are
/// deliberately skipped here — the workspace-level dep gate in the
/// outer iteration owns enforcement for them. See [`are_member_crate_deps_completed`]
/// for the symmetric reasoning on the `for crate in workspace` side.
fn are_standalone_crate_deps_completed(
    crate_exec: &ResolvedCrateExecution,
    crate_map: &HashMap<PathBuf, usize>,
    crate_stmts: &[CrateStatement],
    state_base: &Path,
) -> Result<bool, Error> {
    for dep_path in &crate_exec.dependencies {
        // Intentional silent skip — see function doc-comment above.
        let Some(&dep_idx) = crate_map.get(dep_path) else {
            continue;
        };
        if !is_standalone_crate_completed(dep_idx, crate_stmts, state_base)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Returns `Ok(true)` if all intra-workspace dependencies of a member crate
/// are completed for the given `for crate in workspace` block.
///
/// `crate_map` indexes only the *current workspace's* member crates. A
/// member whose `dependencies` list points at a crate in a different
/// workspace (or at a standalone crate outside the workspace) is
/// deliberately skipped here: the workspace-level dep gate already
/// blocks this workspace from starting until every workspace it depends
/// on has completed, so by the time we walk member-level deps every
/// cross-workspace dep is guaranteed to be done. Checking it again at
/// crate granularity would be redundant and would require threading the
/// full program-wide crate index into this helper.
fn are_member_crate_deps_completed(
    crate_exec: &ResolvedCrateExecution,
    crate_map: &HashMap<PathBuf, usize>,
    for_crate_prefix: &ProgramCursor,
    for_crate_stmts: &[CrateStatement],
    state_base: &Path,
) -> Result<bool, Error> {
    for dep_path in &crate_exec.dependencies {
        // Intentional silent skip — see function doc-comment above.
        let Some(&dep_idx) = crate_map.get(dep_path) else {
            continue;
        };
        let c_prefix = for_crate_prefix
            .clone()
            .with(CursorSegment::CrateIteration(dep_idx));
        if !is_crate_stmts_completed(for_crate_stmts, &c_prefix, state_base)? {
            return Ok(false);
        }
    }
    Ok(true)
}

// ── Find-next helpers ──────────────────────────────────────────────────────────

/// The concrete action to take for a [`NextStatement`].
#[derive(Debug)]
pub enum StatementAction<'a> {
    /// Execute a command in the target directory.
    RunCommand(&'a RunStep),
    /// Pause for a manual user action and confirm completion.
    ManualStep(&'a ManualStepNode),
    /// Evaluate the branch conditions of a workspace `if` block.
    EvaluateWorkspaceIf(&'a WorkspaceIfBlock),
    /// Evaluate the branch conditions of a crate `if` block.
    EvaluateCrateIf(&'a CrateIfBlock),
    /// Capture and store cargo metadata under the given name.
    SnapshotMetadata(&'a SnapshotMetadataNode),
    /// A wait barrier: pending → create state_dir and print message; released → skip.
    WaitForContinue(&'a WaitForContinueNode),
}

/// The next statement that should be executed in a running task.
#[derive(Debug)]
pub struct NextStatement<'a> {
    /// Cursor identifying this statement in the execution tree.
    pub cursor: ProgramCursor,
    /// The directory in which the statement executes.
    pub manifest_dir: &'a Path,
    /// What to do at this cursor position.
    pub action: StatementAction<'a>,
    /// Env file paths from enclosing `with_env_file` blocks, ordered from outermost to
    /// innermost. Each path is either relative to `manifest_dir` or absolute.
    pub env_file_paths: Vec<String>,
}

/// Result of searching a scope for the next executable statement.
///
/// Distinguishes "no further executable statement because everything is done"
/// from "no further executable statement because a `wait_for_continue` barrier
/// is blocking progress". Callers must propagate `Suspended` upward so that
/// statements following a barrier-blocked scope do not run prematurely.
#[derive(Debug)]
pub enum NextOutcome<'a> {
    /// Every statement in the scope is complete.
    Done,
    /// At least one cursor in the scope is blocked at an unreleased
    /// `wait_for_continue` barrier, and no executable statement is available
    /// in the scope without first releasing it.
    Suspended,
    /// The next statement that should be executed.
    Next(NextStatement<'a>),
}

/// Returns `Some(Next(action))` when a leaf statement at `cursor` is not yet
/// complete, otherwise `None` so the caller advances to the next statement.
/// Shared by the crate and workspace `find_next` walks.
fn next_if_incomplete<'a>(
    completed: bool,
    cursor: ProgramCursor,
    manifest_dir: &'a Path,
    action: StatementAction<'a>,
    env_file_paths: &[String],
) -> Option<NextOutcome<'a>> {
    if completed {
        None
    } else {
        Some(NextOutcome::Next(NextStatement {
            cursor,
            manifest_dir,
            action,
            env_file_paths: env_file_paths.to_vec(),
        }))
    }
}

/// Folds the [`NextOutcome`] of a nested scope into the enclosing walk.
///
/// Returns `Some(outcome)` when the walk should short-circuit and return it (a
/// concrete `Next` action was found). A nested `Suspended` records the
/// suspension in `suspended` and yields `None` so the walk keeps scanning later
/// statements; a nested `Done` is skipped.
fn fold_nested<'a>(nested: NextOutcome<'a>, suspended: &mut bool) -> Option<NextOutcome<'a>> {
    match nested {
        NextOutcome::Next(_) => Some(nested),
        NextOutcome::Suspended => {
            *suspended = true;
            None
        }
        NextOutcome::Done => None,
    }
}

/// Resolves a `wait_for_continue` barrier at `state_dir` into the walk's next
/// step: `None` when already released (fall through to the next statement),
/// `Some(Suspended)` when waiting, or `Some(Next(..))` when pending (not yet
/// reached). Shared by the crate and workspace walks.
fn barrier_next<'a>(
    state_dir: &Path,
    cursor: ProgramCursor,
    manifest_dir: &'a Path,
    node: &'a WaitForContinueNode,
    env_file_paths: &[String],
) -> Option<NextOutcome<'a>> {
    if is_wait_barrier_released(state_dir) {
        None
    } else if is_wait_barrier_waiting(state_dir) {
        Some(NextOutcome::Suspended)
    } else {
        Some(NextOutcome::Next(NextStatement {
            cursor,
            manifest_dir,
            action: StatementAction::WaitForContinue(node),
            env_file_paths: env_file_paths.to_vec(),
        }))
    }
}

/// Resolves the next outcome for a crate `if` block: when no branch has been
/// chosen yet, surfaces the `EvaluateCrateIf` action; otherwise descends into
/// the chosen branch (`Done` for "none" or an out-of-range index).
fn find_next_in_crate_if<'a>(
    block: &'a CrateIfBlock,
    cursor: &ProgramCursor,
    state_dir: &Path,
    manifest_dir: &'a Path,
    state_base: &Path,
    env_file_paths: &[String],
) -> Result<NextOutcome<'a>, Error> {
    let Some(chosen) = read_chosen_branch(state_dir)? else {
        return Ok(NextOutcome::Next(NextStatement {
            cursor: cursor.clone(),
            manifest_dir,
            action: StatementAction::EvaluateCrateIf(block),
            env_file_paths: env_file_paths.to_vec(),
        }));
    };
    match chosen.trim() {
        "none" => Ok(NextOutcome::Done),
        "else" => find_next_in_crate_stmts(
            &block.else_statements,
            &cursor.clone().with(CursorSegment::ElseBranch),
            manifest_dir,
            state_base,
            env_file_paths,
        ),
        s => match s.parse::<usize>() {
            Ok(n) => match block.branches.get(n) {
                Some(branch) => find_next_in_crate_stmts(
                    &branch.statements,
                    &cursor.clone().with(CursorSegment::IfBranch(n)),
                    manifest_dir,
                    state_base,
                    env_file_paths,
                ),
                None => Ok(NextOutcome::Done),
            },
            Err(_) => Ok(NextOutcome::Done),
        },
    }
}

/// Resolves the next outcome for a workspace `if` block. See
/// [`find_next_in_crate_if`] for the branch-selection semantics.
fn find_next_in_workspace_if<'a>(
    block: &'a WorkspaceIfBlock,
    cursor: &ProgramCursor,
    state_dir: &Path,
    manifest_dir: &'a Path,
    member_crates: &'a [ResolvedCrateExecution],
    state_base: &Path,
    env_file_paths: &[String],
) -> Result<NextOutcome<'a>, Error> {
    let Some(chosen) = read_chosen_branch(state_dir)? else {
        return Ok(NextOutcome::Next(NextStatement {
            cursor: cursor.clone(),
            manifest_dir,
            action: StatementAction::EvaluateWorkspaceIf(block),
            env_file_paths: env_file_paths.to_vec(),
        }));
    };
    match chosen.trim() {
        "none" => Ok(NextOutcome::Done),
        "else" => find_next_in_workspace_stmts(
            &block.else_statements,
            &cursor.clone().with(CursorSegment::ElseBranch),
            manifest_dir,
            member_crates,
            state_base,
            env_file_paths,
        ),
        s => match s.parse::<usize>() {
            Ok(n) => match block.branches.get(n) {
                Some(branch) => find_next_in_workspace_stmts(
                    &branch.statements,
                    &cursor.clone().with(CursorSegment::IfBranch(n)),
                    manifest_dir,
                    member_crates,
                    state_base,
                    env_file_paths,
                ),
                None => Ok(NextOutcome::Done),
            },
            Err(_) => Ok(NextOutcome::Done),
        },
    }
}

/// Resolves the next outcome for a `for crate in workspace` block, honouring
/// intra-workspace dependency ordering.
///
/// A member whose intra-workspace deps are not yet complete is skipped and
/// marks the block suspended (the dep will surface its own action or barrier
/// when its iteration is reached). A suspended block must not be walked past —
/// see the call site in [`find_next_in_workspace_stmts`].
fn find_next_in_for_crate<'a>(
    block: &'a ForCrateInWorkspaceBlock,
    cursor: &ProgramCursor,
    member_crates: &'a [ResolvedCrateExecution],
    state_base: &Path,
    env_file_paths: &[String],
) -> Result<NextOutcome<'a>, Error> {
    let crate_map: HashMap<PathBuf, usize> = member_crates
        .iter()
        .enumerate()
        .map(|(ci, c)| (c.manifest_dir.clone(), ci))
        .collect();

    let mut block_suspended = false;
    for (c_idx, crate_exec) in member_crates.iter().enumerate() {
        if !are_member_crate_deps_completed(
            crate_exec,
            &crate_map,
            cursor,
            &block.statements,
            state_base,
        )? {
            block_suspended = true;
            continue;
        }
        let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
        let nested = find_next_in_crate_stmts(
            &block.statements,
            &c_prefix,
            &crate_exec.manifest_dir,
            state_base,
            env_file_paths,
        )?;
        if let Some(outcome) = fold_nested(nested, &mut block_suspended) {
            return Ok(outcome);
        }
    }
    Ok(if block_suspended {
        NextOutcome::Suspended
    } else {
        NextOutcome::Done
    })
}

/// Finds the first uncompleted crate statement in `stmts` starting at `prefix`.
///
/// Returns [`NextOutcome::Done`] if every statement is complete,
/// [`NextOutcome::Suspended`] if a `wait_for_continue` barrier in this scope
/// (or in a nested scope) is in the *waiting* state with no executable
/// statement available before it, or [`NextOutcome::Next`] with the next
/// action. Errors are returned only for real I/O failures reading state
/// files; a missing `chosen_branch` (i.e. not yet written) is treated as
/// "evaluate the if-block now".
fn find_next_in_crate_stmts<'a>(
    stmts: &'a [CrateStatement],
    prefix: &ProgramCursor,
    manifest_dir: &'a Path,
    state_base: &Path,
    env_file_paths: &[String],
) -> Result<NextOutcome<'a>, Error> {
    let mut suspended = false;
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            CrateStatement::Run(step) => {
                if let Some(outcome) = next_if_incomplete(
                    is_run_completed(&state_dir)?,
                    cursor,
                    manifest_dir,
                    StatementAction::RunCommand(step),
                    env_file_paths,
                ) {
                    return Ok(outcome);
                }
            }
            CrateStatement::ManualStep(step) => {
                if let Some(outcome) = next_if_incomplete(
                    is_manual_completed(&state_dir),
                    cursor,
                    manifest_dir,
                    StatementAction::ManualStep(step),
                    env_file_paths,
                ) {
                    return Ok(outcome);
                }
            }
            CrateStatement::SnapshotMetadata(step) => {
                if let Some(outcome) = next_if_incomplete(
                    is_snapshot_metadata_completed(&state_dir),
                    cursor,
                    manifest_dir,
                    StatementAction::SnapshotMetadata(step),
                    env_file_paths,
                ) {
                    return Ok(outcome);
                }
            }
            CrateStatement::If(block) => {
                let nested = find_next_in_crate_if(
                    block,
                    &cursor,
                    &state_dir,
                    manifest_dir,
                    state_base,
                    env_file_paths,
                )?;
                if let Some(outcome) = fold_nested(nested, &mut suspended) {
                    return Ok(outcome);
                }
            }
            CrateStatement::WithEnvFile(block) => {
                let inner_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                let mut inner_env_files = env_file_paths.to_vec();
                inner_env_files.push(block.env_file.clone());
                let nested = find_next_in_crate_stmts(
                    &block.statements,
                    &inner_prefix,
                    manifest_dir,
                    state_base,
                    &inner_env_files,
                )?;
                if let Some(outcome) = fold_nested(nested, &mut suspended) {
                    return Ok(outcome);
                }
            }
            CrateStatement::WaitForContinue(node) => {
                if let Some(outcome) =
                    barrier_next(&state_dir, cursor, manifest_dir, node, env_file_paths)
                {
                    return Ok(outcome);
                }
            }
        }
    }
    Ok(if suspended {
        NextOutcome::Suspended
    } else {
        NextOutcome::Done
    })
}

/// Finds the first uncompleted workspace statement in `stmts` starting at `prefix`.
///
/// Returns [`NextOutcome::Done`] when every statement (including nested
/// `for crate in workspace`) is complete, [`NextOutcome::Suspended`] when a
/// nested barrier is blocking progress, or [`NextOutcome::Next`] with the
/// next action.
fn find_next_in_workspace_stmts<'a>(
    stmts: &'a [WorkspaceStatement],
    prefix: &ProgramCursor,
    manifest_dir: &'a Path,
    member_crates: &'a [ResolvedCrateExecution],
    state_base: &Path,
    env_file_paths: &[String],
) -> Result<NextOutcome<'a>, Error> {
    let mut suspended = false;
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            WorkspaceStatement::Run(step) => {
                if let Some(outcome) = next_if_incomplete(
                    is_run_completed(&state_dir)?,
                    cursor,
                    manifest_dir,
                    StatementAction::RunCommand(step),
                    env_file_paths,
                ) {
                    return Ok(outcome);
                }
            }
            WorkspaceStatement::ManualStep(step) => {
                if let Some(outcome) = next_if_incomplete(
                    is_manual_completed(&state_dir),
                    cursor,
                    manifest_dir,
                    StatementAction::ManualStep(step),
                    env_file_paths,
                ) {
                    return Ok(outcome);
                }
            }
            WorkspaceStatement::SnapshotMetadata(step) => {
                if let Some(outcome) = next_if_incomplete(
                    is_snapshot_metadata_completed(&state_dir),
                    cursor,
                    manifest_dir,
                    StatementAction::SnapshotMetadata(step),
                    env_file_paths,
                ) {
                    return Ok(outcome);
                }
            }
            WorkspaceStatement::If(block) => {
                let nested = find_next_in_workspace_if(
                    block,
                    &cursor,
                    &state_dir,
                    manifest_dir,
                    member_crates,
                    state_base,
                    env_file_paths,
                )?;
                if let Some(outcome) = fold_nested(nested, &mut suspended) {
                    return Ok(outcome);
                }
            }
            WorkspaceStatement::WithEnvFile(block) => {
                let inner_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                let mut inner_env_files = env_file_paths.to_vec();
                inner_env_files.push(block.env_file.clone());
                let nested = find_next_in_workspace_stmts(
                    &block.statements,
                    &inner_prefix,
                    manifest_dir,
                    member_crates,
                    state_base,
                    &inner_env_files,
                )?;
                if let Some(outcome) = fold_nested(nested, &mut suspended) {
                    return Ok(outcome);
                }
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                // A suspended `for crate in workspace` block must not be walked
                // past: downstream workspace statements may depend on work its
                // members will do once a barrier is released. So both `Next`
                // and `Suspended` short-circuit here; only `Done` falls through.
                match find_next_in_for_crate(
                    block,
                    &cursor,
                    member_crates,
                    state_base,
                    env_file_paths,
                )? {
                    NextOutcome::Done => {}
                    outcome => return Ok(outcome),
                }
            }
            WorkspaceStatement::WaitForContinue(node) => {
                if let Some(outcome) =
                    barrier_next(&state_dir, cursor, manifest_dir, node, env_file_paths)
                {
                    return Ok(outcome);
                }
            }
        }
    }
    Ok(if suspended {
        NextOutcome::Suspended
    } else {
        NextOutcome::Done
    })
}

/// Finds the next uncompleted statement across all workspaces and standalone crates,
/// respecting inter-target dependency ordering.
///
/// Returns [`NextOutcome::Done`] when every statement in every target has
/// completed, [`NextOutcome::Suspended`] when no statement is currently
/// executable because at least one target is blocked at a `wait_for_continue`
/// barrier (or transitively blocked by one), or [`NextOutcome::Next`] with
/// the next action.
///
/// # Errors
///
/// Returns an error only for real I/O failures reading state files (e.g.
/// permission denied on a `chosen_branch` marker). A missing marker is
/// treated as "evaluate the if-block now", not as an error.
pub fn find_next_statement<'a>(
    program: &'a Program,
    resolved: &'a ResolvedProgram,
    state_base: &Path,
) -> Result<NextOutcome<'a>, Error> {
    let mut suspended = false;
    let ws_stmts = first_workspace_stmts(program);
    let ws_map: HashMap<PathBuf, usize> = resolved
        .workspace_executions
        .iter()
        .enumerate()
        .map(|(i, w)| (w.manifest_dir.clone(), i))
        .collect();

    for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate() {
        if !are_workspace_deps_completed(ws_exec, &ws_map, ws_stmts, resolved, state_base)? {
            // Upstream dep not yet complete; the dep itself will surface its
            // own outcome on its iteration.
            suspended = true;
            continue;
        }
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
        let next = find_next_in_workspace_stmts(
            ws_stmts,
            &prefix,
            &ws_exec.manifest_dir,
            &ws_exec.member_crates,
            state_base,
            &[],
        )?;
        match next {
            NextOutcome::Next(_) => return Ok(next),
            NextOutcome::Suspended => suspended = true,
            NextOutcome::Done => {}
        }
    }

    let crate_stmts = first_crate_stmts(program);
    let crate_map: HashMap<PathBuf, usize> = resolved
        .crate_executions
        .iter()
        .enumerate()
        .map(|(i, c)| (c.manifest_dir.clone(), i))
        .collect();

    for (c_idx, crate_exec) in resolved.crate_executions.iter().enumerate() {
        if !are_standalone_crate_deps_completed(crate_exec, &crate_map, crate_stmts, state_base)? {
            suspended = true;
            continue;
        }
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
        let next = find_next_in_crate_stmts(
            crate_stmts,
            &prefix,
            &crate_exec.manifest_dir,
            state_base,
            &[],
        )?;
        match next {
            NextOutcome::Next(_) => return Ok(next),
            NextOutcome::Suspended => suspended = true,
            NextOutcome::Done => {}
        }
    }

    Ok(if suspended {
        NextOutcome::Suspended
    } else {
        NextOutcome::Done
    })
}

// ── Statement execution ────────────────────────────────────────────────────────

/// Expands `${name.field}` interpolations in `s` using named metadata snapshots.
///
/// Each `${name.field1.field2...}` reference is replaced with the value of the
/// given field path in the current crate's package entry within the named snapshot.
/// If `s` contains no `${` sequences, it is returned unchanged without any
/// filesystem access.
///
/// # Errors
///
/// Returns an error if any interpolation reference is malformed (e.g. missing
/// the closing `}` or the dot-separated field), if the named snapshot does not
/// exist, if the current crate's package cannot be found in the snapshot, or if
/// the given field path does not exist in the package.
fn expand_interpolations(s: &str, manifest_dir: &Path, state_base: &Path) -> Result<String, Error> {
    if !s.contains("${") {
        return Ok(s.to_owned());
    }
    let mut result = String::with_capacity(s.len());
    let mut parts = s.split("${");
    if let Some(first) = parts.next() {
        result.push_str(first);
    }
    for part in parts {
        let (reference, rest) = part
            .split_once('}')
            .ok_or_else(|| Error::InvalidInterpolation(format!("${{{part}")))?;
        let (name, field_path) = reference
            .split_once('.')
            .ok_or_else(|| Error::InvalidInterpolation(reference.to_owned()))?;
        let value = resolve_interpolation(name, field_path, manifest_dir, state_base)?;
        result.push_str(&value);
        result.push_str(rest);
    }
    Ok(result)
}

/// Derives a stable filename key from a manifest directory for snapshot storage.
///
/// Canonicalizes the directory (so `/foo/bar` and `/foo/./bar` collapse to the
/// same key, and symlinks resolve to their target) and hex-encodes the raw
/// bytes of the resulting `OsStr` (so non-UTF-8 paths cannot collide via
/// lossy replacement characters as `to_string_lossy` would produce).
fn manifest_hex_key(manifest_dir: &Path) -> Result<String, Error> {
    const HEX_DIGITS: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let canonical = fs_err::canonicalize(manifest_dir).map_err(|err| {
        Error::CouldNotDetermineCanonicalManifestPath(manifest_dir.to_path_buf(), err)
    })?;
    let bytes = canonical.as_os_str().as_encoded_bytes();
    let mut hex = String::with_capacity(bytes.len().saturating_mul(2));
    // The high and low nibbles of a u8 are always in 0..16, so `HEX_DIGITS.get`
    // will never be `None` in practice. The `unwrap_or('0')` is a defensive
    // default solely to satisfy `clippy::indexing_slicing` / `unwrap_used`.
    for &b in bytes {
        let hi = usize::from(b.wrapping_shr(4));
        let lo = usize::from(b & 0x0F);
        hex.push(HEX_DIGITS.get(hi).copied().unwrap_or('0'));
        hex.push(HEX_DIGITS.get(lo).copied().unwrap_or('0'));
    }
    Ok(hex)
}

/// Looks up a single `${name.field_path}` reference and returns its string value.
///
/// Snapshots are scoped to the `manifest_dir` that ran the capturing step:
/// each context has its own `by_manifest/{hex}.json`. The package for the
/// current crate is found in the snapshot by matching its `manifest_path`
/// against `manifest_dir/Cargo.toml`, and the dot-separated `field_path` is
/// then navigated within that package's JSON.
///
/// # Errors
///
/// Returns an error if no snapshot named `snapshot_name` exists for this
/// context, if the current crate's package cannot be found in the snapshot,
/// or if `field_path` does not exist or is not navigable within the package
/// JSON.
fn resolve_interpolation(
    snapshot_name: &str,
    field_path: &str,
    manifest_dir: &Path,
    state_base: &Path,
) -> Result<String, Error> {
    let name_dir = state_base.join("snapshots").join(snapshot_name);
    let mut filename = manifest_hex_key(manifest_dir)?;
    filename.push_str(".json");
    let per_manifest_path = name_dir.join("by_manifest").join(&filename);
    if !per_manifest_path.exists() {
        return Err(Error::SnapshotNotFound(snapshot_name.to_owned()));
    }
    let json_path = per_manifest_path;
    let json = fs_err::read_to_string(&json_path).map_err(Error::IoError)?;
    let root: serde_json::Value =
        serde_json::from_str(&json).map_err(Error::CouldNotDeserializeMetadataSnapshot)?;
    let target_manifest = manifest_dir.join("Cargo.toml");
    let packages = root
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            Error::SnapshotPackageNotFound(snapshot_name.to_owned(), manifest_dir.to_path_buf())
        })?;
    let package = packages
        .iter()
        .find(|p| {
            p.get("manifest_path")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|mp| std::path::Path::new(mp) == target_manifest)
        })
        .ok_or_else(|| {
            Error::SnapshotPackageNotFound(snapshot_name.to_owned(), manifest_dir.to_path_buf())
        })?;
    let mut current: &serde_json::Value = package;
    for segment in field_path.split('.') {
        current = current.get(segment).ok_or_else(|| {
            Error::SnapshotFieldNotFound(snapshot_name.to_owned(), field_path.to_owned())
        })?;
    }
    Ok(match current {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// Captures cargo metadata for the workspace rooted at `manifest_dir` and
/// stores it under the name given in `step`.
///
/// The snapshot is written to
/// `state_base/snapshots/{name}/by_manifest/{hex_encoded_canonical_manifest_dir}.json`
/// for per-context lookup. A completion marker is written to
/// `state_dir/snapshot_metadata_completed`.
///
/// # Errors
///
/// Returns an error if `cargo metadata` fails, if the JSON cannot be serialized,
/// or if any filesystem operation fails.
#[expect(
    clippy::print_stdout,
    reason = "snapshot step announcement is part of the UI"
)]
async fn execute_snapshot_metadata_step(
    step: &SnapshotMetadataNode,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
) -> Result<(), Error> {
    println!("Snapshot metadata: {:?}", step.name);
    let state_dir = state_base.join(cursor.to_path());
    crate::utils::create_user_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_dir.join("Cargo.toml"))
        .exec()
        .map_err(|e| Error::CargoMetadataError(manifest_dir.to_path_buf(), e))?;
    let json = serde_json::to_string_pretty(&metadata)
        .map_err(Error::CouldNotSerializeMetadataSnapshot)?;
    let name_dir = state_base.join("snapshots").join(&step.name);
    let by_manifest_dir = name_dir.join("by_manifest");
    crate::utils::create_user_dir_all(&by_manifest_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(by_manifest_dir.clone(), e))?;
    let mut filename = manifest_hex_key(manifest_dir)?;
    filename.push_str(".json");
    let per_manifest_path = by_manifest_dir.join(&filename);
    crate::utils::write_user_file(&per_manifest_path, &json)
        .map_err(|e| Error::CouldNotWriteStateFile(per_manifest_path.clone(), e))?;
    let marker = state_dir.join("snapshot_metadata_completed");
    crate::utils::write_user_file(&marker, "done")
        .map_err(|e| Error::CouldNotWriteStateFile(marker.clone(), e))?;
    Ok(())
}

/// Executes a `run` step using asciinema for recording.
///
/// # Errors
///
/// Returns an error if the command is not found, if asciinema fails to launch,
/// or if the exit-status file cannot be written.
#[expect(
    clippy::print_stdout,
    reason = "printing the command is part of the UI"
)]
async fn execute_run_step(
    step: &RunStep,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
    environment: &Environment,
    extra_env: &[(String, String)],
) -> Result<(), Error> {
    let state_dir = state_base.join(cursor.to_path());
    crate::utils::create_user_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;

    let command = expand_interpolations(&step.command, manifest_dir, state_base)?;
    let args = step
        .args
        .iter()
        .map(|a| expand_interpolations(a, manifest_dir, state_base))
        .collect::<Result<Vec<_>, _>>()?;

    if !crate::utils::command_is_executable(&command, environment) {
        return Err(Error::CommandNotFound(command.clone()));
    }

    if command.contains('\0') || args.iter().any(|a| a.contains('\0')) {
        return Err(Error::InvalidCommandArg(command.clone()));
    }
    let command_str =
        shell_words::join(std::iter::once(command.as_str()).chain(args.iter().map(String::as_str)));

    println!("Running: {command_str}");

    let wrapper_path = state_dir.join("run_wrapper.sh");
    let exit_status_path = state_dir.join("exit_status");
    let script = format!(
        "#!/bin/sh\n{command_str}\nrc=$?\nprintf '%d' \"$rc\" > \"$CARGO_FOR_EACH_EXIT_STATUS_PATH\"\nexit \"$rc\"\n"
    );
    crate::utils::write_user_file(&wrapper_path, &script)
        .map_err(|e| Error::CouldNotWriteStateFile(wrapper_path.clone(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(0o700);
        fs_err::set_permissions(&wrapper_path, perms).map_err(Error::IoError)?;
    }

    let cast_path = state_dir.join("asciinema.cast");
    let mut cmd = Command::new("asciinema");
    cmd.arg("record").arg("--overwrite");
    if environment.suppress_subprocess_output {
        cmd.arg("--headless");
    }
    cmd.arg("-q")
        .arg("-c")
        .arg(wrapper_path.to_string_lossy().as_ref())
        .arg(&cast_path);
    cmd.env("CARGO_FOR_EACH_EXIT_STATUS_PATH", &exit_status_path);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.current_dir(manifest_dir);

    match crate::utils::execute_command(&mut cmd, environment, manifest_dir) {
        Err(e) => {
            // Leave a self-describing marker so a later `task list` /
            // `task describe` (or any re-read of state) can distinguish
            // "wrapper exited non-zero" from "wrapper never ran". The
            // error itself is propagated immediately below so the current
            // invocation reports the launch failure directly.
            crate::utils::write_user_file(&exit_status_path, EXEC_FAILED_MARKER)
                .map_err(|we| Error::CouldNotWriteStateFile(exit_status_path, we))?;
            Err(e)
        }
        Ok(_) => {
            // The wrapper.sh that asciinema ran writes the command's exit code
            // to `exit_status_path` before exiting. If we got Ok(_) the
            // wrapper completed, so the file should exist; propagate any I/O
            // or parse failure instead of silently masking it as exit_code=-1
            // (which used to leave the file's actual content untouched, so
            // later status reads disagreed with what was reported here).
            let raw = fs_err::read_to_string(&exit_status_path)
                .map_err(|e| Error::CouldNotReadStateFile(exit_status_path.clone(), e))?;
            let exit_code: i32 = raw
                .trim()
                .parse()
                .map_err(|_parse_err| Error::InvalidRecordedExitStatus(raw.clone()))?;

            if exit_code != 0 {
                return Err(Error::CommandFailed(
                    command_str,
                    manifest_dir.to_path_buf(),
                    exit_code,
                ));
            }
            Ok(())
        }
    }
}

/// Executes a `manual_step` by launching an interactive asciinema recording session.
///
/// # Errors
///
/// Returns an error if asciinema fails, if I/O fails, if the confirmation file
/// cannot be written, or if the user does not confirm completion.
#[expect(
    clippy::print_stdout,
    reason = "ManualStep is part of the interactive UI"
)]
async fn execute_manual_step(
    step: &ManualStepNode,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
    environment: &Environment,
    extra_env: &[(String, String)],
) -> Result<(), Error> {
    let state_dir = state_base.join(cursor.to_path());
    crate::utils::create_user_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;

    let title = expand_interpolations(&step.title, manifest_dir, state_base)?;
    let instructions = expand_interpolations(&step.instructions, manifest_dir, state_base)?;
    println!("--- Manual Step: {title} ---");
    println!("{instructions}");
    println!(
        "Starting a recording shell in {}. Press Ctrl+D or type `exit` to continue.",
        manifest_dir.display()
    );

    let cast_path = state_dir.join("asciinema.cast");
    let mut cmd = Command::new("asciinema");
    cmd.arg("record");
    if environment.suppress_subprocess_output {
        cmd.arg("--headless");
    }
    cmd.arg("-q").arg(&cast_path);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.current_dir(manifest_dir);

    let status = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?.status;
    if !status.success() {
        println!("Shell exited with a non-zero status code: {status}");
    }

    print!("Was the manual step completed successfully? (y/N) ");
    io::stdout().flush().map_err(Error::IoError)?;
    let mut confirmation = String::new();
    io::stdin()
        .read_line(&mut confirmation)
        .map_err(Error::IoError)?;

    let confirmed = confirmation.trim().eq_ignore_ascii_case("y")
        || confirmation.trim().eq_ignore_ascii_case("yes");
    let manual_step_confirmed_path = state_dir.join("manual_step_confirmed");
    crate::utils::write_user_file(
        &manual_step_confirmed_path,
        if confirmed { "y" } else { "n" },
    )
    .map_err(|e| Error::CouldNotWriteStateFile(manual_step_confirmed_path, e))?;

    if !confirmed {
        return Err(Error::ManualStepNotConfirmed);
    }
    Ok(())
}

/// Evaluates the branch conditions of a workspace `if` block and writes `chosen_branch`.
///
/// The branch index written is 0-based; `"none"` means no branch matched and there
/// is no else clause; `"else"` means no branch matched but there is an else clause.
///
/// # Errors
///
/// Returns an error if condition evaluation fails or the state file cannot be written.
#[expect(clippy::print_stdout, reason = "if-block evaluation is part of the UI")]
fn evaluate_workspace_if_block(
    block: &WorkspaceIfBlock,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
) -> Result<(), Error> {
    let state_dir = state_base.join(cursor.to_path());
    crate::utils::create_user_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;

    println!("Evaluating if at {cursor}:");
    let mut chosen: Option<usize> = None;
    for (i, branch) in block.branches.iter().enumerate() {
        let result = evaluate_workspace_condition(
            &branch.condition,
            manifest_dir,
            environment,
            config,
            extra_env,
        )?;
        let detail = workspace_condition_runtime_detail(&branch.condition, manifest_dir)
            .map(|d| format!(" [{d}]"))
            .unwrap_or_default();
        println!("  branch {i}: {}{detail} → {result}", branch.condition);
        if result && chosen.is_none() {
            chosen = Some(i);
        }
    }

    let chosen_str = chosen.map_or_else(
        || {
            if block.else_statements.is_empty() {
                "none".to_owned()
            } else {
                "else".to_owned()
            }
        },
        |n| n.to_string(),
    );
    match chosen_str.as_str() {
        "none" => println!("  → no branch taken"),
        "else" => println!("  → else branch taken"),
        n => println!("  → branch {n} taken"),
    }
    let chosen_branch_path = state_dir.join("chosen_branch");
    crate::utils::write_user_file(&chosen_branch_path, &chosen_str)
        .map_err(|e| Error::CouldNotWriteStateFile(chosen_branch_path, e))?;
    Ok(())
}

/// Evaluates the branch conditions of a crate `if` block and writes `chosen_branch`.
///
/// # Errors
///
/// Returns an error if condition evaluation fails or the state file cannot be written.
#[expect(clippy::print_stdout, reason = "if-block evaluation is part of the UI")]
fn evaluate_crate_if_block(
    block: &CrateIfBlock,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
) -> Result<(), Error> {
    let state_dir = state_base.join(cursor.to_path());
    crate::utils::create_user_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;

    println!("Evaluating if at {cursor}:");
    let mut chosen: Option<usize> = None;
    for (i, branch) in block.branches.iter().enumerate() {
        let result = evaluate_crate_condition(
            &branch.condition,
            manifest_dir,
            environment,
            config,
            extra_env,
        )?;
        let detail = crate_condition_runtime_detail(&branch.condition, manifest_dir)
            .map(|d| format!(" [{d}]"))
            .unwrap_or_default();
        println!("  branch {i}: {}{detail} → {result}", branch.condition);
        if result && chosen.is_none() {
            chosen = Some(i);
        }
    }

    let chosen_str = chosen.map_or_else(
        || {
            if block.else_statements.is_empty() {
                "none".to_owned()
            } else {
                "else".to_owned()
            }
        },
        |n| n.to_string(),
    );
    match chosen_str.as_str() {
        "none" => println!("  → no branch taken"),
        "else" => println!("  → else branch taken"),
        n => println!("  → branch {n} taken"),
    }
    let chosen_branch_path = state_dir.join("chosen_branch");
    crate::utils::write_user_file(&chosen_branch_path, &chosen_str)
        .map_err(|e| Error::CouldNotWriteStateFile(chosen_branch_path, e))?;
    Ok(())
}

/// Result of running a scope of statements to completion.
///
/// `Suspended` is propagated up through every enclosing scope so the parallel
/// runner and CLI can distinguish a workspace/crate that genuinely finished
/// from one that stopped mid-execution at a `wait_for_continue` barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// All statements in the scope ran to completion successfully.
    Done,
    /// Execution stopped because a `wait_for_continue` barrier in this scope
    /// is currently in the *waiting* state. Statements before the barrier
    /// (in execution order) completed normally; statements after it did not
    /// run and will resume after `task continue` is issued.
    Suspended,
}

/// Handles reaching a `wait_for_continue` barrier during a `run` walk.
///
/// Returns `None` when the barrier is already released (so the walk continues to
/// the next statement). Otherwise it transitions a pending barrier to *waiting*
/// (creating its state dir), prints the release instructions, and returns
/// `Some(StepOutcome::Suspended)` so the walk halts. Shared by the crate and
/// workspace walks.
#[expect(clippy::print_stdout, reason = "barrier message is part of the UI")]
fn barrier_step(
    state_dir: &Path,
    cursor: &ProgramCursor,
    node: &WaitForContinueNode,
    task_name: &str,
) -> Result<Option<StepOutcome>, Error> {
    if is_wait_barrier_released(state_dir) {
        return Ok(None);
    }
    // Pending or waiting — create state_dir (pending → waiting) and stop.
    if !state_dir.exists() {
        crate::utils::create_user_dir_all(state_dir)
            .map_err(|e| Error::CouldNotCreateStateDir(state_dir.to_path_buf(), e))?;
    }
    println!(
        "Wait barrier reached at {}: \"{}\". Release with `cargo-for-each task continue --name {} --cursor {}`.",
        cursor.to_path_string(),
        node.description,
        task_name,
        cursor.to_path_string()
    );
    Ok(Some(StepOutcome::Suspended))
}

/// Runs the chosen branch of a crate `if` block to completion, evaluating the
/// branch conditions first when they have not been evaluated yet.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors run_crate_stmts_to_completion's parameter set"
)]
async fn run_crate_if_block(
    block: &CrateIfBlock,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
    task_name: &str,
) -> Result<StepOutcome, Error> {
    let state_dir = state_base.join(cursor.to_path());
    let chosen_branch_path = state_dir.join("chosen_branch");
    if !chosen_branch_path.exists() {
        evaluate_crate_if_block(
            block,
            cursor,
            manifest_dir,
            state_base,
            environment,
            config,
            extra_env,
        )?;
    }
    let chosen = fs_err::read_to_string(&chosen_branch_path)
        .map_err(|e| Error::CouldNotReadStateFile(chosen_branch_path.clone(), e))?;
    let trimmed = chosen.trim();
    match trimmed {
        "none" => Ok(StepOutcome::Done),
        "else" => {
            Box::pin(run_crate_stmts_to_completion(
                &block.else_statements,
                &cursor.clone().with(CursorSegment::ElseBranch),
                manifest_dir,
                state_base,
                environment,
                config,
                extra_env,
                task_name,
            ))
            .await
        }
        s => {
            let n: usize = s
                .parse()
                .map_err(|_parse_err| Error::InvalidChosenBranch(trimmed.to_owned()))?;
            let branch = block
                .branches
                .get(n)
                .ok_or_else(|| Error::InvalidChosenBranch(trimmed.to_owned()))?;
            Box::pin(run_crate_stmts_to_completion(
                &branch.statements,
                &cursor.clone().with(CursorSegment::IfBranch(n)),
                manifest_dir,
                state_base,
                environment,
                config,
                extra_env,
                task_name,
            ))
            .await
        }
    }
}

/// Runs the chosen branch of a workspace `if` block to completion. See
/// [`run_crate_if_block`] for the branch-selection semantics.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors run_workspace_stmts_to_completion's parameter set"
)]
async fn run_workspace_if_block(
    block: &WorkspaceIfBlock,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
    task_name: &str,
) -> Result<StepOutcome, Error> {
    let state_dir = state_base.join(cursor.to_path());
    let chosen_branch_path = state_dir.join("chosen_branch");
    if !chosen_branch_path.exists() {
        evaluate_workspace_if_block(
            block,
            cursor,
            manifest_dir,
            state_base,
            environment,
            config,
            extra_env,
        )?;
    }
    let chosen = fs_err::read_to_string(&chosen_branch_path)
        .map_err(|e| Error::CouldNotReadStateFile(chosen_branch_path.clone(), e))?;
    let trimmed = chosen.trim();
    match trimmed {
        "none" => Ok(StepOutcome::Done),
        "else" => {
            Box::pin(run_workspace_stmts_to_completion(
                &block.else_statements,
                &cursor.clone().with(CursorSegment::ElseBranch),
                manifest_dir,
                member_crates,
                state_base,
                environment,
                config,
                extra_env,
                task_name,
            ))
            .await
        }
        s => {
            let n: usize = s
                .parse()
                .map_err(|_parse_err| Error::InvalidChosenBranch(trimmed.to_owned()))?;
            let branch = block
                .branches
                .get(n)
                .ok_or_else(|| Error::InvalidChosenBranch(trimmed.to_owned()))?;
            Box::pin(run_workspace_stmts_to_completion(
                &branch.statements,
                &cursor.clone().with(CursorSegment::IfBranch(n)),
                manifest_dir,
                member_crates,
                state_base,
                environment,
                config,
                extra_env,
                task_name,
            ))
            .await
        }
    }
}

/// Runs all crate statements to completion, skipping already-completed ones.
///
/// Handles `if` blocks by evaluating conditions if not yet done, then running
/// the chosen branch's statements recursively.
///
/// Returns [`StepOutcome::Suspended`] if execution stopped at a
/// `wait_for_continue` barrier; callers must propagate that upward.
///
/// # Errors
///
/// Returns an error if any statement fails.
#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are needed; the task_name threading adds one more than clippy's default limit"
)]
async fn run_crate_stmts_to_completion(
    stmts: &[CrateStatement],
    prefix: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
    task_name: &str,
) -> Result<StepOutcome, Error> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            CrateStatement::Run(step) => {
                if !is_run_completed(&state_dir)? {
                    execute_run_step(
                        step,
                        &cursor,
                        manifest_dir,
                        state_base,
                        environment,
                        extra_env,
                    )
                    .await?;
                }
            }
            CrateStatement::ManualStep(step) => {
                if !is_manual_completed(&state_dir) {
                    execute_manual_step(
                        step,
                        &cursor,
                        manifest_dir,
                        state_base,
                        environment,
                        extra_env,
                    )
                    .await?;
                }
            }
            CrateStatement::SnapshotMetadata(step) => {
                if !is_snapshot_metadata_completed(&state_dir) {
                    execute_snapshot_metadata_step(step, &cursor, manifest_dir, state_base).await?;
                }
            }
            CrateStatement::If(block) => {
                let inner = run_crate_if_block(
                    block,
                    &cursor,
                    manifest_dir,
                    state_base,
                    environment,
                    config,
                    extra_env,
                    task_name,
                )
                .await?;
                if matches!(inner, StepOutcome::Suspended) {
                    return Ok(StepOutcome::Suspended);
                }
            }
            CrateStatement::WithEnvFile(block) => {
                let file_vars = load_env_file(&manifest_dir.join(&block.env_file))?;
                let mut combined = extra_env.to_vec();
                combined.extend(file_vars);
                let inner_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                let inner = Box::pin(run_crate_stmts_to_completion(
                    &block.statements,
                    &inner_prefix,
                    manifest_dir,
                    state_base,
                    environment,
                    config,
                    &combined,
                    task_name,
                ))
                .await?;
                if matches!(inner, StepOutcome::Suspended) {
                    return Ok(StepOutcome::Suspended);
                }
            }
            CrateStatement::WaitForContinue(node) => {
                if let Some(outcome) = barrier_step(&state_dir, &cursor, node, task_name)? {
                    return Ok(outcome);
                }
            }
        }
    }
    Ok(StepOutcome::Done)
}

/// Runs every member crate of a `for crate in workspace` block to completion,
/// in intra-workspace dependency order.
///
/// Returns [`StepOutcome::Suspended`] as soon as a member stops at a barrier —
/// later members (and the workspace statements after the block) must not run,
/// since they may depend on work the suspended member does after release.
///
/// # Errors
///
/// Propagates any error from running a member crate.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors run_crate_stmts_to_completion's parameter set"
)]
async fn run_for_crate_in_workspace(
    block: &ForCrateInWorkspaceBlock,
    cursor: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
    task_name: &str,
) -> Result<StepOutcome, Error> {
    for (c_idx, crate_exec) in member_crates.iter().enumerate() {
        let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
        let inner = run_crate_stmts_to_completion(
            &block.statements,
            &c_prefix,
            &crate_exec.manifest_dir,
            state_base,
            environment,
            config,
            extra_env,
            task_name,
        )
        .await?;
        if matches!(inner, StepOutcome::Suspended) {
            return Ok(StepOutcome::Suspended);
        }
    }
    Ok(StepOutcome::Done)
}

/// Runs all workspace statements to completion, including nested `for crate in workspace`.
///
/// Already-completed statements are skipped.
///
/// # Errors
///
/// Returns an error if any statement fails.
#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are needed; the env-file threading adds one more than clippy's default limit"
)]
async fn run_workspace_stmts_to_completion(
    stmts: &[WorkspaceStatement],
    prefix: &ProgramCursor,
    manifest_dir: &Path,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
    task_name: &str,
) -> Result<StepOutcome, Error> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            WorkspaceStatement::Run(step) => {
                if !is_run_completed(&state_dir)? {
                    execute_run_step(
                        step,
                        &cursor,
                        manifest_dir,
                        state_base,
                        environment,
                        extra_env,
                    )
                    .await?;
                }
            }
            WorkspaceStatement::ManualStep(step) => {
                if !is_manual_completed(&state_dir) {
                    execute_manual_step(
                        step,
                        &cursor,
                        manifest_dir,
                        state_base,
                        environment,
                        extra_env,
                    )
                    .await?;
                }
            }
            WorkspaceStatement::SnapshotMetadata(step) => {
                if !is_snapshot_metadata_completed(&state_dir) {
                    execute_snapshot_metadata_step(step, &cursor, manifest_dir, state_base).await?;
                }
            }
            WorkspaceStatement::If(block) => {
                let inner = run_workspace_if_block(
                    block,
                    &cursor,
                    manifest_dir,
                    member_crates,
                    state_base,
                    environment,
                    config,
                    extra_env,
                    task_name,
                )
                .await?;
                if matches!(inner, StepOutcome::Suspended) {
                    return Ok(StepOutcome::Suspended);
                }
            }
            WorkspaceStatement::WithEnvFile(block) => {
                let file_vars = load_env_file(&manifest_dir.join(&block.env_file))?;
                let mut combined = extra_env.to_vec();
                combined.extend(file_vars);
                let inner_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                let inner = Box::pin(run_workspace_stmts_to_completion(
                    &block.statements,
                    &inner_prefix,
                    manifest_dir,
                    member_crates,
                    state_base,
                    environment,
                    config,
                    &combined,
                    task_name,
                ))
                .await?;
                if matches!(inner, StepOutcome::Suspended) {
                    return Ok(StepOutcome::Suspended);
                }
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                if matches!(
                    run_for_crate_in_workspace(
                        block,
                        &cursor,
                        member_crates,
                        state_base,
                        environment,
                        config,
                        extra_env,
                        task_name,
                    )
                    .await?,
                    StepOutcome::Suspended
                ) {
                    return Ok(StepOutcome::Suspended);
                }
            }
            WorkspaceStatement::WaitForContinue(node) => {
                if let Some(outcome) = barrier_step(&state_dir, &cursor, node, task_name)? {
                    return Ok(outcome);
                }
            }
        }
    }
    Ok(StepOutcome::Done)
}

// ── Load helpers ───────────────────────────────────────────────────────────────

/// Loads the parsed program and resolved snapshot for the given task.
///
/// # Errors
///
/// Returns an error if the task directory does not exist, if the program source
/// file cannot be read or parsed, or if the resolved program snapshot cannot be
/// read or parsed.
fn load_task_data(
    task_name: &str,
    environment: &Environment,
) -> Result<(Program, ResolvedProgram), Error> {
    let task_dir = named_dir_path(task_name, environment)?;
    if !task_dir.exists() {
        return Err(Error::TaskNotFound(task_name.to_owned()));
    }

    let program_source_path = task_dir.join("program.cfe");
    let source =
        fs_err::read_to_string(&program_source_path).map_err(Error::CouldNotReadProgramFile)?;
    let program = crate::program::parser::parse(&source, "program.cfe").map_err(|errors| {
        let msgs = errors
            .iter()
            .map(|e| e.as_str().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        Error::ProgramParseErrors(msgs)
    })?;

    let resolved_path = task_dir.join("resolved-program.toml");
    let resolved_src = fs_err::read_to_string(&resolved_path)
        .map_err(|e| Error::CouldNotReadResolvedProgram(resolved_path.clone(), e))?;
    let resolved: ResolvedProgram = toml::from_str(&resolved_src)
        .map_err(|e| Error::CouldNotParseResolvedProgram(resolved_path.clone(), e))?;

    Ok((program, resolved))
}

// ── Rewind helpers ─────────────────────────────────────────────────────────────

/// Finds the cursor of the last completed crate statement (searched in reverse).
///
/// An `if` whose `chosen_branch` is `"none"` (conditions all false, no `else`)
/// counts as a rewindable step even though no body executed. This is
/// intentional: a rewind here forces `evaluate_crate_if_block` to re-run the
/// conditions next time forward, which is the desired UX when the user is
/// rewinding because they answered an `ask_user` wrong, or when a `run`/
/// `file_exists` condition probes external state that may have changed.
fn find_last_completed_crate_stmt(
    stmts: &[CrateStatement],
    prefix: &ProgramCursor,
    state_base: &Path,
) -> Result<Option<ProgramCursor>, Error> {
    for (i, stmt) in stmts.iter().enumerate().rev() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        // Check inside IfBlocks and WithEnvFile blocks for nested completed statements first.
        match stmt {
            CrateStatement::If(block) => {
                let state_dir = state_base.join(cursor.to_path());
                if let Some(chosen) = read_chosen_branch(&state_dir)? {
                    let nested = match chosen.trim() {
                        "else" => {
                            let p = cursor.clone().with(CursorSegment::ElseBranch);
                            find_last_completed_crate_stmt(&block.else_statements, &p, state_base)?
                        }
                        s => match s.parse::<usize>() {
                            Ok(n) => match block.branches.get(n) {
                                Some(branch) => {
                                    let p = cursor.clone().with(CursorSegment::IfBranch(n));
                                    find_last_completed_crate_stmt(
                                        &branch.statements,
                                        &p,
                                        state_base,
                                    )?
                                }
                                None => None,
                            },
                            Err(_) => None,
                        },
                    };
                    if nested.is_some() {
                        return Ok(nested);
                    }
                }
            }
            CrateStatement::WithEnvFile(block) => {
                let p = cursor.clone().with(CursorSegment::WithEnvFile);
                let nested = find_last_completed_crate_stmt(&block.statements, &p, state_base)?;
                if nested.is_some() {
                    return Ok(nested);
                }
            }
            CrateStatement::Run(_)
            | CrateStatement::ManualStep(_)
            | CrateStatement::SnapshotMetadata(_)
            | CrateStatement::WaitForContinue(_) => {}
        }
        if is_crate_stmt_completed(stmt, &cursor, state_base)? {
            return Ok(Some(cursor));
        }
    }
    Ok(None)
}

/// Finds the cursor of the last completed workspace statement (searched in reverse).
///
/// See [`find_last_completed_crate_stmt`] for why an `if` with
/// `chosen_branch == "none"` is treated as a rewindable step.
fn find_last_completed_workspace_stmt(
    stmts: &[WorkspaceStatement],
    prefix: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
) -> Result<Option<ProgramCursor>, Error> {
    for (i, stmt) in stmts.iter().enumerate().rev() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        match stmt {
            WorkspaceStatement::If(block) => {
                let state_dir = state_base.join(cursor.to_path());
                if let Some(chosen) = read_chosen_branch(&state_dir)? {
                    let nested = match chosen.trim() {
                        "else" => {
                            let p = cursor.clone().with(CursorSegment::ElseBranch);
                            find_last_completed_workspace_stmt(
                                &block.else_statements,
                                &p,
                                member_crates,
                                state_base,
                            )?
                        }
                        s => match s.parse::<usize>() {
                            Ok(n) => match block.branches.get(n) {
                                Some(branch) => {
                                    let p = cursor.clone().with(CursorSegment::IfBranch(n));
                                    find_last_completed_workspace_stmt(
                                        &branch.statements,
                                        &p,
                                        member_crates,
                                        state_base,
                                    )?
                                }
                                None => None,
                            },
                            Err(_) => None,
                        },
                    };
                    if nested.is_some() {
                        return Ok(nested);
                    }
                }
            }
            WorkspaceStatement::WithEnvFile(block) => {
                let p = cursor.clone().with(CursorSegment::WithEnvFile);
                let nested = find_last_completed_workspace_stmt(
                    &block.statements,
                    &p,
                    member_crates,
                    state_base,
                )?;
                if nested.is_some() {
                    return Ok(nested);
                }
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                for (c_idx, _) in member_crates.iter().enumerate().rev() {
                    let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
                    let nested =
                        find_last_completed_crate_stmt(&block.statements, &c_prefix, state_base)?;
                    if nested.is_some() {
                        return Ok(nested);
                    }
                }
            }
            WorkspaceStatement::Run(_)
            | WorkspaceStatement::ManualStep(_)
            | WorkspaceStatement::SnapshotMetadata(_)
            | WorkspaceStatement::WaitForContinue(_) => {}
        }
        if is_workspace_stmt_completed(stmt, &cursor, member_crates, state_base)? {
            return Ok(Some(cursor));
        }
    }
    Ok(None)
}

// ── Command implementations ────────────────────────────────────────────────────

/// Creates a new task by parsing and resolving the given `.cfe` program file.
///
/// # Errors
///
/// Returns an error if the program file cannot be read or parsed, if the
/// configuration cannot be loaded, if the program cannot be resolved, if the
/// task directory already exists or cannot be created, or if the task files
/// cannot be written.
#[instrument]
pub async fn task_create_command(
    params: CreateTaskParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    if !params.program.exists() {
        return Err(Error::ProgramNotFound(params.program.clone()));
    }
    let source = fs_err::read_to_string(&params.program).map_err(Error::CouldNotReadProgramFile)?;
    let program = crate::program::parser::parse(&source, &params.program.to_string_lossy())
        .map_err(|errors| {
            let msgs = errors
                .iter()
                .map(|e| e.as_str().to_owned())
                .collect::<Vec<_>>()
                .join("\n");
            Error::ProgramParseErrors(msgs)
        })?;

    use crate::program::resolve::{
        ResolvedProgram, resolve_explicit_crate_targets, resolve_explicit_workspace_targets,
    };
    let resolved = if params.workspaces.is_empty() && params.crates.is_empty() {
        let config = Config::load(&environment)?;
        crate::program::resolve::resolve_program(&program, &config)?
    } else if params.workspaces.is_empty() || params.crates.is_empty() {
        // One side uses explicit paths; the other still needs the program selection.
        let config = Config::load(&environment)?;
        let from_program = crate::program::resolve::resolve_program(&program, &config)?;
        let workspace_executions = if params.workspaces.is_empty() {
            from_program.workspace_executions
        } else {
            resolve_explicit_workspace_targets(&params.workspaces)?
        };
        let crate_executions = if params.crates.is_empty() {
            from_program.crate_executions
        } else {
            resolve_explicit_crate_targets(&params.crates)?
        };
        ResolvedProgram {
            workspace_executions,
            crate_executions,
        }
    } else {
        // Both sides are explicit — no config or program selection needed.
        ResolvedProgram {
            workspace_executions: resolve_explicit_workspace_targets(&params.workspaces)?,
            crate_executions: resolve_explicit_crate_targets(&params.crates)?,
        }
    };

    let task_dir = named_dir_path(&params.name, &environment)?;
    if task_dir.exists() {
        return Err(Error::AlreadyExists(format!("task {}", params.name)));
    }
    crate::utils::create_user_dir_all(&task_dir)
        .map_err(|e| Error::CouldNotCreateTaskDir(task_dir.clone(), e))?;

    crate::utils::copy_user_file(&params.program, task_dir.join("program.cfe")).map_err(|e| {
        Error::CouldNotCopyFile(params.program.clone(), task_dir.join("program.cfe"), e)
    })?;

    let resolved_path = task_dir.join("resolved-program.toml");
    crate::utils::write_user_file(
        &resolved_path,
        toml::to_string(&resolved).map_err(Error::CouldNotSerializeResolvedProgram)?,
    )
    .map_err(Error::CouldNotWriteResolvedProgram)?;

    Ok(())
}

/// Executes the action of a single resolved [`NextStatement`].
///
/// # Errors
///
/// Propagates errors from the underlying step executor or if-block evaluator.
#[expect(clippy::print_stdout, reason = "barrier message is part of the UI")]
async fn execute_next_action(
    next: NextStatement<'_>,
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
    task_name: &str,
) -> Result<(), Error> {
    match next.action {
        StatementAction::RunCommand(step) => {
            execute_run_step(
                step,
                &next.cursor,
                next.manifest_dir,
                state_base,
                environment,
                extra_env,
            )
            .await
        }
        StatementAction::ManualStep(step) => {
            execute_manual_step(
                step,
                &next.cursor,
                next.manifest_dir,
                state_base,
                environment,
                extra_env,
            )
            .await
        }
        StatementAction::EvaluateWorkspaceIf(block) => evaluate_workspace_if_block(
            block,
            &next.cursor,
            next.manifest_dir,
            state_base,
            environment,
            config,
            extra_env,
        ),
        StatementAction::EvaluateCrateIf(block) => evaluate_crate_if_block(
            block,
            &next.cursor,
            next.manifest_dir,
            state_base,
            environment,
            config,
            extra_env,
        ),
        StatementAction::SnapshotMetadata(step) => {
            execute_snapshot_metadata_step(step, &next.cursor, next.manifest_dir, state_base).await
        }
        StatementAction::WaitForContinue(node) => {
            let state_dir = state_base.join(next.cursor.to_path());
            crate::utils::create_user_dir_all(&state_dir)
                .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;
            // NOTE on a theoretical race: if a concurrent `task continue` writes
            // the `barrier_released` marker between `find_next_statement`
            // returning and this `println!` firing, the message tells the user
            // to release a barrier that has already been released. The next
            // `task run` resumes past the barrier correctly — only the message
            // is misleading. The window is one mkdir plus a println, and the
            // triggering scenarios are contrived for single-user usage.
            // Deliberately unfixed; do not flag.
            println!(
                "Wait barrier reached at {}: \"{}\". Release with `cargo-for-each task continue --name {} --cursor {}`.",
                next.cursor.to_path_string(),
                node.description,
                task_name,
                next.cursor.to_path_string()
            );
            Ok(())
        }
    }
}

/// Prints the set of `wait_for_continue` barriers currently blocking progress.
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
fn report_suspension(
    program: &Program,
    resolved: &ResolvedProgram,
    state_base: &Path,
    task_name: &str,
) {
    let barriers = find_waiting_barriers(program, resolved, state_base);
    if barriers.is_empty() {
        // No barrier surfaced on disk, but find_next reported the tree as
        // blocked — fall back to a generic message.
        println!(
            "No statement is currently executable; execution is suspended at one or more `wait_for_continue` barriers."
        );
    } else {
        println!(
            "Execution is suspended at {} `wait_for_continue` barrier(s):",
            barriers.len()
        );
        for (cursor, description) in &barriers {
            let cursor_str = cursor.to_path_string();
            println!(
                "  {cursor_str}: \"{description}\" — release with `cargo-for-each task continue --name {task_name} --cursor {cursor_str}`"
            );
        }
    }
}

/// Finds and executes the next uncompleted statement in a task.
///
/// # Errors
///
/// Returns an error if the task cannot be loaded or if the statement fails.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_step_command(
    params: RunSingleStepParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let (program, resolved) = load_task_data(&params.name, &environment)?;
    let config = Config::load(&environment)?;
    let state_base = state_dir_for_task(&params.name, &environment)?;

    match find_next_statement(&program, &resolved, &state_base)? {
        NextOutcome::Next(next) => {
            println!(
                "Running statement at {} for {}",
                next.cursor,
                next.manifest_dir.display()
            );
            let extra_env = load_env_vars_from_files(&next.env_file_paths, next.manifest_dir)?;
            execute_next_action(
                next,
                &state_base,
                &environment,
                &config,
                &extra_env,
                &params.name,
            )
            .await?;
        }
        NextOutcome::Suspended => {
            report_suspension(&program, &resolved, &state_base, &params.name);
        }
        NextOutcome::Done => {
            println!("All statements for all targets completed successfully.");
        }
    }
    Ok(())
}

/// Runs all remaining statements for the first ready workspace or standalone crate.
///
/// # Errors
///
/// Returns an error if the task cannot be loaded or if any statement fails.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn run_single_target_command(
    params: RunSingleTargetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let (program, resolved) = load_task_data(&params.name, &environment)?;
    let config = Config::load(&environment)?;
    let state_base = state_dir_for_task(&params.name, &environment)?;

    let ws_stmts = first_workspace_stmts(&program);
    let ws_map: HashMap<PathBuf, usize> = resolved
        .workspace_executions
        .iter()
        .enumerate()
        .map(|(i, w)| (w.manifest_dir.clone(), i))
        .collect();

    for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate() {
        if !are_workspace_deps_completed(ws_exec, &ws_map, ws_stmts, &resolved, &state_base)? {
            continue;
        }
        if is_workspace_completed(ws_idx, ws_exec, ws_stmts, &state_base)? {
            continue;
        }
        println!(
            "Running all statements for workspace {}.",
            ws_exec.manifest_dir.display()
        );
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
        // A `StepOutcome::Suspended` return is not an error here — the inner
        // function has already printed the barrier message to stdout — so we
        // just drop the outcome and return.
        let _outcome = run_workspace_stmts_to_completion(
            ws_stmts,
            &prefix,
            &ws_exec.manifest_dir,
            &ws_exec.member_crates,
            &state_base,
            &environment,
            &config,
            &[],
            &params.name,
        )
        .await?;
        return Ok(());
    }

    let crate_stmts = first_crate_stmts(&program);
    let crate_map: HashMap<PathBuf, usize> = resolved
        .crate_executions
        .iter()
        .enumerate()
        .map(|(i, c)| (c.manifest_dir.clone(), i))
        .collect();

    for (c_idx, crate_exec) in resolved.crate_executions.iter().enumerate() {
        if !are_standalone_crate_deps_completed(crate_exec, &crate_map, crate_stmts, &state_base)? {
            continue;
        }
        if is_standalone_crate_completed(c_idx, crate_stmts, &state_base)? {
            continue;
        }
        println!(
            "Running all statements for crate {}.",
            crate_exec.manifest_dir.display()
        );
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
        let _outcome = run_crate_stmts_to_completion(
            crate_stmts,
            &prefix,
            &crate_exec.manifest_dir,
            &state_base,
            &environment,
            &config,
            &[],
            &params.name,
        )
        .await?;
        return Ok(());
    }

    println!("All targets are either completed or waiting for dependencies.");
    Ok(())
}

/// Returns `true` if `program` contains any statement that reads from stdin
/// (`manual_step`) or any condition that does (`ask_user`).
///
/// Used to refuse parallel execution: with `-j > 1` multiple workspace
/// pipelines run concurrently and would race for the same stdin/stdout if
/// they prompted simultaneously.
fn program_has_interactive_steps(program: &Program) -> bool {
    fn common_has_ask_user(cond: &CommonCondition) -> bool {
        match cond {
            CommonCondition::AskUser(_) => true,
            CommonCondition::Not(inner) => common_has_ask_user(inner),
            CommonCondition::And(conds) | CommonCondition::Or(conds) => {
                conds.iter().any(common_has_ask_user)
            }
            CommonCondition::RunCommand { .. }
            | CommonCondition::FileExists(_)
            | CommonCondition::WorkingDirectoryClean
            | CommonCondition::GitConfigEquals { .. } => false,
        }
    }
    fn ws_cond_has_ask_user(cond: &WorkspaceCondition) -> bool {
        match cond {
            WorkspaceCondition::Common(c) => common_has_ask_user(c),
            WorkspaceCondition::Not(inner) => ws_cond_has_ask_user(inner),
            WorkspaceCondition::And(conds) | WorkspaceCondition::Or(conds) => {
                conds.iter().any(ws_cond_has_ask_user)
            }
            WorkspaceCondition::Standalone | WorkspaceCondition::HasMembers => false,
        }
    }
    fn crate_cond_has_ask_user(cond: &CrateCondition) -> bool {
        match cond {
            CrateCondition::Common(c) => common_has_ask_user(c),
            CrateCondition::Not(inner) => crate_cond_has_ask_user(inner),
            CrateCondition::And(conds) | CrateCondition::Or(conds) => {
                conds.iter().any(crate_cond_has_ask_user)
            }
            CrateCondition::CrateType(_)
            | CrateCondition::TargetKind(_)
            | CrateCondition::Standalone => false,
        }
    }
    fn crate_has_interactive(stmt: &CrateStatement) -> bool {
        match stmt {
            CrateStatement::ManualStep(_) => true,
            CrateStatement::If(block) => {
                block.branches.iter().any(|b| {
                    crate_cond_has_ask_user(&b.condition)
                        || b.statements.iter().any(crate_has_interactive)
                }) || block.else_statements.iter().any(crate_has_interactive)
            }
            CrateStatement::WithEnvFile(block) => {
                block.statements.iter().any(crate_has_interactive)
            }
            CrateStatement::Run(_)
            | CrateStatement::SnapshotMetadata(_)
            | CrateStatement::WaitForContinue(_) => false,
        }
    }
    fn ws_has_interactive(stmt: &WorkspaceStatement) -> bool {
        match stmt {
            WorkspaceStatement::ManualStep(_) => true,
            WorkspaceStatement::If(block) => {
                block.branches.iter().any(|b| {
                    ws_cond_has_ask_user(&b.condition)
                        || b.statements.iter().any(ws_has_interactive)
                }) || block.else_statements.iter().any(ws_has_interactive)
            }
            WorkspaceStatement::WithEnvFile(block) => {
                block.statements.iter().any(ws_has_interactive)
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                block.statements.iter().any(crate_has_interactive)
            }
            WorkspaceStatement::Run(_)
            | WorkspaceStatement::SnapshotMetadata(_)
            | WorkspaceStatement::WaitForContinue(_) => false,
        }
    }
    program.statements.iter().any(|s| match s {
        GlobalStatement::ForWorkspace(b) => b.statements.iter().any(ws_has_interactive),
        GlobalStatement::ForCrate(b) => b.statements.iter().any(crate_has_interactive),
        GlobalStatement::SelectWorkspaces(_) | GlobalStatement::SelectCrates(_) => false,
    })
}

/// Runs one phase of [`run_all_targets_command`]: a set of targets sharing a
/// dependency graph, executed in dependency order with up to `jobs` running
/// concurrently.
///
/// `manifest_dirs[i]` and `dependencies[i]` describe target `i`; a dependency
/// is satisfied once the target whose `manifest_dir` it names has completed
/// (dependencies pointing outside this phase are ignored). `run_target(i)`
/// produces the future that runs target `i`.
///
/// # Errors
///
/// On a target error: returns it immediately unless `keep_going` is set, in
/// which case the target is marked failed (logged with `error_label`) and the
/// phase ends in [`Error::SomeStepsFailed`]. If no target can make progress yet
/// none are suspended at a barrier, returns [`Error::CircularDependency`].
async fn run_scheduler_phase<F, Fut>(
    manifest_dirs: &[PathBuf],
    dependencies: &[Vec<PathBuf>],
    jobs: usize,
    keep_going: bool,
    error_label: &str,
    run_target: F,
) -> Result<(), Error>
where
    F: Fn(usize) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<StepOutcome, Error>> + Send,
{
    let n = manifest_dirs.len();
    let mut completed = vec![false; n];
    let mut failed = vec![false; n];
    // `suspended[i]` marks a target that returned `StepOutcome::Suspended`: its
    // work isn't done, but it can't progress until the user releases a barrier
    // with `task continue`, so dependents must NOT see it as completed.
    let mut suspended = vec![false; n];
    let mut has_errors = false;

    let dep_map: HashMap<PathBuf, usize> = manifest_dirs
        .iter()
        .enumerate()
        .map(|(i, d)| (d.clone(), i))
        .collect();

    loop {
        let ready: Vec<usize> = (0..n)
            .filter(|&idx| {
                !completed.get(idx).copied().unwrap_or(false)
                    && !failed.get(idx).copied().unwrap_or(false)
                    && !suspended.get(idx).copied().unwrap_or(false)
                    && dependencies.get(idx).is_some_and(|deps| {
                        deps.iter().all(|dep| {
                            dep_map
                                .get(dep)
                                .is_none_or(|&di| completed.get(di).copied().unwrap_or(false))
                        })
                    })
            })
            .collect();

        if ready.is_empty() {
            break;
        }

        let results: Vec<(usize, Result<StepOutcome, Error>)> = stream::iter(ready)
            .map(|idx| {
                let fut = run_target(idx);
                async move { (idx, fut.await) }
            })
            .buffer_unordered(jobs)
            .collect()
            .await;

        for (idx, result) in results {
            match result {
                Ok(StepOutcome::Done) => {
                    if let Some(slot) = completed.get_mut(idx) {
                        *slot = true;
                    }
                }
                Ok(StepOutcome::Suspended) => {
                    if let Some(slot) = suspended.get_mut(idx) {
                        *slot = true;
                    }
                }
                Err(e) => {
                    if keep_going {
                        tracing::error!("{error_label}: {e}");
                        if let Some(slot) = failed.get_mut(idx) {
                            *slot = true;
                        }
                        has_errors = true;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    if has_errors {
        return Err(Error::SomeStepsFailed);
    }
    // Some targets suspended (or transitively blocked by a suspended/failed
    // upstream) is not a circular dependency; the user can release barriers
    // with `task continue` and re-run.
    if !suspended.iter().any(|&s| s) && !completed.iter().all(|&c| c) {
        return Err(Error::CircularDependency);
    }
    Ok(())
}

/// Runs all targets in dependency order with optional parallelism.
///
/// Workspaces are executed first (in dependency order), followed by standalone
/// crates.
///
/// # Errors
///
/// Returns an error if the task cannot be loaded, if a statement fails (unless
/// `keep_going` is set), if some steps failed with `keep_going`, or if a
/// circular dependency is detected. Also errors if the program contains
/// interactive steps (`manual_step` / `ask_user`) and `--jobs > 1` was
/// requested, since parallel pipelines would race for stdin.
#[instrument]
pub async fn run_all_targets_command(
    params: RunAllTargetsParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let (program, resolved) = load_task_data(&params.name, &environment)?;
    let config = Arc::new(Config::load(&environment)?);
    let state_base = Arc::new(state_dir_for_task(&params.name, &environment)?);
    let keep_going = params.keep_going;
    let jobs = params.jobs.unwrap_or(1);
    // Reject parallel execution of programs with interactive steps: with
    // jobs > 1 multiple workspace/crate pipelines run concurrently via
    // buffer_unordered below and would race for the same stdin/stdout when
    // any of them prompted the user.
    if jobs > 1 && program_has_interactive_steps(&program) {
        return Err(Error::InteractiveStepsRequireSingleJob);
    }
    let resolved = Arc::new(resolved);

    let ws_stmts: Arc<Vec<WorkspaceStatement>> = Arc::new(first_workspace_stmts(&program).to_vec());
    let crate_stmts: Arc<Vec<CrateStatement>> = Arc::new(first_crate_stmts(&program).to_vec());

    // Phase 1: workspaces (in inter-workspace dependency order).
    let ws_manifests: Vec<PathBuf> = resolved
        .workspace_executions
        .iter()
        .map(|w| w.manifest_dir.clone())
        .collect();
    let ws_deps: Vec<Vec<PathBuf>> = resolved
        .workspace_executions
        .iter()
        .map(|w| w.dependencies.clone())
        .collect();
    run_scheduler_phase(
        &ws_manifests,
        &ws_deps,
        jobs,
        keep_going,
        "Workspace failed",
        |idx| {
            let ws_stmts = Arc::clone(&ws_stmts);
            let config = Arc::clone(&config);
            let state_base = Arc::clone(&state_base);
            let resolved = Arc::clone(&resolved);
            let environment = environment.clone();
            let task_name = params.name.clone();
            async move {
                // `idx` always indexes `workspace_executions` (it came from
                // `0..len`); the `None` arm is unreachable.
                let Some(ws_exec) = resolved.workspace_executions.get(idx) else {
                    return Ok(StepOutcome::Done);
                };
                let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(idx));
                run_workspace_stmts_to_completion(
                    &ws_stmts,
                    &prefix,
                    &ws_exec.manifest_dir,
                    &ws_exec.member_crates,
                    &state_base,
                    &environment,
                    &config,
                    &[],
                    &task_name,
                )
                .await
            }
        },
    )
    .await?;

    // Phase 2: standalone crates (in dependency order).
    let crate_manifests: Vec<PathBuf> = resolved
        .crate_executions
        .iter()
        .map(|c| c.manifest_dir.clone())
        .collect();
    let crate_deps: Vec<Vec<PathBuf>> = resolved
        .crate_executions
        .iter()
        .map(|c| c.dependencies.clone())
        .collect();
    run_scheduler_phase(
        &crate_manifests,
        &crate_deps,
        jobs,
        keep_going,
        "Crate execution failed",
        |idx| {
            let crate_stmts = Arc::clone(&crate_stmts);
            let config = Arc::clone(&config);
            let state_base = Arc::clone(&state_base);
            let resolved = Arc::clone(&resolved);
            let environment = environment.clone();
            let task_name = params.name.clone();
            async move {
                let Some(crate_exec) = resolved.crate_executions.get(idx) else {
                    return Ok(StepOutcome::Done);
                };
                let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(idx));
                run_crate_stmts_to_completion(
                    &crate_stmts,
                    &prefix,
                    &crate_exec.manifest_dir,
                    &state_base,
                    &environment,
                    &config,
                    &[],
                    &task_name,
                )
                .await
            }
        },
    )
    .await?;

    Ok(())
}

/// Dispatches the `task run` subcommand.
///
/// # Errors
///
/// Propagates errors from the chosen subcommand.
#[instrument]
pub async fn task_run_command(
    params: TaskRunParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match params.sub_command {
        TaskRunSubCommand::SingleStep(p) => run_single_step_command(p, environment).await,
        TaskRunSubCommand::SingleTarget(p) => run_single_target_command(p, environment).await,
        TaskRunSubCommand::AllTargets(p) => run_all_targets_command(p, environment).await,
    }
}

// ── Rewind commands ────────────────────────────────────────────────────────────

/// Removes all execution state for a task.
///
/// # Errors
///
/// Returns an error if the state directory cannot be removed.
#[instrument]
pub async fn rewind_all_targets_command(
    params: RewindAllTargetsParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let state_dir = state_dir_for_task(&params.name, &environment)?;
    if state_dir.exists() {
        fs_err::remove_dir_all(&state_dir)
            .map_err(|e| Error::CouldNotRemoveTaskStateDir(state_dir.clone(), e))?;
        tracing::info!("Removed all state for task '{}'.", params.name);
    } else {
        tracing::info!(
            "No state found for task '{}', nothing to rewind.",
            params.name
        );
    }
    Ok(())
}

/// Removes the state for the last completed workspace or standalone crate.
///
/// Standalone crates are checked first (they execute after workspaces).
///
/// # Errors
///
/// Returns an error if the task cannot be loaded or if the state cannot be removed.
#[instrument]
pub async fn rewind_single_target_command(
    params: RewindSingleTargetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let (program, resolved) = load_task_data(&params.name, &environment)?;
    let state_base = state_dir_for_task(&params.name, &environment)?;

    let ws_stmts = first_workspace_stmts(&program);
    let crate_stmts = first_crate_stmts(&program);

    // Standalone crates execute last — search them in reverse first.
    for (c_idx, _) in resolved.crate_executions.iter().enumerate().rev() {
        if is_standalone_crate_completed(c_idx, crate_stmts, &state_base)? {
            let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
            let target_state_dir = state_base.join(prefix.to_path());
            if target_state_dir.exists() {
                fs_err::remove_dir_all(&target_state_dir)
                    .map_err(|e| Error::CouldNotRemoveTaskStateDir(target_state_dir.clone(), e))?;
            }
            tracing::info!(
                "Rewound standalone crate {} in task '{}'.",
                c_idx,
                params.name
            );
            return Ok(());
        }
    }

    for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate().rev() {
        if is_workspace_completed(ws_idx, ws_exec, ws_stmts, &state_base)? {
            let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
            let target_state_dir = state_base.join(prefix.to_path());
            if target_state_dir.exists() {
                fs_err::remove_dir_all(&target_state_dir)
                    .map_err(|e| Error::CouldNotRemoveTaskStateDir(target_state_dir.clone(), e))?;
            }
            tracing::info!("Rewound workspace {} in task '{}'.", ws_idx, params.name);
            return Ok(());
        }
    }

    tracing::info!(
        "No completed targets found for task '{}', nothing to rewind.",
        params.name
    );
    Ok(())
}

/// Removes the state directory for the last completed statement in a task.
///
/// # Errors
///
/// Returns an error if the task cannot be loaded or if the state cannot be removed.
#[instrument]
pub async fn rewind_single_step_command(
    params: RewindSingleStepParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let task_dir = named_dir_path(&params.name, &environment)?;
    if !task_dir.exists() {
        return Err(Error::TaskNotFound(params.name));
    }
    let (program, resolved) = load_task_data(&params.name, &environment)?;
    let state_base = state_dir_for_task(&params.name, &environment)?;

    let crate_stmts = first_crate_stmts(&program);
    let ws_stmts = first_workspace_stmts(&program);

    // Standalone crates execute last — search in reverse first.
    for (c_idx, _) in resolved.crate_executions.iter().enumerate().rev() {
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
        if let Some(cursor) = find_last_completed_crate_stmt(crate_stmts, &prefix, &state_base)? {
            let step_state_dir = state_base.join(cursor.to_path());
            if step_state_dir.exists() {
                fs_err::remove_dir_all(&step_state_dir)
                    .map_err(|e| Error::CouldNotRemoveTaskStateDir(step_state_dir.clone(), e))?;
            }
            tracing::info!("Rewound statement {} in task '{}'.", cursor, params.name);
            return Ok(());
        }
    }

    for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate().rev() {
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
        if let Some(cursor) = find_last_completed_workspace_stmt(
            ws_stmts,
            &prefix,
            &ws_exec.member_crates,
            &state_base,
        )? {
            let step_state_dir = state_base.join(cursor.to_path());
            if step_state_dir.exists() {
                fs_err::remove_dir_all(&step_state_dir)
                    .map_err(|e| Error::CouldNotRemoveTaskStateDir(step_state_dir.clone(), e))?;
            }
            tracing::info!("Rewound statement {} in task '{}'.", cursor, params.name);
            return Ok(());
        }
    }

    tracing::info!(
        "No completed statements found for task '{}', nothing to rewind.",
        params.name
    );
    Ok(())
}

/// Dispatches the `task rewind` subcommand.
///
/// # Errors
///
/// Propagates errors from the chosen subcommand.
#[instrument]
pub async fn task_rewind_command(
    params: TaskRewindParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match params.sub_command {
        TaskRewindSubCommand::SingleStep(p) => rewind_single_step_command(p, environment).await,
        TaskRewindSubCommand::SingleTarget(p) => rewind_single_target_command(p, environment).await,
        TaskRewindSubCommand::AllTargets(p) => rewind_all_targets_command(p, environment).await,
    }
}

// ── Describe and list commands ─────────────────────────────────────────────────

/// Builds the label string for a crate statement (raw AST, no interpolation).
fn crate_stmt_label(stmt: &CrateStatement) -> String {
    match stmt {
        CrateStatement::Run(step) => {
            let mut parts = vec![format!("\"{}\"", step.command)];
            parts.extend(step.args.iter().map(|a| format!("\"{a}\"")));
            format!("run {}", parts.join(" "))
        }
        CrateStatement::ManualStep(node) => format!("manual_step \"{}\"", node.title),
        CrateStatement::SnapshotMetadata(node) => {
            format!("snapshot_metadata \"{}\"", node.name)
        }
        CrateStatement::If(_) => String::from("if ..."),
        CrateStatement::WithEnvFile(block) => {
            format!("with_env_file \"{}\"", block.env_file)
        }
        CrateStatement::WaitForContinue(node) => {
            format!("wait_for_continue \"{}\"", node.description)
        }
    }
}

/// Builds the label string for a workspace statement (raw AST, no interpolation).
fn workspace_stmt_label(stmt: &WorkspaceStatement) -> String {
    match stmt {
        WorkspaceStatement::Run(step) => {
            let mut parts = vec![format!("\"{}\"", step.command)];
            parts.extend(step.args.iter().map(|a| format!("\"{a}\"")));
            format!("run {}", parts.join(" "))
        }
        WorkspaceStatement::ManualStep(node) => format!("manual_step \"{}\"", node.title),
        WorkspaceStatement::SnapshotMetadata(node) => {
            format!("snapshot_metadata \"{}\"", node.name)
        }
        WorkspaceStatement::If(_) => String::from("if ..."),
        WorkspaceStatement::ForCrateInWorkspace(_) => String::from("for crate in workspace"),
        WorkspaceStatement::WithEnvFile(block) => {
            format!("with_env_file \"{}\"", block.env_file)
        }
        WorkspaceStatement::WaitForContinue(node) => {
            format!("wait_for_continue \"{}\"", node.description)
        }
    }
}

/// Icon for a completed step.
const ICON_DONE: &str = "\u{2705}";
/// Icon for a step that has not started / is incomplete.
const ICON_PENDING: &str = "\u{2B1C}";
/// Icon for a `run` step that exited non-zero.
const ICON_FAILED: &str = "\u{274C}";
/// Icon for a `wait_for_continue` barrier in the *waiting* state.
const ICON_WAITING: &str = "\u{23F3}";

/// One rendered line of `task describe` output: an indent, the cursor path,
/// a status icon, and a human-readable label. Built purely (no I/O on the
/// output stream) so the walk can be unit-tested independently of printing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DescribeLine {
    /// Leading whitespace conveying the statement's nesting depth.
    indent: String,
    /// The cursor path string identifying the statement.
    cursor: String,
    /// Status icon (one of the `ICON_*` constants).
    icon: &'static str,
    /// Human-readable statement label.
    label: String,
}

/// Builds a [`DescribeLine`] from borrowed parts.
fn describe_line(indent: &str, cursor: &str, icon: &'static str, label: &str) -> DescribeLine {
    DescribeLine {
        indent: indent.to_owned(),
        cursor: cursor.to_owned(),
        icon,
        label: label.to_owned(),
    }
}

/// `ICON_DONE` when `done`, otherwise `ICON_PENDING`.
const fn bool_icon(done: bool) -> &'static str {
    if done { ICON_DONE } else { ICON_PENDING }
}

/// Done / failed / pending icon for a `run` step's state directory.
fn run_icon(state_dir: &Path) -> Result<&'static str, Error> {
    if is_run_completed(state_dir)? {
        Ok(ICON_DONE)
    } else if is_run_failed(state_dir)? {
        Ok(ICON_FAILED)
    } else {
        Ok(ICON_PENDING)
    }
}

/// Released / waiting / pending icon for a `wait_for_continue` barrier.
fn barrier_icon(state_dir: &Path) -> &'static str {
    if is_wait_barrier_released(state_dir) {
        ICON_DONE
    } else if is_wait_barrier_waiting(state_dir) {
        ICON_WAITING
    } else {
        ICON_PENDING
    }
}

/// Maps a trimmed `chosen_branch` marker to an `if` line's (icon, label).
fn if_describe(chosen: &str) -> (&'static str, &'static str) {
    if chosen.is_empty() {
        (ICON_PENDING, "if [not yet evaluated]")
    } else if chosen == "none" {
        (ICON_DONE, "if [no branch matched]")
    } else if chosen == "else" {
        (ICON_DONE, "if [else branch taken]")
    } else {
        (ICON_DONE, "if [branch taken]")
    }
}

/// Reads and trims the `chosen_branch` marker, treating any read error as
/// "not yet evaluated" (empty string) — matching the describe UI's intent of
/// never failing just because a marker is unreadable.
fn read_chosen_branch_lossy(state_dir: &Path) -> String {
    fs_err::read_to_string(state_dir.join("chosen_branch"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// Recursively appends `DescribeLine`s for the chosen child branch of an `if`.
fn build_crate_if_children(
    block: &CrateIfBlock,
    cursor: &ProgramCursor,
    chosen: &str,
    state_base: &Path,
    indent: &str,
    out: &mut Vec<DescribeLine>,
) -> Result<(), Error> {
    let nested_indent = format!("{indent}  ");
    if chosen == "else" {
        build_crate_stmts_describe(
            &block.else_statements,
            &cursor.clone().with(CursorSegment::ElseBranch),
            state_base,
            &nested_indent,
            out,
        )?;
    } else if let Ok(n) = chosen.parse::<usize>()
        && let Some(branch) = block.branches.get(n)
    {
        build_crate_stmts_describe(
            &branch.statements,
            &cursor.clone().with(CursorSegment::IfBranch(n)),
            state_base,
            &nested_indent,
            out,
        )?;
    }
    Ok(())
}

/// Completion icon for a non-`if` crate statement.
fn crate_stmt_icon(
    stmt: &CrateStatement,
    cursor: &ProgramCursor,
    state_dir: &Path,
    state_base: &Path,
) -> Result<&'static str, Error> {
    match stmt {
        CrateStatement::Run(_) => run_icon(state_dir),
        CrateStatement::WaitForContinue(_) => Ok(barrier_icon(state_dir)),
        CrateStatement::WithEnvFile(block) => {
            let env_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
            Ok(bool_icon(is_crate_stmts_completed(
                &block.statements,
                &env_prefix,
                state_base,
            )?))
        }
        CrateStatement::ManualStep(_) | CrateStatement::SnapshotMetadata(_) => Ok(bool_icon(
            is_crate_stmt_completed(stmt, cursor, state_base)?,
        )),
        CrateStatement::If(_) => Ok(ICON_PENDING),
    }
}

/// Recursively builds the describe lines for crate statements.
fn build_crate_stmts_describe(
    stmts: &[CrateStatement],
    prefix: &ProgramCursor,
    state_base: &Path,
    indent: &str,
    out: &mut Vec<DescribeLine>,
) -> Result<(), Error> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());
        let cursor_str = cursor.to_path_string();

        if let CrateStatement::If(block) = stmt {
            let chosen = read_chosen_branch_lossy(&state_dir);
            let (icon, label) = if_describe(&chosen);
            out.push(describe_line(indent, &cursor_str, icon, label));
            build_crate_if_children(block, &cursor, &chosen, state_base, indent, out)?;
        } else {
            let icon = crate_stmt_icon(stmt, &cursor, &state_dir, state_base)?;
            out.push(describe_line(
                indent,
                &cursor_str,
                icon,
                &crate_stmt_label(stmt),
            ));
            if let CrateStatement::WithEnvFile(block) = stmt {
                let env_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                let nested_indent = format!("{indent}  ");
                build_crate_stmts_describe(
                    &block.statements,
                    &env_prefix,
                    state_base,
                    &nested_indent,
                    out,
                )?;
            }
        }
    }
    Ok(())
}

/// Prints describe lines to stdout in the format used by `task describe`.
#[expect(clippy::print_stdout, reason = "part of the describe UI")]
fn print_describe_lines(lines: &[DescribeLine]) {
    for line in lines {
        println!(
            "{}{:<20}  {}  {}",
            line.indent, line.cursor, line.icon, line.label,
        );
    }
}

/// Appends describe lines for a workspace `for crate in workspace` block: a
/// header line for the block, then a per-member-crate header and the crate's
/// body lines.
#[expect(
    clippy::too_many_arguments,
    reason = "describe walk threads cursor/state/indent/output through each helper"
)]
fn build_workspace_for_crate(
    stmt: &WorkspaceStatement,
    block: &ForCrateInWorkspaceBlock,
    cursor: &ProgramCursor,
    cursor_str: &str,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
    indent: &str,
    out: &mut Vec<DescribeLine>,
) -> Result<(), Error> {
    let icon = bool_icon(is_workspace_stmt_completed(
        stmt,
        cursor,
        member_crates,
        state_base,
    )?);
    out.push(describe_line(
        indent,
        cursor_str,
        icon,
        "for crate in workspace",
    ));
    let crate_indent = format!("{indent}  ");
    let nested_indent = format!("{indent}    ");
    for (c_idx, crate_exec) in member_crates.iter().enumerate() {
        let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
        let c_prefix_str = c_prefix.to_path_string();
        let crate_icon = bool_icon(is_crate_stmts_completed(
            &block.statements,
            &c_prefix,
            state_base,
        )?);
        out.push(describe_line(
            &crate_indent,
            &c_prefix_str,
            crate_icon,
            &format!("crate {}", crate_exec.manifest_dir.display()),
        ));
        build_crate_stmts_describe(
            &block.statements,
            &c_prefix,
            state_base,
            &nested_indent,
            out,
        )?;
    }
    Ok(())
}

/// Recursively appends describe lines for the chosen child branch of a
/// workspace `if`.
fn build_workspace_if_children(
    block: &WorkspaceIfBlock,
    cursor: &ProgramCursor,
    chosen: &str,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
    indent: &str,
    out: &mut Vec<DescribeLine>,
) -> Result<(), Error> {
    let nested_indent = format!("{indent}  ");
    if chosen == "else" {
        build_workspace_stmts_describe(
            &block.else_statements,
            &cursor.clone().with(CursorSegment::ElseBranch),
            member_crates,
            state_base,
            &nested_indent,
            out,
        )?;
    } else if let Ok(n) = chosen.parse::<usize>()
        && let Some(branch) = block.branches.get(n)
    {
        build_workspace_stmts_describe(
            &branch.statements,
            &cursor.clone().with(CursorSegment::IfBranch(n)),
            member_crates,
            state_base,
            &nested_indent,
            out,
        )?;
    }
    Ok(())
}

/// Completion icon for a non-`if`, non-`for crate` workspace statement.
fn workspace_stmt_icon(
    stmt: &WorkspaceStatement,
    cursor: &ProgramCursor,
    state_dir: &Path,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
) -> Result<&'static str, Error> {
    match stmt {
        WorkspaceStatement::Run(_) => run_icon(state_dir),
        WorkspaceStatement::WaitForContinue(_) => Ok(barrier_icon(state_dir)),
        WorkspaceStatement::WithEnvFile(block) => {
            let env_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
            Ok(bool_icon(is_workspace_stmts_completed(
                &block.statements,
                &env_prefix,
                member_crates,
                state_base,
            )?))
        }
        WorkspaceStatement::ManualStep(_) | WorkspaceStatement::SnapshotMetadata(_) => {
            Ok(bool_icon(is_workspace_stmt_completed(
                stmt,
                cursor,
                member_crates,
                state_base,
            )?))
        }
        WorkspaceStatement::If(_) | WorkspaceStatement::ForCrateInWorkspace(_) => Ok(ICON_PENDING),
    }
}

/// Recursively builds the describe lines for workspace statements.
fn build_workspace_stmts_describe(
    stmts: &[WorkspaceStatement],
    prefix: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
    indent: &str,
    out: &mut Vec<DescribeLine>,
) -> Result<(), Error> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());
        let cursor_str = cursor.to_path_string();

        match stmt {
            WorkspaceStatement::If(block) => {
                let chosen = read_chosen_branch_lossy(&state_dir);
                let (icon, label) = if_describe(&chosen);
                out.push(describe_line(indent, &cursor_str, icon, label));
                build_workspace_if_children(
                    block,
                    &cursor,
                    &chosen,
                    member_crates,
                    state_base,
                    indent,
                    out,
                )?;
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                build_workspace_for_crate(
                    stmt,
                    block,
                    &cursor,
                    &cursor_str,
                    member_crates,
                    state_base,
                    indent,
                    out,
                )?;
            }
            _ => {
                let icon =
                    workspace_stmt_icon(stmt, &cursor, &state_dir, member_crates, state_base)?;
                out.push(describe_line(
                    indent,
                    &cursor_str,
                    icon,
                    &workspace_stmt_label(stmt),
                ));
                if let WorkspaceStatement::WithEnvFile(block) = stmt {
                    let env_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                    let nested_indent = format!("{indent}  ");
                    build_workspace_stmts_describe(
                        &block.statements,
                        &env_prefix,
                        member_crates,
                        state_base,
                        &nested_indent,
                        out,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Displays the current execution status of every target in a task.
///
/// # Errors
///
/// Returns an error if the task cannot be loaded.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn task_describe_command(
    params: DescribeTaskParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let (program, resolved) = load_task_data(&params.name, &environment)?;
    let state_base = state_dir_for_task(&params.name, &environment)?;

    println!("Task: {}", params.name);

    let ws_stmts = first_workspace_stmts(&program);
    if !resolved.workspace_executions.is_empty() {
        println!("Workspaces:");
        for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate() {
            let done = is_workspace_completed(ws_idx, ws_exec, ws_stmts, &state_base)?;
            println!("  {} {}", bool_icon(done), ws_exec.manifest_dir.display());
            let mut lines = Vec::new();
            build_workspace_stmts_describe(
                ws_stmts,
                &ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx)),
                &ws_exec.member_crates,
                &state_base,
                "    ",
                &mut lines,
            )?;
            print_describe_lines(&lines);
        }
    }

    let crate_stmts = first_crate_stmts(&program);
    if !resolved.crate_executions.is_empty() {
        println!("Standalone crates:");
        for (c_idx, crate_exec) in resolved.crate_executions.iter().enumerate() {
            let done = is_standalone_crate_completed(c_idx, crate_stmts, &state_base)?;
            println!(
                "  {} {}",
                bool_icon(done),
                crate_exec.manifest_dir.display()
            );
            let mut lines = Vec::new();
            build_crate_stmts_describe(
                crate_stmts,
                &ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx)),
                &state_base,
                "    ",
                &mut lines,
            )?;
            print_describe_lines(&lines);
        }
    }

    Ok(())
}

/// Lists all tasks found in the tasks configuration directory.
///
/// # Errors
///
/// Returns an error if the tasks directory cannot be read.
#[instrument]
#[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
pub async fn task_list_command(environment: crate::Environment) -> Result<(), Error> {
    let tasks_dir = dir_path(&environment)?;

    if !tasks_dir.exists() {
        println!("No tasks found.");
        return Ok(());
    }

    println!("Existing tasks:");
    for entry in fs_err::read_dir(&tasks_dir)
        .map_err(|e| Error::CouldNotReadTasksDir(tasks_dir.clone(), e))?
    {
        let entry = entry.map_err(|e| Error::CouldNotReadTasksDir(tasks_dir.clone(), e))?;
        let path = entry.path();
        if path.is_dir()
            && let Some(task_name) = path.file_name().and_then(|s| s.to_str())
        {
            println!("- {task_name}");
        }
    }
    Ok(())
}

/// Dispatches the `task` subcommand.
///
/// # Errors
///
/// Propagates errors from the chosen subcommand.
#[instrument]
pub async fn task_command(
    task_parameters: TaskParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match task_parameters.sub_command {
        TaskSubCommand::Create(params) => {
            task_create_command(params, environment).await?;
        }
        TaskSubCommand::Remove(params) => {
            let task_dir = named_dir_path(&params.name, &environment)?;
            if !task_dir.exists() {
                return Err(Error::TaskNotFound(params.name));
            }
            fs_err::remove_dir_all(&task_dir)
                .map_err(|e| Error::CouldNotRemoveTaskDir(task_dir.clone(), e))?;
            // Also remove the execution-state directory (exit_status files,
            // snapshots, asciinema casts, barrier markers); otherwise a task
            // recreated with the same name inherits stale completion state.
            // The state directory may not exist if the task was never run.
            let state_dir = state_dir_for_task(&params.name, &environment)?;
            match fs_err::remove_dir_all(&state_dir) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::CouldNotRemoveTaskStateDir(state_dir, err)),
            }
        }
        TaskSubCommand::Run(params) => {
            task_run_command(params, environment).await?;
        }
        TaskSubCommand::List => {
            task_list_command(environment).await?;
        }
        TaskSubCommand::Describe(params) => {
            task_describe_command(params, environment).await?;
        }
        TaskSubCommand::Rewind(params) => {
            task_rewind_command(params, environment).await?;
        }
        TaskSubCommand::Continue(params) => {
            release_wait_barrier_command(params, environment).await?;
        }
    }
    Ok(())
}

/// Walks the program and collects every `wait_for_continue` cursor whose
/// barrier is currently in the *waiting* state on disk (state_dir exists but
/// `barrier_released` does not).
///
/// Returned tuples are `(cursor, description)` in document order, for use in
/// user-facing "execution suspended" messages.
fn find_waiting_barriers(
    program: &Program,
    resolved: &ResolvedProgram,
    state_base: &Path,
) -> Vec<(ProgramCursor, String)> {
    fn walk_crate(
        stmts: &[CrateStatement],
        prefix: &ProgramCursor,
        state_base: &Path,
        out: &mut Vec<(ProgramCursor, String)>,
    ) {
        for (i, stmt) in stmts.iter().enumerate() {
            let cursor = prefix.clone().with(CursorSegment::Statement(i));
            match stmt {
                CrateStatement::WaitForContinue(node) => {
                    let state_dir = state_base.join(cursor.to_path());
                    if is_wait_barrier_waiting(&state_dir) {
                        out.push((cursor, node.description.clone()));
                    }
                }
                CrateStatement::If(block) => {
                    for (b_idx, branch) in block.branches.iter().enumerate() {
                        let p = cursor.clone().with(CursorSegment::IfBranch(b_idx));
                        walk_crate(&branch.statements, &p, state_base, out);
                    }
                    let p = cursor.clone().with(CursorSegment::ElseBranch);
                    walk_crate(&block.else_statements, &p, state_base, out);
                }
                CrateStatement::WithEnvFile(block) => {
                    let p = cursor.clone().with(CursorSegment::WithEnvFile);
                    walk_crate(&block.statements, &p, state_base, out);
                }
                CrateStatement::Run(_)
                | CrateStatement::ManualStep(_)
                | CrateStatement::SnapshotMetadata(_) => {}
            }
        }
    }
    fn walk_workspace(
        stmts: &[WorkspaceStatement],
        prefix: &ProgramCursor,
        member_crates: &[ResolvedCrateExecution],
        state_base: &Path,
        out: &mut Vec<(ProgramCursor, String)>,
    ) {
        for (i, stmt) in stmts.iter().enumerate() {
            let cursor = prefix.clone().with(CursorSegment::Statement(i));
            match stmt {
                WorkspaceStatement::WaitForContinue(node) => {
                    let state_dir = state_base.join(cursor.to_path());
                    if is_wait_barrier_waiting(&state_dir) {
                        out.push((cursor, node.description.clone()));
                    }
                }
                WorkspaceStatement::If(block) => {
                    for (b_idx, branch) in block.branches.iter().enumerate() {
                        let p = cursor.clone().with(CursorSegment::IfBranch(b_idx));
                        walk_workspace(&branch.statements, &p, member_crates, state_base, out);
                    }
                    let p = cursor.clone().with(CursorSegment::ElseBranch);
                    walk_workspace(&block.else_statements, &p, member_crates, state_base, out);
                }
                WorkspaceStatement::WithEnvFile(block) => {
                    let p = cursor.clone().with(CursorSegment::WithEnvFile);
                    walk_workspace(&block.statements, &p, member_crates, state_base, out);
                }
                WorkspaceStatement::ForCrateInWorkspace(block) => {
                    for (c_idx, _) in member_crates.iter().enumerate() {
                        let p = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
                        walk_crate(&block.statements, &p, state_base, out);
                    }
                }
                WorkspaceStatement::Run(_)
                | WorkspaceStatement::ManualStep(_)
                | WorkspaceStatement::SnapshotMetadata(_) => {}
            }
        }
    }

    let mut out = Vec::new();
    let ws_stmts = first_workspace_stmts(program);
    for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate() {
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
        walk_workspace(
            ws_stmts,
            &prefix,
            &ws_exec.member_crates,
            state_base,
            &mut out,
        );
    }
    let crate_stmts = first_crate_stmts(program);
    for (c_idx, _crate_exec) in resolved.crate_executions.iter().enumerate() {
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
        walk_crate(crate_stmts, &prefix, state_base, &mut out);
    }
    out
}

/// Outcome of resolving a [`ProgramCursor`] against a [`Program`]'s
/// statement tree.
///
/// Used by `task continue` to distinguish "the cursor doesn't address any
/// statement at all" (likely a typo or a stale path that pre-dates a
/// program edit) from "the cursor addresses a real statement, but that
/// statement isn't a `wait_for_continue` barrier" — these surface different
/// error messages to the user.
#[derive(Debug, PartialEq, Eq)]
enum CursorTarget {
    /// Cursor resolves to a `wait_for_continue` statement.
    WaitForContinue,
    /// Cursor resolves to a real statement, but it isn't `wait_for_continue`.
    OtherStatement,
    /// Cursor structure does not match the program: a segment indexes past
    /// the end of its scope, or its segment kind doesn't fit at that
    /// position (e.g. a `CrateIteration` at a top-level `for workspace`).
    NotInProgram,
}

/// Walks `cursor` through `program` and classifies what statement, if any,
/// it addresses.
///
/// This is `task continue`'s sole way of telling apart the two reasons a
/// continue request can fail without an explicit parse error — see
/// [`CursorTarget`] for the distinction.
fn cursor_targets_wait_for_continue(program: &Program, cursor: &ProgramCursor) -> CursorTarget {
    fn walk_crate(stmts: &[CrateStatement], segs: &[CursorSegment]) -> CursorTarget {
        let Some((first, rest)) = segs.split_first() else {
            return CursorTarget::NotInProgram;
        };
        let CursorSegment::Statement(n) = *first else {
            return CursorTarget::NotInProgram;
        };
        let Some(stmt) = stmts.get(n) else {
            return CursorTarget::NotInProgram;
        };
        if rest.is_empty() {
            return if matches!(stmt, CrateStatement::WaitForContinue(_)) {
                CursorTarget::WaitForContinue
            } else {
                CursorTarget::OtherStatement
            };
        }
        match (stmt, rest.split_first()) {
            (CrateStatement::If(block), Some((CursorSegment::IfBranch(b), rest))) => block
                .branches
                .get(*b)
                .map_or(CursorTarget::NotInProgram, |branch| {
                    walk_crate(&branch.statements, rest)
                }),
            (CrateStatement::If(block), Some((CursorSegment::ElseBranch, rest))) => {
                walk_crate(&block.else_statements, rest)
            }
            (CrateStatement::WithEnvFile(block), Some((CursorSegment::WithEnvFile, rest))) => {
                walk_crate(&block.statements, rest)
            }
            _ => CursorTarget::NotInProgram,
        }
    }
    fn walk_workspace(stmts: &[WorkspaceStatement], segs: &[CursorSegment]) -> CursorTarget {
        let Some((first, rest)) = segs.split_first() else {
            return CursorTarget::NotInProgram;
        };
        let CursorSegment::Statement(n) = *first else {
            return CursorTarget::NotInProgram;
        };
        let Some(stmt) = stmts.get(n) else {
            return CursorTarget::NotInProgram;
        };
        if rest.is_empty() {
            return if matches!(stmt, WorkspaceStatement::WaitForContinue(_)) {
                CursorTarget::WaitForContinue
            } else {
                CursorTarget::OtherStatement
            };
        }
        match (stmt, rest.split_first()) {
            (WorkspaceStatement::If(block), Some((CursorSegment::IfBranch(b), rest))) => block
                .branches
                .get(*b)
                .map_or(CursorTarget::NotInProgram, |branch| {
                    walk_workspace(&branch.statements, rest)
                }),
            (WorkspaceStatement::If(block), Some((CursorSegment::ElseBranch, rest))) => {
                walk_workspace(&block.else_statements, rest)
            }
            (WorkspaceStatement::WithEnvFile(block), Some((CursorSegment::WithEnvFile, rest))) => {
                walk_workspace(&block.statements, rest)
            }
            (
                WorkspaceStatement::ForCrateInWorkspace(block),
                Some((CursorSegment::CrateIteration(_), rest)),
            ) => walk_crate(&block.statements, rest),
            _ => CursorTarget::NotInProgram,
        }
    }

    let Some((first, rest)) = cursor.segments().split_first() else {
        return CursorTarget::NotInProgram;
    };
    // Parser already enforces at most one top-level `for workspace` / `for
    // crate` block (see `validate_unique_top_level_blocks`), so `find` is
    // sufficient — but if no matching block exists at all, the cursor
    // doesn't address anything.
    match first {
        CursorSegment::WorkspaceIteration(_) => program
            .statements
            .iter()
            .find_map(|s| match s {
                GlobalStatement::ForWorkspace(b) => Some(walk_workspace(&b.statements, rest)),
                _ => None,
            })
            .unwrap_or(CursorTarget::NotInProgram),
        CursorSegment::CrateIteration(_) => program
            .statements
            .iter()
            .find_map(|s| match s {
                GlobalStatement::ForCrate(b) => Some(walk_crate(&b.statements, rest)),
                _ => None,
            })
            .unwrap_or(CursorTarget::NotInProgram),
        _ => CursorTarget::NotInProgram,
    }
}

/// Releases a wait barrier so execution can continue past it.
///
/// # Errors
///
/// Returns an error if the task does not exist, the program cannot be loaded,
/// the cursor cannot be parsed, the cursor does not address a
/// `wait_for_continue` statement in the program, or the state files cannot be
/// written.
#[instrument]
pub async fn release_wait_barrier_command(
    params: ContinueBarrierParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let cursor = ProgramCursor::from_path_string(&params.cursor)
        .map_err(|e| Error::InvalidCursorString(params.cursor.clone(), e.to_string()))?;
    // Loading the program also verifies the task exists (TaskNotFound).
    let (program, _resolved) = load_task_data(&params.name, &environment)?;
    match cursor_targets_wait_for_continue(&program, &cursor) {
        CursorTarget::WaitForContinue => {}
        CursorTarget::OtherStatement => {
            return Err(Error::CursorNotAtBarrier(cursor.to_path_string()));
        }
        CursorTarget::NotInProgram => {
            return Err(Error::CursorNotInProgram(cursor.to_path_string()));
        }
    }
    let state_base = state_dir_for_task(&params.name, &environment)?;
    let state_dir = state_base.join(cursor.to_path());
    if !state_dir.exists() {
        crate::utils::create_user_dir_all(&state_dir)
            .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;
    }
    let release_file = state_dir.join("barrier_released");
    crate::utils::write_user_file(&release_file, "")
        .map_err(|e| Error::CouldNotWriteStateFile(release_file.clone(), e))?;
    println!(
        "Barrier at {} released. Execution can continue.",
        cursor.to_path_string()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helpers panic on unexpected match arms; clearer than assert with message"
    )]

    use std::path::{Path, PathBuf};

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use std::collections::BTreeSet;

    use super::{
        ContinueBarrierParameters, CursorTarget, DescribeLine, DescribeTaskParameters, NextOutcome,
        RewindSingleStepParameters, RewindSingleTargetParameters, RunSingleStepParameters,
        RunSingleTargetParameters, StatementAction, StepOutcome, build_crate_stmts_describe,
        build_workspace_stmts_describe, crate_stmt_label, cursor_targets_wait_for_continue,
        dir_path, evaluate_crate_if_block, evaluate_workspace_if_block, expand_interpolations,
        find_last_completed_crate_stmt, find_last_completed_workspace_stmt,
        find_next_in_crate_stmts, find_next_in_workspace_stmts, find_next_statement,
        find_waiting_barriers, is_crate_stmt_completed, is_run_completed,
        is_workspace_stmt_completed, manifest_hex_key, named_dir_path, parse_env_file_content,
        program_has_interactive_steps, release_wait_barrier_command, resolve_interpolation,
        rewind_single_step_command, rewind_single_target_command, run_crate_if_block,
        run_crate_stmts_to_completion, run_single_step_command, run_single_target_command,
        run_workspace_if_block, run_workspace_stmts_to_completion, state_dir_for_task,
        task_describe_command, task_list_command, validate_task_name, workspace_stmt_label,
    };
    use crate::error::Error;
    use crate::program::ast::common::{
        AtLeastTwo, Branch, CommonCondition, IfBlock, ManualStepNode, NonEmptyBranches, RunStep,
        SnapshotMetadataNode, WaitForContinueNode, WithEnvFileBlock,
    };
    use crate::program::ast::crate_ctx::CrateStatement;
    use crate::program::ast::crate_ctx::{CrateCondition, CrateIfBlock, ForCrateBlock};
    use crate::program::ast::workspace_ctx::{
        ForCrateInWorkspaceBlock, ForWorkspaceBlock, WorkspaceCondition, WorkspaceIfBlock,
        WorkspaceStatement,
    };
    use crate::program::cursor::{CursorSegment, ProgramCursor};
    use crate::program::resolve::{
        ResolvedCrateExecution, ResolvedProgram, ResolvedWorkspaceExecution,
    };
    use crate::program::{GlobalStatement, Program};
    use crate::targets::{CrateType, TargetKind};
    use crate::{Config, Crate, Environment, Workspace};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Build a minimal test environment pointing at `temp_dir`.
    fn make_environment(temp_dir: &tempfile::TempDir) -> Environment {
        Environment {
            config_dir: temp_dir.path().join("config"),
            state_dir: temp_dir.path().join("state"),
            paths: vec![],
            suppress_subprocess_output: true,
        }
    }

    /// Create the state directory for the given cursor and return its path.
    fn make_cursor_state_dir(
        state_base: &Path,
        cursor: &ProgramCursor,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let dir = state_base.join(cursor.to_path());
        crate::utils::create_user_dir_all(&dir)?;
        Ok(dir)
    }

    /// Build a simple program containing one `for crate` block.
    fn crate_program(stmts: Vec<CrateStatement>) -> Program {
        Program {
            statements: vec![GlobalStatement::ForCrate(ForCrateBlock {
                statements: stmts,
            })],
        }
    }

    /// Build a simple program containing one `for workspace` block.
    fn workspace_program(stmts: Vec<WorkspaceStatement>) -> Program {
        Program {
            statements: vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: stmts,
            })],
        }
    }

    /// Build a `ResolvedProgram` with a single standalone crate at `manifest_dir`.
    fn resolved_with_one_crate(manifest_dir: PathBuf) -> ResolvedProgram {
        ResolvedProgram {
            workspace_executions: vec![],
            crate_executions: vec![ResolvedCrateExecution {
                manifest_dir,
                dependencies: vec![],
            }],
        }
    }

    /// Build a `ResolvedProgram` with a single workspace at `manifest_dir`.
    fn resolved_with_one_workspace(manifest_dir: PathBuf) -> ResolvedProgram {
        ResolvedProgram {
            workspace_executions: vec![ResolvedWorkspaceExecution {
                manifest_dir,
                dependencies: vec![],
                member_crates: vec![],
            }],
            crate_executions: vec![],
        }
    }

    // ── statement builders shared across tests ────────────────────────────────

    fn run_step(command: &str, args: &[&str]) -> RunStep {
        RunStep {
            command: command.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    /// A crate `if <standalone>` block with the given then/else bodies.
    fn crate_if_standalone(
        then_stmts: Vec<CrateStatement>,
        else_stmts: Vec<CrateStatement>,
    ) -> CrateStatement {
        CrateStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: then_stmts,
                },
                vec![],
            ),
            else_statements: else_stmts,
        })
    }

    /// A workspace `if <standalone>` block with the given then/else bodies.
    fn workspace_if_standalone(
        then_stmts: Vec<WorkspaceStatement>,
        else_stmts: Vec<WorkspaceStatement>,
    ) -> WorkspaceStatement {
        WorkspaceStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: WorkspaceCondition::Standalone,
                    statements: then_stmts,
                },
                vec![],
            ),
            else_statements: else_stmts,
        })
    }

    // ── statement labels ──────────────────────────────────────────────────────

    #[test]
    fn crate_stmt_label_all_variants() {
        assert_eq!(
            crate_stmt_label(&CrateStatement::Run(run_step(
                "cargo",
                &["build", "--release"]
            ))),
            r#"run "cargo" "build" "--release""#,
        );
        assert_eq!(
            crate_stmt_label(&CrateStatement::Run(run_step("ls", &[]))),
            r#"run "ls""#,
        );
        assert_eq!(
            crate_stmt_label(&CrateStatement::ManualStep(ManualStepNode {
                title: "Review".to_owned(),
                instructions: "do it".to_owned(),
            })),
            r#"manual_step "Review""#,
        );
        assert_eq!(
            crate_stmt_label(&CrateStatement::SnapshotMetadata(SnapshotMetadataNode {
                name: "snap".to_owned(),
            })),
            r#"snapshot_metadata "snap""#,
        );
        assert_eq!(
            crate_stmt_label(&crate_if_standalone(vec![], vec![])),
            "if ...",
        );
        assert_eq!(
            crate_stmt_label(&CrateStatement::WithEnvFile(WithEnvFileBlock {
                env_file: ".env".to_owned(),
                statements: vec![],
            })),
            r#"with_env_file ".env""#,
        );
        assert_eq!(
            crate_stmt_label(&CrateStatement::WaitForContinue(WaitForContinueNode {
                description: "hold".to_owned(),
            })),
            r#"wait_for_continue "hold""#,
        );
    }

    #[test]
    fn workspace_stmt_label_all_variants() {
        assert_eq!(
            workspace_stmt_label(&WorkspaceStatement::Run(run_step("cargo", &["test"]))),
            r#"run "cargo" "test""#,
        );
        assert_eq!(
            workspace_stmt_label(&WorkspaceStatement::ManualStep(ManualStepNode {
                title: "Tag".to_owned(),
                instructions: "tag it".to_owned(),
            })),
            r#"manual_step "Tag""#,
        );
        assert_eq!(
            workspace_stmt_label(&WorkspaceStatement::SnapshotMetadata(
                SnapshotMetadataNode {
                    name: "meta".to_owned(),
                }
            )),
            r#"snapshot_metadata "meta""#,
        );
        assert_eq!(
            workspace_stmt_label(&workspace_if_standalone(vec![], vec![])),
            "if ...",
        );
        assert_eq!(
            workspace_stmt_label(&WorkspaceStatement::ForCrateInWorkspace(
                ForCrateInWorkspaceBlock { statements: vec![] },
            )),
            "for crate in workspace",
        );
        assert_eq!(
            workspace_stmt_label(&WorkspaceStatement::WithEnvFile(WithEnvFileBlock {
                env_file: "shared.env".to_owned(),
                statements: vec![],
            })),
            r#"with_env_file "shared.env""#,
        );
        assert_eq!(
            workspace_stmt_label(&WorkspaceStatement::WaitForContinue(WaitForContinueNode {
                description: "pause".to_owned(),
            })),
            r#"wait_for_continue "pause""#,
        );
    }

    // ── is_run_completed / is_run_failed ──────────────────────────────────────

    #[test]
    fn run_completed_no_state_dir() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        assert!(!is_run_completed(&state_dir)?);
        assert!(!super::is_run_failed(&state_dir)?);
        Ok(())
    }

    #[test]
    fn run_completed_no_exit_status_file() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        assert!(!is_run_completed(&state_dir)?);
        assert!(!super::is_run_failed(&state_dir)?);
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_zero() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        crate::utils::write_user_file(state_dir.join("exit_status"), "0")?;
        assert!(is_run_completed(&state_dir)?);
        assert!(!super::is_run_failed(&state_dir)?);
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_nonzero() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        crate::utils::write_user_file(state_dir.join("exit_status"), "1")?;
        assert!(!is_run_completed(&state_dir)?);
        assert!(super::is_run_failed(&state_dir)?);
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_exec_failed_marker() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        crate::utils::write_user_file(state_dir.join("exit_status"), "exec failed")?;
        assert!(!is_run_completed(&state_dir)?);
        assert!(super::is_run_failed(&state_dir)?);
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_invalid_is_error() -> TestResult {
        // Anything that isn't `"0"`, a valid `i32`, or the
        // "exec failed" marker must surface as an explicit error
        // instead of being silently classified as a failure.
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        for bad in ["", "not-a-number", "0xff", "1.5", "exec_failed"] {
            crate::utils::write_user_file(state_dir.join("exit_status"), bad)?;
            match is_run_completed(&state_dir) {
                Err(Error::InvalidRecordedExitStatus(s)) => assert_eq!(s, bad),
                other => panic!("expected InvalidRecordedExitStatus({bad:?}), got {other:?}"),
            }
            match super::is_run_failed(&state_dir) {
                Err(Error::InvalidRecordedExitStatus(s)) => assert_eq!(s, bad),
                other => panic!("expected InvalidRecordedExitStatus({bad:?}), got {other:?}"),
            }
        }
        Ok(())
    }

    // ── is_crate_stmt_completed ───────────────────────────────────────────────

    #[test]
    fn crate_run_stmt_completed_when_exit_zero() -> TestResult {
        let temp = tempdir()?;
        let cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let state_dir = temp.path().join(cursor.to_path());
        crate::utils::create_user_dir_all(&state_dir)?;
        crate::utils::write_user_file(state_dir.join("exit_status"), "0")?;

        let stmt = CrateStatement::Run(RunStep {
            command: "echo".to_owned(),
            args: vec![],
        });
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        Ok(())
    }

    #[test]
    fn crate_run_stmt_not_completed_when_no_dir() -> TestResult {
        let temp = tempdir()?;
        let cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));

        let stmt = CrateStatement::Run(RunStep {
            command: "echo".to_owned(),
            args: vec![],
        });
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        Ok(())
    }

    // ── find_next_statement ───────────────────────────────────────────────────

    #[test]
    fn find_next_returns_none_when_all_completed() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp");
        let cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let stmt_dir = make_cursor_state_dir(&state_base, &cursor)?;
        crate::utils::write_user_file(stmt_dir.join("exit_status"), "0")?;

        let program = crate_program(vec![CrateStatement::Run(RunStep {
            command: "echo".to_owned(),
            args: vec![],
        })]);
        let resolved = resolved_with_one_crate(dir);
        assert!(matches!(
            find_next_statement(&program, &resolved, &state_base)?,
            NextOutcome::Done
        ));
        Ok(())
    }

    #[test]
    fn find_next_returns_first_stmt_when_nothing_run() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp");

        let program = crate_program(vec![CrateStatement::Run(RunStep {
            command: "echo".to_owned(),
            args: vec![],
        })]);
        let resolved = resolved_with_one_crate(dir);
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base)? else {
            return Err("expected NextOutcome::Next".into());
        };
        assert_eq!(
            next.cursor,
            ProgramCursor::new()
                .with(CursorSegment::CrateIteration(0))
                .with(CursorSegment::Statement(0))
        );
        Ok(())
    }

    #[test]
    fn find_next_skips_completed_and_returns_second() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp");

        // Mark first statement completed.
        let cursor0 = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let stmt0_dir = make_cursor_state_dir(&state_base, &cursor0)?;
        crate::utils::write_user_file(stmt0_dir.join("exit_status"), "0")?;

        let program = crate_program(vec![
            CrateStatement::Run(RunStep {
                command: "echo".to_owned(),
                args: vec!["a".to_owned()],
            }),
            CrateStatement::Run(RunStep {
                command: "echo".to_owned(),
                args: vec!["b".to_owned()],
            }),
        ]);
        let resolved = resolved_with_one_crate(dir);
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base)? else {
            return Err("expected NextOutcome::Next".into());
        };
        assert_eq!(
            next.cursor,
            ProgramCursor::new()
                .with(CursorSegment::CrateIteration(0))
                .with(CursorSegment::Statement(1))
        );
        Ok(())
    }

    #[test]
    fn find_next_returns_failed_stmt_for_retry() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp");

        let cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let stmt_dir = make_cursor_state_dir(&state_base, &cursor)?;
        crate::utils::write_user_file(stmt_dir.join("exit_status"), "1")?; // failed

        let program = crate_program(vec![CrateStatement::Run(RunStep {
            command: "echo".to_owned(),
            args: vec![],
        })]);
        let resolved = resolved_with_one_crate(dir);
        let outcome = find_next_statement(&program, &resolved, &state_base)?;
        assert!(
            matches!(outcome, NextOutcome::Next(_)),
            "Failed statement should be returned for retry"
        );
        Ok(())
    }

    #[test]
    fn find_next_workspace_stmt() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp");

        let program = workspace_program(vec![WorkspaceStatement::Run(RunStep {
            command: "cargo".to_owned(),
            args: vec!["build".to_owned()],
        })]);
        let resolved = resolved_with_one_workspace(dir);
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base)? else {
            return Err("expected NextOutcome::Next".into());
        };
        assert_eq!(
            next.cursor,
            ProgramCursor::new()
                .with(CursorSegment::WorkspaceIteration(0))
                .with(CursorSegment::Statement(0))
        );
        Ok(())
    }

    /// Regression test for KNOWN_ISSUES.md §1a: when every member crate of a
    /// `for crate in workspace` block is suspended at a `wait_for_continue`
    /// barrier, `find_next_statement` must report `Suspended` and not walk
    /// past the for-crate block to a later workspace-level statement.
    #[test]
    fn find_next_reports_suspended_when_for_crate_member_at_barrier() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let ws_dir = PathBuf::from("/tmp/ws");
        let crate_dir = PathBuf::from("/tmp/ws/member");

        // for workspace { for crate in workspace { wait_for_continue ... } ;
        //                 run echo "after" }
        let program = Program {
            statements: vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: vec![
                    WorkspaceStatement::ForCrateInWorkspace(ForCrateInWorkspaceBlock {
                        statements: vec![CrateStatement::WaitForContinue(WaitForContinueNode {
                            description: "pause".to_owned(),
                        })],
                    }),
                    WorkspaceStatement::Run(RunStep {
                        command: "echo".to_owned(),
                        args: vec!["after".to_owned()],
                    }),
                ],
            })],
        };
        let resolved = ResolvedProgram {
            workspace_executions: vec![ResolvedWorkspaceExecution {
                manifest_dir: ws_dir,
                dependencies: vec![],
                member_crates: vec![ResolvedCrateExecution {
                    manifest_dir: crate_dir,
                    dependencies: vec![],
                }],
            }],
            crate_executions: vec![],
        };

        // Put the barrier into the "waiting" state by creating its state_dir
        // without the barrier_released marker.
        let barrier_cursor = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(0)) // ForCrateInWorkspace
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0)); // WaitForContinue
        let _state_dir = make_cursor_state_dir(&state_base, &barrier_cursor)?;

        let outcome = find_next_statement(&program, &resolved, &state_base)?;
        assert!(
            matches!(outcome, NextOutcome::Suspended),
            "expected Suspended, got {outcome:?}"
        );
        Ok(())
    }

    /// Regression test for KNOWN_ISSUES.md §1: when the barrier has not yet
    /// been reached (no state_dir for it), the barrier should be surfaced as
    /// the next action, not a Suspended outcome.
    #[test]
    fn find_next_returns_barrier_action_when_not_yet_reached() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp/c");

        let program = crate_program(vec![CrateStatement::WaitForContinue(WaitForContinueNode {
            description: "pause".to_owned(),
        })]);
        let resolved = resolved_with_one_crate(dir);
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base)? else {
            return Err("expected NextOutcome::Next for pending barrier".into());
        };
        assert_eq!(
            next.cursor,
            ProgramCursor::new()
                .with(CursorSegment::CrateIteration(0))
                .with(CursorSegment::Statement(0))
        );
        Ok(())
    }

    /// Regression test for KNOWN_ISSUES.md §1: a barrier in the *released*
    /// state should be transparent — `find_next` should look past it to the
    /// next statement.
    #[test]
    fn find_next_walks_past_released_barrier() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp/c");

        // Mark barrier released.
        let barrier_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let barrier_dir = make_cursor_state_dir(&state_base, &barrier_cursor)?;
        crate::utils::write_user_file(barrier_dir.join("barrier_released"), "")?;

        let program = crate_program(vec![
            CrateStatement::WaitForContinue(WaitForContinueNode {
                description: "pause".to_owned(),
            }),
            CrateStatement::Run(RunStep {
                command: "echo".to_owned(),
                args: vec!["after".to_owned()],
            }),
        ]);
        let resolved = resolved_with_one_crate(dir);
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base)? else {
            return Err("expected NextOutcome::Next past released barrier".into());
        };
        assert_eq!(
            next.cursor,
            ProgramCursor::new()
                .with(CursorSegment::CrateIteration(0))
                .with(CursorSegment::Statement(1))
        );
        Ok(())
    }

    // ── validate_task_name ────────────────────────────────────────────────────

    /// Regression test for KNOWN_ISSUES.md §10: a transient I/O error reading
    /// `chosen_branch` must propagate as an error, not be silently swallowed
    /// as "branch not yet chosen". The previous `Err(_) => …` pattern would
    /// have caused the runner to re-evaluate the if-block's branch
    /// conditions and overwrite the existing `chosen_branch` — picking a
    /// different branch than originally chosen because `ask_user` /
    /// `run`-style conditions are not deterministic.
    ///
    /// We simulate a non-NotFound I/O error by making `chosen_branch` itself
    /// a directory; `read_to_string` then fails with `IsADirectory` /
    /// `EISDIR`, which is not `NotFound`.
    #[test]
    fn find_next_propagates_chosen_branch_io_error() -> TestResult {
        use crate::program::ast::common::CommonCondition;
        use crate::program::ast::crate_ctx::{CrateBranch, CrateCondition, CrateIfBlock};

        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = env.state_dir.join("cargo-for-each").join("tasks").join("t");
        let dir = PathBuf::from("/tmp");

        // Construct a tiny if-block program.
        let program = crate_program(vec![CrateStatement::If(CrateIfBlock {
            branches: crate::program::ast::common::NonEmptyBranches::from_first_and_rest(
                CrateBranch {
                    condition: CrateCondition::Common(CommonCondition::WorkingDirectoryClean),
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![],
        })]);
        let resolved = resolved_with_one_crate(dir);

        // Force a non-NotFound read error by making chosen_branch a directory.
        let if_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let if_state_dir = make_cursor_state_dir(&state_base, &if_cursor)?;
        fs_err::create_dir_all(if_state_dir.join("chosen_branch"))?;

        let outcome = find_next_statement(&program, &resolved, &state_base);
        assert!(
            matches!(outcome, Err(Error::CouldNotReadStateFile(_, _))),
            "expected CouldNotReadStateFile, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn validate_task_name_accepts_plain_names() {
        for ok_name in &[
            "foo", "task_1", "my-task", "abc123", "résumé", // unicode
            "a",      // single char
        ] {
            assert!(
                validate_task_name(ok_name).is_ok(),
                "expected {ok_name:?} to be accepted"
            );
        }
    }

    #[test]
    fn validate_task_name_rejects_empty() {
        assert!(matches!(
            validate_task_name(""),
            Err(Error::InvalidTaskName(_, _))
        ));
    }

    #[test]
    fn validate_task_name_rejects_whitespace_padding() {
        for bad in &[" foo", "foo ", " foo ", "\tfoo", "foo\n"] {
            assert!(
                matches!(validate_task_name(bad), Err(Error::InvalidTaskName(_, _))),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_task_name_rejects_path_separators() {
        for bad in &["foo/bar", "foo\\bar", "/foo", "foo/", "\\foo"] {
            assert!(
                matches!(validate_task_name(bad), Err(Error::InvalidTaskName(_, _))),
                "expected {bad:?} to be rejected"
            );
        }
    }

    /// Regression test for KNOWN_ISSUES.md §9: `cargo-for-each task create
    /// --name ../../tmp/escape` previously wrote files outside the tasks
    /// directory. The validator must reject every form of parent-traversal,
    /// current-dir reference, and absolute path.
    #[test]
    fn validate_task_name_rejects_traversal_and_special_components() {
        for bad in &[
            "..",
            ".",
            "../foo",
            "./foo",
            "/",
            "/etc",
            "../../tmp/escape",
        ] {
            assert!(
                matches!(validate_task_name(bad), Err(Error::InvalidTaskName(_, _))),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_task_name_rejects_nul_byte() {
        assert!(matches!(
            validate_task_name("foo\0bar"),
            Err(Error::InvalidTaskName(_, _))
        ));
    }

    /// Build a `for workspace` program with two statements: a `run` step,
    /// then a `wait_for_continue`. Used by the cursor-classifier tests.
    fn program_with_run_then_barrier() -> Program {
        Program {
            statements: vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: vec![
                    WorkspaceStatement::Run(RunStep {
                        command: "true".to_owned(),
                        args: vec![],
                    }),
                    WorkspaceStatement::WaitForContinue(WaitForContinueNode {
                        description: "ready".to_owned(),
                    }),
                ],
            })],
        }
    }

    #[test]
    fn cursor_classifier_finds_wait_for_continue() {
        let program = program_with_run_then_barrier();
        // w0/s1 → the WaitForContinue.
        let cursor = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(1));
        assert_eq!(
            cursor_targets_wait_for_continue(&program, &cursor),
            CursorTarget::WaitForContinue,
        );
    }

    #[test]
    fn cursor_classifier_reports_other_statement_for_non_barrier() {
        let program = program_with_run_then_barrier();
        // w0/s0 → the Run step, a real statement but not a barrier.
        let cursor = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(0));
        assert_eq!(
            cursor_targets_wait_for_continue(&program, &cursor),
            CursorTarget::OtherStatement,
        );
    }

    #[test]
    fn cursor_classifier_reports_not_in_program_for_out_of_range_statement() {
        let program = program_with_run_then_barrier();
        // w0/s99 → statement index past the end.
        let cursor = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(99));
        assert_eq!(
            cursor_targets_wait_for_continue(&program, &cursor),
            CursorTarget::NotInProgram,
        );
    }

    #[test]
    fn cursor_classifier_reports_not_in_program_for_missing_top_level_block() {
        let program = program_with_run_then_barrier();
        // The program has no `for crate` block, so a c-rooted cursor cannot
        // resolve.
        let cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        assert_eq!(
            cursor_targets_wait_for_continue(&program, &cursor),
            CursorTarget::NotInProgram,
        );
    }

    #[test]
    fn cursor_classifier_reports_not_in_program_for_empty_cursor() {
        let program = program_with_run_then_barrier();
        let cursor = ProgramCursor::new();
        assert_eq!(
            cursor_targets_wait_for_continue(&program, &cursor),
            CursorTarget::NotInProgram,
        );
    }

    // ── parse_env_file_content ────────────────────────────────────────────────

    #[test]
    fn parse_env_handles_comments_blanks_export_and_quotes() {
        let content = "\
# a comment
   # indented comment

FOO=bar
export BAZ=qux
QUOTED=\"with spaces\"
SINGLE='single quoted'
  SPACED = trimmed
EMPTY=
=no_key
not_a_pair
";
        let vars = parse_env_file_content(content);
        assert_eq!(
            vars,
            vec![
                ("FOO".to_owned(), "bar".to_owned()),
                ("BAZ".to_owned(), "qux".to_owned()),
                ("QUOTED".to_owned(), "with spaces".to_owned()),
                ("SINGLE".to_owned(), "single quoted".to_owned()),
                ("SPACED".to_owned(), "trimmed".to_owned()),
                ("EMPTY".to_owned(), String::new()),
            ],
        );
    }

    #[test]
    fn parse_env_keeps_duplicate_keys_in_order() {
        let vars = parse_env_file_content("K=1\nK=2\n");
        assert_eq!(
            vars,
            vec![
                ("K".to_owned(), "1".to_owned()),
                ("K".to_owned(), "2".to_owned())
            ],
        );
    }

    // ── interpolation ─────────────────────────────────────────────────────────

    /// Writes a one-package metadata snapshot under `state_base` for `name`,
    /// keyed by `manifest_dir`, and returns nothing. `package` is the JSON for
    /// the single package (its `manifest_path` must match `manifest_dir`).
    fn write_snapshot(
        state_base: &Path,
        name: &str,
        manifest_dir: &Path,
        package: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut filename = manifest_hex_key(manifest_dir)?;
        filename.push_str(".json");
        let dir = state_base.join("snapshots").join(name).join("by_manifest");
        crate::utils::create_user_dir_all(&dir)?;
        let root = serde_json::json!({ "packages": [package] });
        crate::utils::write_user_file(dir.join(filename), root.to_string())?;
        Ok(())
    }

    #[test]
    fn expand_interpolations_passes_through_without_reference() -> TestResult {
        let temp = tempdir()?;
        assert_eq!(
            expand_interpolations("plain text", temp.path(), temp.path())?,
            "plain text",
        );
        Ok(())
    }

    #[test]
    fn expand_interpolations_rejects_unterminated_and_missing_dot() -> TestResult {
        let temp = tempdir()?;
        match expand_interpolations("a ${unterminated", temp.path(), temp.path()) {
            Err(Error::InvalidInterpolation(_)) => {}
            other => return Err(format!("expected InvalidInterpolation, got {other:?}").into()),
        }
        match expand_interpolations("${nodot}", temp.path(), temp.path()) {
            Err(Error::InvalidInterpolation(r)) => assert_eq!(r, "nodot"),
            other => return Err(format!("expected InvalidInterpolation, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn resolve_interpolation_reads_string_and_numeric_fields() -> TestResult {
        let temp = tempdir()?;
        let manifest_dir = fs_err::canonicalize(temp.path())?;
        let state_base = manifest_dir.join("state");
        let manifest_path = manifest_dir.join("Cargo.toml");
        write_snapshot(
            &state_base,
            "snap",
            &manifest_dir,
            serde_json::json!({
                "manifest_path": manifest_path.to_string_lossy(),
                "name": "demo",
                "version": "1.2.3",
                "rust_version": 42,
            }),
        )?;

        assert_eq!(
            resolve_interpolation("snap", "version", &manifest_dir, &state_base)?,
            "1.2.3",
        );
        // A non-string JSON value is rendered via its `to_string`.
        assert_eq!(
            resolve_interpolation("snap", "rust_version", &manifest_dir, &state_base)?,
            "42",
        );
        // expand_interpolations splices the resolved value back into the text.
        assert_eq!(
            expand_interpolations("v${snap.version}!", &manifest_dir, &state_base)?,
            "v1.2.3!",
        );
        Ok(())
    }

    #[test]
    fn resolve_interpolation_error_paths() -> TestResult {
        let temp = tempdir()?;
        let manifest_dir = fs_err::canonicalize(temp.path())?;
        let state_base = manifest_dir.join("state");

        // No snapshot written at all.
        match resolve_interpolation("missing", "version", &manifest_dir, &state_base) {
            Err(Error::SnapshotNotFound(n)) => assert_eq!(n, "missing"),
            other => return Err(format!("expected SnapshotNotFound, got {other:?}").into()),
        }

        // Snapshot present but no package matches this manifest dir.
        write_snapshot(
            &state_base,
            "snap",
            &manifest_dir,
            serde_json::json!({
                "manifest_path": "/somewhere/else/Cargo.toml",
                "version": "9",
            }),
        )?;
        match resolve_interpolation("snap", "version", &manifest_dir, &state_base) {
            Err(Error::SnapshotPackageNotFound(n, _)) => assert_eq!(n, "snap"),
            other => return Err(format!("expected SnapshotPackageNotFound, got {other:?}").into()),
        }

        // Matching package, but the requested field does not exist.
        write_snapshot(
            &state_base,
            "snap2",
            &manifest_dir,
            serde_json::json!({
                "manifest_path": manifest_dir.join("Cargo.toml").to_string_lossy(),
                "version": "9",
            }),
        )?;
        match resolve_interpolation("snap2", "no_such_field", &manifest_dir, &state_base) {
            Err(Error::SnapshotFieldNotFound(n, f)) => {
                assert_eq!(n, "snap2");
                assert_eq!(f, "no_such_field");
            }
            other => return Err(format!("expected SnapshotFieldNotFound, got {other:?}").into()),
        }
        Ok(())
    }

    // ── completion checks (manual / snapshot / barrier / if / env-file) ────────

    fn crate_stmt_state_dir(
        state_base: &Path,
        cursor: &ProgramCursor,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let dir = state_base.join(cursor.to_path());
        crate::utils::create_user_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn manual_step_completion_requires_y_marker() -> TestResult {
        let temp = tempdir()?;
        let cursor = ProgramCursor::new().with(CursorSegment::Statement(0));
        let stmt = CrateStatement::ManualStep(ManualStepNode {
            title: "t".to_owned(),
            instructions: "i".to_owned(),
        });
        // No state dir -> not completed.
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);

        let dir = crate_stmt_state_dir(temp.path(), &cursor)?;
        crate::utils::write_user_file(dir.join("manual_step_confirmed"), "n")?;
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);

        crate::utils::write_user_file(dir.join("manual_step_confirmed"), "y")?;
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        Ok(())
    }

    #[test]
    fn snapshot_metadata_completion_requires_marker_file() -> TestResult {
        let temp = tempdir()?;
        let cursor = ProgramCursor::new().with(CursorSegment::Statement(0));
        let stmt = CrateStatement::SnapshotMetadata(SnapshotMetadataNode {
            name: "s".to_owned(),
        });
        let dir = crate_stmt_state_dir(temp.path(), &cursor)?;
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        crate::utils::write_user_file(dir.join("snapshot_metadata_completed"), "done")?;
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        Ok(())
    }

    #[test]
    fn wait_barrier_completion_requires_released_marker() -> TestResult {
        let temp = tempdir()?;
        let cursor = ProgramCursor::new().with(CursorSegment::Statement(0));
        let stmt = CrateStatement::WaitForContinue(WaitForContinueNode {
            description: "d".to_owned(),
        });
        let dir = crate_stmt_state_dir(temp.path(), &cursor)?;
        // Waiting (dir exists, no release marker) -> not completed.
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        crate::utils::write_user_file(dir.join("barrier_released"), "")?;
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        Ok(())
    }

    #[test]
    fn if_completion_follows_chosen_branch() -> TestResult {
        let temp = tempdir()?;
        let cursor = ProgramCursor::new().with(CursorSegment::Statement(0));
        // if <cond> { run } with an else { run }
        let stmt = CrateStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: vec![CrateStatement::Run(run_step("a", &[]))],
                },
                vec![],
            ),
            else_statements: vec![CrateStatement::Run(run_step("b", &[]))],
        });
        let if_dir = crate_stmt_state_dir(temp.path(), &cursor)?;

        // No chosen_branch yet -> not completed.
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);

        // "none" (conditions all false, no else taken) counts as completed.
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "none")?;
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path())?);

        // Branch 0 chosen but its run statement not completed -> incomplete.
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "0")?;
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        let branch_run = cursor
            .clone()
            .with(CursorSegment::IfBranch(0))
            .with(CursorSegment::Statement(0));
        let run_dir = crate_stmt_state_dir(temp.path(), &branch_run)?;
        crate::utils::write_user_file(run_dir.join("exit_status"), "0")?;
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path())?);

        // Else branch chosen -> depends on the else body.
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "else")?;
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        let else_run = cursor
            .clone()
            .with(CursorSegment::ElseBranch)
            .with(CursorSegment::Statement(0));
        let else_dir = crate_stmt_state_dir(temp.path(), &else_run)?;
        crate::utils::write_user_file(else_dir.join("exit_status"), "0")?;
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path())?);
        Ok(())
    }

    #[test]
    fn workspace_for_crate_completion_requires_all_members() -> TestResult {
        let temp = tempdir()?;
        let cursor = ProgramCursor::new().with(CursorSegment::Statement(0));
        let stmt = WorkspaceStatement::ForCrateInWorkspace(ForCrateInWorkspaceBlock {
            statements: vec![CrateStatement::Run(run_step("x", &[]))],
        });
        let members = vec![
            ResolvedCrateExecution {
                manifest_dir: PathBuf::from("/a"),
                dependencies: vec![],
            },
            ResolvedCrateExecution {
                manifest_dir: PathBuf::from("/b"),
                dependencies: vec![],
            },
        ];

        // Complete member 0 only -> the block is not complete.
        let run0 = cursor
            .clone()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let dir0 = crate_stmt_state_dir(temp.path(), &run0)?;
        crate::utils::write_user_file(dir0.join("exit_status"), "0")?;
        assert!(!is_workspace_stmt_completed(
            &stmt,
            &cursor,
            &members,
            temp.path()
        )?);

        // Complete member 1 too -> now complete.
        let run = cursor
            .clone()
            .with(CursorSegment::CrateIteration(1))
            .with(CursorSegment::Statement(0));
        let dir = crate_stmt_state_dir(temp.path(), &run)?;
        crate::utils::write_user_file(dir.join("exit_status"), "0")?;
        assert!(is_workspace_stmt_completed(
            &stmt,
            &cursor,
            &members,
            temp.path()
        )?);
        Ok(())
    }

    // ── find_last_completed_* ─────────────────────────────────────────────────

    #[test]
    fn find_last_completed_crate_none_when_nothing_done() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![
            CrateStatement::Run(run_step("a", &[])),
            CrateStatement::Run(run_step("b", &[])),
        ];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        assert_eq!(
            find_last_completed_crate_stmt(&stmts, &prefix, temp.path())?,
            None,
        );
        Ok(())
    }

    #[test]
    fn find_last_completed_crate_returns_highest_completed_index() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![
            CrateStatement::Run(run_step("a", &[])),
            CrateStatement::Run(run_step("b", &[])),
            CrateStatement::Run(run_step("c", &[])),
        ];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        for i in [0_usize, 1] {
            let c = prefix.clone().with(CursorSegment::Statement(i));
            let dir = crate_stmt_state_dir(temp.path(), &c)?;
            crate::utils::write_user_file(dir.join("exit_status"), "0")?;
        }
        let expected = prefix.clone().with(CursorSegment::Statement(1));
        assert_eq!(
            find_last_completed_crate_stmt(&stmts, &prefix, temp.path())?,
            Some(expected),
        );
        Ok(())
    }

    #[test]
    fn find_last_completed_crate_descends_into_if_branch() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![CrateStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: vec![CrateStatement::Run(run_step("inner", &[]))],
                },
                vec![],
            ),
            else_statements: vec![],
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let if_dir = crate_stmt_state_dir(temp.path(), &if_cursor)?;
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "0")?;
        let inner = if_cursor
            .clone()
            .with(CursorSegment::IfBranch(0))
            .with(CursorSegment::Statement(0));
        let inner_dir = crate_stmt_state_dir(temp.path(), &inner)?;
        crate::utils::write_user_file(inner_dir.join("exit_status"), "0")?;

        assert_eq!(
            find_last_completed_crate_stmt(&stmts, &prefix, temp.path())?,
            Some(inner),
        );
        Ok(())
    }

    #[test]
    fn find_last_completed_workspace_returns_completed_run() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![
            WorkspaceStatement::Run(run_step("a", &[])),
            WorkspaceStatement::Run(run_step("b", &[])),
        ];
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));
        let c0 = prefix.clone().with(CursorSegment::Statement(0));
        let dir = crate_stmt_state_dir(temp.path(), &c0)?;
        crate::utils::write_user_file(dir.join("exit_status"), "0")?;

        assert_eq!(
            find_last_completed_workspace_stmt(&stmts, &prefix, &[], temp.path())?,
            Some(c0),
        );
        Ok(())
    }

    // ── program_has_interactive_steps ─────────────────────────────────────────

    #[test]
    fn interactive_detection_for_manual_steps_and_ask_user() {
        // manual_step in a crate program.
        let manual = crate_program(vec![CrateStatement::ManualStep(ManualStepNode {
            title: "t".to_owned(),
            instructions: "i".to_owned(),
        })]);
        assert!(program_has_interactive_steps(&manual));

        // ask_user condition nested in an if inside a workspace's for-crate block.
        let ask = workspace_program(vec![WorkspaceStatement::ForCrateInWorkspace(
            ForCrateInWorkspaceBlock {
                statements: vec![CrateStatement::If(IfBlock {
                    branches: NonEmptyBranches::from_first_and_rest(
                        Branch {
                            condition: CrateCondition::Common(CommonCondition::AskUser(
                                "ok?".to_owned(),
                            )),
                            statements: vec![],
                        },
                        vec![],
                    ),
                    else_statements: vec![],
                })],
            },
        )]);
        assert!(program_has_interactive_steps(&ask));

        // A workspace `if standalone` with no interactive body is not interactive.
        let nested_cond = workspace_program(vec![workspace_if_standalone(vec![], vec![])]);
        assert!(!program_has_interactive_steps(&nested_cond));

        // Pure run program is non-interactive.
        let plain = crate_program(vec![CrateStatement::Run(run_step("echo", &[]))]);
        assert!(!program_has_interactive_steps(&plain));
    }

    #[test]
    fn interactive_detection_sees_ask_user_under_not_and_or() {
        let cond = CrateCondition::Not(Box::new(CrateCondition::And(AtLeastTwo::from_pair(
            CrateCondition::Standalone,
            CrateCondition::Common(CommonCondition::AskUser("q".to_owned())),
        ))));
        let prog = crate_program(vec![CrateStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: cond,
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![],
        })]);
        assert!(program_has_interactive_steps(&prog));
    }

    // ── find_waiting_barriers ─────────────────────────────────────────────────

    #[test]
    fn waiting_barriers_only_reports_waiting_state() -> TestResult {
        let temp = tempdir()?;
        let program = crate_program(vec![
            CrateStatement::Run(run_step("a", &[])),
            CrateStatement::WaitForContinue(WaitForContinueNode {
                description: "hold here".to_owned(),
            }),
        ]);
        let resolved = resolved_with_one_crate(PathBuf::from("/tmp"));
        let barrier_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(1));

        // Pending (no dir) -> nothing reported.
        assert!(find_waiting_barriers(&program, &resolved, temp.path()).is_empty());

        // Waiting (dir exists, no release marker) -> reported.
        crate_stmt_state_dir(temp.path(), &barrier_cursor)?;
        assert_eq!(
            find_waiting_barriers(&program, &resolved, temp.path()),
            vec![(barrier_cursor.clone(), "hold here".to_owned())],
        );

        // Released -> no longer reported.
        crate::utils::write_user_file(
            temp.path()
                .join(barrier_cursor.to_path())
                .join("barrier_released"),
            "",
        )?;
        assert!(find_waiting_barriers(&program, &resolved, temp.path()).is_empty());
        Ok(())
    }

    // ── evaluate_*_if_block ───────────────────────────────────────────────────

    fn standalone_config(dir: &Path, standalone: bool) -> Config {
        Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: standalone,
            }],
            crates: vec![Crate {
                manifest_dir: dir.to_path_buf(),
                workspace_manifest_dir: dir.to_path_buf(),
                crate_types: BTreeSet::from([CrateType::Bin]),
                target_kinds: BTreeSet::<TargetKind>::new(),
            }],
        }
    }

    fn read_chosen(state_base: &Path, cursor: &ProgramCursor) -> String {
        let path = state_base.join(cursor.to_path()).join("chosen_branch");
        fs_err::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn evaluate_workspace_if_writes_branch_index_when_condition_true() -> TestResult {
        let temp = tempdir()?;
        let dir = temp.path();
        let env = make_environment(&temp);
        let config = standalone_config(dir, true);
        let cursor = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(0));
        let block: WorkspaceIfBlock = IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: WorkspaceCondition::Standalone,
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![],
        };
        evaluate_workspace_if_block(&block, &cursor, dir, dir, &env, &config, &[])?;
        assert_eq!(read_chosen(dir, &cursor), "0");
        Ok(())
    }

    #[test]
    fn evaluate_workspace_if_writes_none_and_else_when_condition_false() -> TestResult {
        let temp = tempdir()?;
        let dir = temp.path();
        let env = make_environment(&temp);
        let config = standalone_config(dir, false); // Standalone -> false

        // No else clause -> "none".
        let cursor_none = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(0));
        let block_none: WorkspaceIfBlock = IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: WorkspaceCondition::Standalone,
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![],
        };
        evaluate_workspace_if_block(&block_none, &cursor_none, dir, dir, &env, &config, &[])?;
        assert_eq!(read_chosen(dir, &cursor_none), "none");

        // With an else clause -> "else".
        let cursor_else = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(1));
        let block_else: WorkspaceIfBlock = IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: WorkspaceCondition::Standalone,
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![WorkspaceStatement::Run(run_step("x", &[]))],
        };
        evaluate_workspace_if_block(&block_else, &cursor_else, dir, dir, &env, &config, &[])?;
        assert_eq!(read_chosen(dir, &cursor_else), "else");
        Ok(())
    }

    #[test]
    fn evaluate_crate_if_writes_branch_index_when_condition_true() -> TestResult {
        let temp = tempdir()?;
        let dir = temp.path();
        let env = make_environment(&temp);
        let config = standalone_config(dir, true);
        let cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let block: CrateIfBlock = IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![],
        };
        evaluate_crate_if_block(&block, &cursor, dir, dir, &env, &config, &[])?;
        assert_eq!(read_chosen(dir, &cursor), "0");
        Ok(())
    }

    // ── describe builders ─────────────────────────────────────────────────────

    fn line(indent: &str, cursor: &ProgramCursor, icon: &'static str, label: &str) -> DescribeLine {
        DescribeLine {
            indent: indent.to_owned(),
            cursor: cursor.to_path_string(),
            icon,
            label: label.to_owned(),
        }
    }

    #[test]
    fn build_crate_describe_renders_run_and_if_branch() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![
            CrateStatement::Run(run_step("a", &[])),
            CrateStatement::If(IfBlock {
                branches: NonEmptyBranches::from_first_and_rest(
                    Branch {
                        condition: CrateCondition::Standalone,
                        statements: vec![CrateStatement::Run(run_step("inner", &[]))],
                    },
                    vec![],
                ),
                else_statements: vec![CrateStatement::Run(run_step("e", &[]))],
            }),
        ];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));

        // Mark the first run completed and choose branch 0 of the if.
        let run0 = prefix.clone().with(CursorSegment::Statement(0));
        let d0 = crate_stmt_state_dir(temp.path(), &run0)?;
        crate::utils::write_user_file(d0.join("exit_status"), "0")?;
        let if_cursor = prefix.clone().with(CursorSegment::Statement(1));
        let if_dir = crate_stmt_state_dir(temp.path(), &if_cursor)?;
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "0")?;

        let mut out = Vec::new();
        build_crate_stmts_describe(&stmts, &prefix, temp.path(), "", &mut out)?;

        let inner = if_cursor
            .clone()
            .with(CursorSegment::IfBranch(0))
            .with(CursorSegment::Statement(0));
        assert_eq!(
            out,
            vec![
                line("", &run0, super::ICON_DONE, r#"run "a""#),
                line("", &if_cursor, super::ICON_DONE, "if [branch taken]"),
                line("  ", &inner, super::ICON_PENDING, r#"run "inner""#),
            ],
        );
        Ok(())
    }

    #[test]
    fn build_crate_describe_marks_unevaluated_if_pending() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![CrateStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: vec![CrateStatement::Run(run_step("inner", &[]))],
                },
                vec![],
            ),
            else_statements: vec![],
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));

        let mut out = Vec::new();
        build_crate_stmts_describe(&stmts, &prefix, temp.path(), "", &mut out)?;

        // No chosen_branch on disk -> pending, no children descended.
        assert_eq!(
            out,
            vec![line(
                "",
                &if_cursor,
                super::ICON_PENDING,
                "if [not yet evaluated]"
            )],
        );
        Ok(())
    }

    // ── find_next_in_* helper paths (if descent / with_env_file) ──────────────

    #[test]
    fn find_next_crate_surfaces_evaluate_if_when_unevaluated() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![CrateStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: vec![CrateStatement::Run(run_step("inner", &[]))],
                },
                vec![],
            ),
            else_statements: vec![],
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let manifest = PathBuf::from("/tmp");
        let NextOutcome::Next(next) =
            find_next_in_crate_stmts(&stmts, &prefix, &manifest, temp.path(), &[])?
        else {
            return Err("expected Next".into());
        };
        assert!(matches!(next.action, StatementAction::EvaluateCrateIf(_)));
        assert_eq!(
            next.cursor,
            prefix.clone().with(CursorSegment::Statement(0))
        );
        Ok(())
    }

    #[test]
    fn find_next_crate_descends_into_chosen_branch() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![CrateStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: vec![CrateStatement::Run(run_step("inner", &[]))],
                },
                vec![],
            ),
            else_statements: vec![],
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let if_dir = crate_stmt_state_dir(temp.path(), &if_cursor)?;
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "0")?;

        let manifest = PathBuf::from("/tmp");
        let NextOutcome::Next(next) =
            find_next_in_crate_stmts(&stmts, &prefix, &manifest, temp.path(), &[])?
        else {
            return Err("expected Next".into());
        };
        assert!(matches!(next.action, StatementAction::RunCommand(_)));
        assert_eq!(
            next.cursor,
            if_cursor
                .with(CursorSegment::IfBranch(0))
                .with(CursorSegment::Statement(0)),
        );
        Ok(())
    }

    #[test]
    fn find_next_crate_with_env_file_collects_env_paths() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![CrateStatement::WithEnvFile(WithEnvFileBlock {
            env_file: "inner.env".to_owned(),
            statements: vec![CrateStatement::Run(run_step("inner", &[]))],
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let manifest = PathBuf::from("/tmp");
        let NextOutcome::Next(next) = find_next_in_crate_stmts(
            &stmts,
            &prefix,
            &manifest,
            temp.path(),
            &["outer.env".to_owned()],
        )?
        else {
            return Err("expected Next".into());
        };
        // Outer env files are preserved and the block's env file appended.
        assert_eq!(
            next.env_file_paths,
            vec!["outer.env".to_owned(), "inner.env".to_owned()]
        );
        assert!(matches!(next.action, StatementAction::RunCommand(_)));
        Ok(())
    }

    #[test]
    fn find_next_workspace_descends_into_else_branch() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![WorkspaceStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: WorkspaceCondition::Standalone,
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![WorkspaceStatement::Run(run_step("elserun", &[]))],
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let if_dir = crate_stmt_state_dir(temp.path(), &if_cursor)?;
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "else")?;

        let manifest = PathBuf::from("/tmp");
        let NextOutcome::Next(next) =
            find_next_in_workspace_stmts(&stmts, &prefix, &manifest, &[], temp.path(), &[])?
        else {
            return Err("expected Next".into());
        };
        assert_eq!(
            next.cursor,
            if_cursor
                .with(CursorSegment::ElseBranch)
                .with(CursorSegment::Statement(0)),
        );
        Ok(())
    }

    // ── is_workspace_stmt_completed: every arm ────────────────────────────────

    #[test]
    fn workspace_stmt_completed_covers_all_arms() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let cur = |i: usize| ProgramCursor::new().with(CursorSegment::Statement(i));
        let complete_run = |c: &ProgramCursor| -> TestResult {
            let d = crate_stmt_state_dir(base, c)?;
            crate::utils::write_user_file(d.join("exit_status"), "0")?;
            Ok(())
        };

        // Run.
        let run = WorkspaceStatement::Run(run_step("r", &[]));
        complete_run(&cur(0))?;
        assert!(is_workspace_stmt_completed(&run, &cur(0), &[], base)?);

        // ManualStep.
        let manual = WorkspaceStatement::ManualStep(ManualStepNode {
            title: "t".to_owned(),
            instructions: "i".to_owned(),
        });
        let d = crate_stmt_state_dir(base, &cur(1))?;
        crate::utils::write_user_file(d.join("manual_step_confirmed"), "y")?;
        assert!(is_workspace_stmt_completed(&manual, &cur(1), &[], base)?);

        // SnapshotMetadata.
        let snap = WorkspaceStatement::SnapshotMetadata(SnapshotMetadataNode {
            name: "s".to_owned(),
        });
        let d = crate_stmt_state_dir(base, &cur(2))?;
        crate::utils::write_user_file(d.join("snapshot_metadata_completed"), "x")?;
        assert!(is_workspace_stmt_completed(&snap, &cur(2), &[], base)?);

        // WaitForContinue.
        let wait = WorkspaceStatement::WaitForContinue(WaitForContinueNode {
            description: "d".to_owned(),
        });
        let d = crate_stmt_state_dir(base, &cur(3))?;
        crate::utils::write_user_file(d.join("barrier_released"), "")?;
        assert!(is_workspace_stmt_completed(&wait, &cur(3), &[], base)?);

        // If with chosen "none" -> completed.
        let if_none =
            workspace_if_standalone(vec![WorkspaceStatement::Run(run_step("x", &[]))], vec![]);
        let d = crate_stmt_state_dir(base, &cur(4))?;
        crate::utils::write_user_file(d.join("chosen_branch"), "none")?;
        assert!(is_workspace_stmt_completed(&if_none, &cur(4), &[], base)?);

        // If with chosen branch 0 -> depends on the branch body.
        let if_branch =
            workspace_if_standalone(vec![WorkspaceStatement::Run(run_step("y", &[]))], vec![]);
        let if_dir = crate_stmt_state_dir(base, &cur(5))?;
        crate::utils::write_user_file(if_dir.join("chosen_branch"), "0")?;
        assert!(!is_workspace_stmt_completed(
            &if_branch,
            &cur(5),
            &[],
            base
        )?);
        complete_run(
            &cur(5)
                .with(CursorSegment::IfBranch(0))
                .with(CursorSegment::Statement(0)),
        )?;
        assert!(is_workspace_stmt_completed(&if_branch, &cur(5), &[], base)?);

        // WithEnvFile -> depends on the nested body.
        let env = WorkspaceStatement::WithEnvFile(WithEnvFileBlock {
            env_file: ".env".to_owned(),
            statements: vec![WorkspaceStatement::Run(run_step("z", &[]))],
        });
        assert!(!is_workspace_stmt_completed(&env, &cur(6), &[], base)?);
        complete_run(
            &cur(6)
                .with(CursorSegment::WithEnvFile)
                .with(CursorSegment::Statement(0)),
        )?;
        assert!(is_workspace_stmt_completed(&env, &cur(6), &[], base)?);
        Ok(())
    }

    // ── find_last_completed_*: branch descent ─────────────────────────────────

    #[test]
    fn find_last_completed_workspace_descends_for_crate_and_env() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));

        // for crate in workspace { run } over two members; complete member 1.
        let stmts = vec![WorkspaceStatement::ForCrateInWorkspace(
            ForCrateInWorkspaceBlock {
                statements: vec![CrateStatement::Run(run_step("c", &[]))],
            },
        )];
        let members = vec![
            ResolvedCrateExecution {
                manifest_dir: PathBuf::from("/a"),
                dependencies: vec![],
            },
            ResolvedCrateExecution {
                manifest_dir: PathBuf::from("/b"),
                dependencies: vec![],
            },
        ];
        let m1_run = prefix
            .clone()
            .with(CursorSegment::Statement(0))
            .with(CursorSegment::CrateIteration(1))
            .with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &m1_run)?;
        crate::utils::write_user_file(d.join("exit_status"), "0")?;
        assert_eq!(
            find_last_completed_workspace_stmt(&stmts, &prefix, &members, base)?,
            Some(m1_run),
        );
        Ok(())
    }

    #[test]
    fn find_last_completed_crate_descends_into_env_file() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let stmts = vec![CrateStatement::WithEnvFile(WithEnvFileBlock {
            env_file: ".env".to_owned(),
            statements: vec![CrateStatement::Run(run_step("x", &[]))],
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let inner = prefix
            .clone()
            .with(CursorSegment::Statement(0))
            .with(CursorSegment::WithEnvFile)
            .with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &inner)?;
        crate::utils::write_user_file(d.join("exit_status"), "0")?;
        assert_eq!(
            find_last_completed_crate_stmt(&stmts, &prefix, base)?,
            Some(inner),
        );
        Ok(())
    }

    // ── find_waiting_barriers: workspace + nested ─────────────────────────────

    #[test]
    fn waiting_barriers_walks_workspace_if_and_env() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        // A workspace if whose branch 0 holds a barrier; chosen is irrelevant —
        // find_waiting_barriers walks *all* branches structurally.
        let program = workspace_program(vec![workspace_if_standalone(
            vec![WorkspaceStatement::WaitForContinue(WaitForContinueNode {
                description: "ws hold".to_owned(),
            })],
            vec![],
        )]);
        let resolved = resolved_with_one_workspace(PathBuf::from("/tmp"));
        let barrier = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(0))
            .with(CursorSegment::IfBranch(0))
            .with(CursorSegment::Statement(0));
        crate_stmt_state_dir(base, &barrier)?; // waiting
        assert_eq!(
            find_waiting_barriers(&program, &resolved, base),
            vec![(barrier, "ws hold".to_owned())],
        );
        Ok(())
    }

    // ── run_*_to_completion / run_*_if_block (no subprocess) ───────────────────

    fn empty_config() -> Config {
        Config {
            workspaces: vec![],
            crates: vec![],
        }
    }

    #[tokio::test]
    async fn run_crate_skips_done_run_and_suspends_at_barrier() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = temp.path().join("state");
        let manifest = PathBuf::from("/tmp");
        let stmts = vec![
            CrateStatement::Run(run_step("echo", &[])),
            CrateStatement::WaitForContinue(WaitForContinueNode {
                description: "hold".to_owned(),
            }),
        ];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        // Mark the run completed so execute_run_step (a subprocess) is skipped.
        let run_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let run_dir = crate_stmt_state_dir(&state_base, &run_cursor)?;
        crate::utils::write_user_file(run_dir.join("exit_status"), "0")?;

        let outcome = run_crate_stmts_to_completion(
            &stmts,
            &prefix,
            &manifest,
            &state_base,
            &env,
            &empty_config(),
            &[],
            "t",
        )
        .await?;
        assert_eq!(outcome, StepOutcome::Suspended);
        // The barrier transitioned pending -> waiting.
        let barrier = prefix.with(CursorSegment::Statement(1));
        assert!(state_base.join(barrier.to_path()).exists());
        Ok(())
    }

    #[tokio::test]
    async fn run_crate_continues_past_released_barrier() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = temp.path().join("state");
        let manifest = PathBuf::from("/tmp");
        let stmts = vec![CrateStatement::WaitForContinue(WaitForContinueNode {
            description: "hold".to_owned(),
        })];
        let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let barrier = prefix.clone().with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(&state_base, &barrier)?;
        crate::utils::write_user_file(d.join("barrier_released"), "")?;

        let outcome = run_crate_stmts_to_completion(
            &stmts,
            &prefix,
            &manifest,
            &state_base,
            &env,
            &empty_config(),
            &[],
            "t",
        )
        .await?;
        assert_eq!(outcome, StepOutcome::Done);
        Ok(())
    }

    #[tokio::test]
    async fn run_crate_if_block_evaluates_and_runs_empty_branch() -> TestResult {
        let temp = tempdir()?;
        let dir = temp.path();
        let env = make_environment(&temp);
        let config = standalone_config(dir, true);
        let state_base = dir.join("state");
        let block: CrateIfBlock = IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: CrateCondition::Standalone,
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![],
        };
        let cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let outcome =
            run_crate_if_block(&block, &cursor, dir, &state_base, &env, &config, &[], "t").await?;
        assert_eq!(outcome, StepOutcome::Done);
        // The condition was evaluated and branch 0 recorded.
        assert_eq!(read_chosen(&state_base, &cursor), "0");
        Ok(())
    }

    #[tokio::test]
    async fn run_workspace_for_crate_suspends_when_member_hits_barrier() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        let state_base = temp.path().join("state");
        let manifest = PathBuf::from("/tmp");
        let stmts = vec![WorkspaceStatement::ForCrateInWorkspace(
            ForCrateInWorkspaceBlock {
                statements: vec![CrateStatement::WaitForContinue(WaitForContinueNode {
                    description: "member hold".to_owned(),
                })],
            },
        )];
        let members = vec![ResolvedCrateExecution {
            manifest_dir: PathBuf::from("/a"),
            dependencies: vec![],
        }];
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));
        let outcome = run_workspace_stmts_to_completion(
            &stmts,
            &prefix,
            &manifest,
            &members,
            &state_base,
            &env,
            &empty_config(),
            &[],
            "t",
        )
        .await?;
        assert_eq!(outcome, StepOutcome::Suspended);
        Ok(())
    }

    #[tokio::test]
    async fn run_workspace_if_block_runs_else_branch() -> TestResult {
        let temp = tempdir()?;
        let dir = temp.path();
        let env = make_environment(&temp);
        let config = standalone_config(dir, false); // Standalone -> false -> else
        let state_base = dir.join("state");
        let block: WorkspaceIfBlock = IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: WorkspaceCondition::Standalone,
                    statements: vec![],
                },
                vec![],
            ),
            // Non-empty else so the chosen marker is "else"; the single run is
            // pre-completed below so no subprocess actually launches.
            else_statements: vec![WorkspaceStatement::Run(run_step("e", &[]))],
        };
        let cursor = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(0));
        let else_run = cursor
            .clone()
            .with(CursorSegment::ElseBranch)
            .with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(&state_base, &else_run)?;
        crate::utils::write_user_file(d.join("exit_status"), "0")?;

        let outcome = run_workspace_if_block(
            &block,
            &cursor,
            dir,
            &[],
            &state_base,
            &env,
            &config,
            &[],
            "t",
        )
        .await?;
        assert_eq!(outcome, StepOutcome::Done);
        assert_eq!(read_chosen(&state_base, &cursor), "else");
        Ok(())
    }

    // ── program_has_interactive_steps: more arms ──────────────────────────────

    #[test]
    fn interactive_detection_covers_env_and_else_and_workspace_manual() {
        // Workspace ManualStep directly.
        let ws_manual = workspace_program(vec![WorkspaceStatement::ManualStep(ManualStepNode {
            title: "t".to_owned(),
            instructions: "i".to_owned(),
        })]);
        assert!(program_has_interactive_steps(&ws_manual));

        // Crate manual nested in a with_env_file.
        let env_manual = crate_program(vec![CrateStatement::WithEnvFile(WithEnvFileBlock {
            env_file: ".env".to_owned(),
            statements: vec![CrateStatement::ManualStep(ManualStepNode {
                title: "t".to_owned(),
                instructions: "i".to_owned(),
            })],
        })]);
        assert!(program_has_interactive_steps(&env_manual));

        // Crate manual only in the else branch of an if.
        let else_manual = crate_program(vec![crate_if_standalone(
            vec![],
            vec![CrateStatement::ManualStep(ManualStepNode {
                title: "t".to_owned(),
                instructions: "i".to_owned(),
            })],
        )]);
        assert!(program_has_interactive_steps(&else_manual));

        // Workspace if whose condition uses ask_user.
        let ws_ask = workspace_program(vec![WorkspaceStatement::If(IfBlock {
            branches: NonEmptyBranches::from_first_and_rest(
                Branch {
                    condition: WorkspaceCondition::Common(CommonCondition::AskUser("?".to_owned())),
                    statements: vec![],
                },
                vec![],
            ),
            else_statements: vec![],
        })]);
        assert!(program_has_interactive_steps(&ws_ask));
    }

    // ── find_next_in_workspace_stmts: more leaf arms ──────────────────────────

    #[test]
    fn find_next_workspace_surfaces_manual_snapshot_and_barrier() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let manifest = PathBuf::from("/tmp");
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));

        // ManualStep not done -> Next(ManualStep).
        let manual = vec![WorkspaceStatement::ManualStep(ManualStepNode {
            title: "t".to_owned(),
            instructions: "i".to_owned(),
        })];
        let NextOutcome::Next(n) =
            find_next_in_workspace_stmts(&manual, &prefix, &manifest, &[], base, &[])?
        else {
            return Err("expected Next".into());
        };
        assert!(matches!(n.action, StatementAction::ManualStep(_)));

        // SnapshotMetadata not done -> Next(SnapshotMetadata).
        let snap = vec![WorkspaceStatement::SnapshotMetadata(SnapshotMetadataNode {
            name: "s".to_owned(),
        })];
        let NextOutcome::Next(n) =
            find_next_in_workspace_stmts(&snap, &prefix, &manifest, &[], base, &[])?
        else {
            return Err("expected Next".into());
        };
        assert!(matches!(n.action, StatementAction::SnapshotMetadata(_)));

        // WaitForContinue pending -> Next(WaitForContinue).
        let wait = vec![WorkspaceStatement::WaitForContinue(WaitForContinueNode {
            description: "d".to_owned(),
        })];
        let NextOutcome::Next(n) =
            find_next_in_workspace_stmts(&wait, &prefix, &manifest, &[], base, &[])?
        else {
            return Err("expected Next for pending barrier".into());
        };
        assert!(matches!(n.action, StatementAction::WaitForContinue(_)));

        // WaitForContinue waiting (state dir exists, no release) -> Suspended.
        let barrier_cursor = prefix.clone().with(CursorSegment::Statement(0));
        crate_stmt_state_dir(base, &barrier_cursor)?;
        assert!(matches!(
            find_next_in_workspace_stmts(&wait, &prefix, &manifest, &[], base, &[])?,
            NextOutcome::Suspended,
        ));
        Ok(())
    }

    #[test]
    fn find_next_workspace_handles_for_crate_and_all_done() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let manifest = PathBuf::from("/tmp");
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));

        // for crate in workspace { run } with one member, nothing done -> the
        // member's run surfaces as the next statement.
        let stmts = vec![WorkspaceStatement::ForCrateInWorkspace(
            ForCrateInWorkspaceBlock {
                statements: vec![CrateStatement::Run(run_step("c", &[]))],
            },
        )];
        let members = vec![ResolvedCrateExecution {
            manifest_dir: PathBuf::from("/a"),
            dependencies: vec![],
        }];
        let NextOutcome::Next(n) =
            find_next_in_workspace_stmts(&stmts, &prefix, &manifest, &members, base, &[])?
        else {
            return Err("expected Next from for-crate body".into());
        };
        assert_eq!(
            n.cursor,
            prefix
                .clone()
                .with(CursorSegment::Statement(0))
                .with(CursorSegment::CrateIteration(0))
                .with(CursorSegment::Statement(0)),
        );

        // Once that run is complete, the whole scope reports Done.
        let d = crate_stmt_state_dir(base, &n.cursor)?;
        crate::utils::write_user_file(d.join("exit_status"), "0")?;
        assert!(matches!(
            find_next_in_workspace_stmts(&stmts, &prefix, &manifest, &members, base, &[])?,
            NextOutcome::Done,
        ));
        Ok(())
    }

    #[test]
    fn find_next_workspace_with_env_file_and_if_none() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let manifest = PathBuf::from("/tmp");
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));

        // with_env_file { run } — the run surfaces with the env file appended.
        let env_stmts = vec![WorkspaceStatement::WithEnvFile(WithEnvFileBlock {
            env_file: "w.env".to_owned(),
            statements: vec![WorkspaceStatement::Run(run_step("x", &[]))],
        })];
        let NextOutcome::Next(n) =
            find_next_in_workspace_stmts(&env_stmts, &prefix, &manifest, &[], base, &[])?
        else {
            return Err("expected Next from with_env_file body".into());
        };
        assert_eq!(n.env_file_paths, vec!["w.env".to_owned()]);

        // An if resolved to "none" is complete, so the walk advances past it to
        // a following run.
        let if_none_then_run = vec![
            workspace_if_standalone(vec![], vec![]),
            WorkspaceStatement::Run(run_step("after", &[])),
        ];
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &if_cursor)?;
        crate::utils::write_user_file(d.join("chosen_branch"), "none")?;
        let NextOutcome::Next(n) =
            find_next_in_workspace_stmts(&if_none_then_run, &prefix, &manifest, &[], base, &[])?
        else {
            return Err("expected Next after if-none".into());
        };
        assert_eq!(n.cursor, prefix.with(CursorSegment::Statement(1)));
        Ok(())
    }

    // ── find_last_completed_workspace_stmt: if + env descent ──────────────────

    #[test]
    fn find_last_completed_workspace_descends_if_and_env() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));

        // if standalone { run } — chosen branch 0, inner run completed.
        let if_stmts = vec![workspace_if_standalone(
            vec![WorkspaceStatement::Run(run_step("inner", &[]))],
            vec![],
        )];
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &if_cursor)?;
        crate::utils::write_user_file(d.join("chosen_branch"), "0")?;
        let inner = if_cursor
            .clone()
            .with(CursorSegment::IfBranch(0))
            .with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &inner)?;
        crate::utils::write_user_file(d.join("exit_status"), "0")?;
        assert_eq!(
            find_last_completed_workspace_stmt(&if_stmts, &prefix, &[], base)?,
            Some(inner),
        );

        // with_env_file { run } — inner run completed.
        let env_stmts = vec![WorkspaceStatement::WithEnvFile(WithEnvFileBlock {
            env_file: ".env".to_owned(),
            statements: vec![WorkspaceStatement::Run(run_step("x", &[]))],
        })];
        let env_inner = prefix
            .clone()
            .with(CursorSegment::Statement(0))
            .with(CursorSegment::WithEnvFile)
            .with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &env_inner)?;
        crate::utils::write_user_file(d.join("exit_status"), "0")?;
        assert_eq!(
            find_last_completed_workspace_stmt(&env_stmts, &prefix, &[], base)?,
            Some(env_inner),
        );
        Ok(())
    }

    #[test]
    fn find_last_completed_workspace_reverse_and_none() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));
        let stmts = vec![
            WorkspaceStatement::Run(run_step("a", &[])),
            WorkspaceStatement::Run(run_step("b", &[])),
        ];

        // Nothing completed -> None.
        assert_eq!(
            find_last_completed_workspace_stmt(&stmts, &prefix, &[], base)?,
            None,
        );

        // Only statement 0 completed: the reverse scan skips the incomplete
        // statement 1 and returns statement 0.
        let s0 = prefix.clone().with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &s0)?;
        crate::utils::write_user_file(d.join("exit_status"), "0")?;
        assert_eq!(
            find_last_completed_workspace_stmt(&stmts, &prefix, &[], base)?,
            Some(s0),
        );
        Ok(())
    }

    #[test]
    fn find_last_completed_workspace_treats_if_none_as_rewindable() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));
        // An if whose conditions all evaluated false with no else ("none") has
        // no completed body, but counts as a rewindable completed step itself.
        let stmts = vec![workspace_if_standalone(
            vec![WorkspaceStatement::Run(run_step("a", &[]))],
            vec![],
        )];
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &if_cursor)?;
        crate::utils::write_user_file(d.join("chosen_branch"), "none")?;
        assert_eq!(
            find_last_completed_workspace_stmt(&stmts, &prefix, &[], base)?,
            Some(if_cursor),
        );
        Ok(())
    }

    // ── cursor_targets_wait_for_continue: crate + nested ──────────────────────

    #[test]
    fn cursor_classifier_walks_crate_and_nested_blocks() {
        // Crate program: run then barrier.
        let crate_prog = crate_program(vec![
            CrateStatement::Run(run_step("a", &[])),
            CrateStatement::WaitForContinue(WaitForContinueNode {
                description: "b".to_owned(),
            }),
        ]);
        let at = |c: ProgramCursor| cursor_targets_wait_for_continue(&crate_prog, &c);
        assert_eq!(
            at(ProgramCursor::new()
                .with(CursorSegment::CrateIteration(0))
                .with(CursorSegment::Statement(1))),
            CursorTarget::WaitForContinue,
        );
        assert_eq!(
            at(ProgramCursor::new()
                .with(CursorSegment::CrateIteration(0))
                .with(CursorSegment::Statement(0))),
            CursorTarget::OtherStatement,
        );

        // Crate if-branch holding a barrier.
        let crate_if = crate_program(vec![crate_if_standalone(
            vec![CrateStatement::WaitForContinue(WaitForContinueNode {
                description: "x".to_owned(),
            })],
            vec![],
        )]);
        assert_eq!(
            cursor_targets_wait_for_continue(
                &crate_if,
                &ProgramCursor::new()
                    .with(CursorSegment::CrateIteration(0))
                    .with(CursorSegment::Statement(0))
                    .with(CursorSegment::IfBranch(0))
                    .with(CursorSegment::Statement(0)),
            ),
            CursorTarget::WaitForContinue,
        );

        // Workspace for-crate body holding a barrier.
        let ws_for_crate = workspace_program(vec![WorkspaceStatement::ForCrateInWorkspace(
            ForCrateInWorkspaceBlock {
                statements: vec![CrateStatement::WaitForContinue(WaitForContinueNode {
                    description: "y".to_owned(),
                })],
            },
        )]);
        assert_eq!(
            cursor_targets_wait_for_continue(
                &ws_for_crate,
                &ProgramCursor::new()
                    .with(CursorSegment::WorkspaceIteration(0))
                    .with(CursorSegment::Statement(0))
                    .with(CursorSegment::CrateIteration(0))
                    .with(CursorSegment::Statement(0)),
            ),
            CursorTarget::WaitForContinue,
        );

        // Workspace with_env_file body holding a barrier.
        let ws_env = workspace_program(vec![WorkspaceStatement::WithEnvFile(WithEnvFileBlock {
            env_file: ".env".to_owned(),
            statements: vec![WorkspaceStatement::WaitForContinue(WaitForContinueNode {
                description: "z".to_owned(),
            })],
        })]);
        assert_eq!(
            cursor_targets_wait_for_continue(
                &ws_env,
                &ProgramCursor::new()
                    .with(CursorSegment::WorkspaceIteration(0))
                    .with(CursorSegment::Statement(0))
                    .with(CursorSegment::WithEnvFile)
                    .with(CursorSegment::Statement(0)),
            ),
            CursorTarget::WaitForContinue,
        );
    }

    // ── task_list_command ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn task_list_command_handles_missing_and_present_dirs() -> TestResult {
        let temp = tempdir()?;
        let env = make_environment(&temp);
        // No tasks dir yet -> prints "No tasks found", returns Ok.
        task_list_command(env.clone()).await?;

        // Create the tasks dir with two task subdirs and a stray file.
        let tasks = dir_path(&env)?;
        crate::utils::create_user_dir_all(tasks.join("alpha"))?;
        crate::utils::create_user_dir_all(tasks.join("beta"))?;
        crate::utils::write_user_file(tasks.join("not-a-task"), "x")?;
        task_list_command(env).await?;
        Ok(())
    }

    #[test]
    fn build_workspace_describe_renders_if_branch() -> TestResult {
        let temp = tempdir()?;
        let base = temp.path();
        let stmts = vec![workspace_if_standalone(
            vec![WorkspaceStatement::Run(run_step("inner", &[]))],
            vec![],
        )];
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));
        let if_cursor = prefix.clone().with(CursorSegment::Statement(0));
        let d = crate_stmt_state_dir(base, &if_cursor)?;
        crate::utils::write_user_file(d.join("chosen_branch"), "0")?;

        let mut out = Vec::new();
        build_workspace_stmts_describe(&stmts, &prefix, &[], base, "", &mut out)?;

        let inner = if_cursor
            .clone()
            .with(CursorSegment::IfBranch(0))
            .with(CursorSegment::Statement(0));
        assert_eq!(
            out,
            vec![
                line("", &if_cursor, super::ICON_DONE, "if [branch taken]"),
                line("  ", &inner, super::ICON_PENDING, r#"run "inner""#),
            ],
        );
        Ok(())
    }

    #[test]
    fn build_workspace_describe_renders_for_crate_members() -> TestResult {
        let temp = tempdir()?;
        let stmts = vec![
            WorkspaceStatement::Run(run_step("w", &[])),
            WorkspaceStatement::ForCrateInWorkspace(ForCrateInWorkspaceBlock {
                statements: vec![CrateStatement::Run(run_step("c", &[]))],
            }),
        ];
        let members = vec![
            ResolvedCrateExecution {
                manifest_dir: PathBuf::from("/a"),
                dependencies: vec![],
            },
            ResolvedCrateExecution {
                manifest_dir: PathBuf::from("/b"),
                dependencies: vec![],
            },
        ];
        let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));

        let mut out = Vec::new();
        build_workspace_stmts_describe(&stmts, &prefix, &members, temp.path(), "", &mut out)?;

        let ws_run = prefix.clone().with(CursorSegment::Statement(0));
        let for_crate = prefix.clone().with(CursorSegment::Statement(1));
        let c0 = for_crate.clone().with(CursorSegment::CrateIteration(0));
        let c0_run = c0.clone().with(CursorSegment::Statement(0));
        let c1 = for_crate.clone().with(CursorSegment::CrateIteration(1));
        let c1_run = c1.clone().with(CursorSegment::Statement(0));
        assert_eq!(
            out,
            vec![
                line("", &ws_run, super::ICON_PENDING, r#"run "w""#),
                line(
                    "",
                    &for_crate,
                    super::ICON_PENDING,
                    "for crate in workspace"
                ),
                line("  ", &c0, super::ICON_PENDING, "crate /a"),
                line("    ", &c0_run, super::ICON_PENDING, r#"run "c""#),
                line("  ", &c1, super::ICON_PENDING, "crate /b"),
                line("    ", &c1_run, super::ICON_PENDING, r#"run "c""#),
            ],
        );
        Ok(())
    }

    // ── command handlers (via task fixtures) ──────────────────────────────────

    /// Writes a task definition (`program.cfe` + `resolved-program.toml`) so the
    /// `load_task_data`-based command handlers can run against it.
    fn write_task_fixture(
        env: &Environment,
        name: &str,
        cfe_src: &str,
        resolved: &ResolvedProgram,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let task_dir = named_dir_path(name, env)?;
        crate::utils::create_user_dir_all(&task_dir)?;
        crate::utils::write_user_file(task_dir.join("program.cfe"), cfe_src)?;
        crate::utils::write_user_file(
            task_dir.join("resolved-program.toml"),
            toml::to_string(resolved)?,
        )?;
        Ok(())
    }

    /// A `ResolvedProgram` with a single standalone crate at `manifest_dir`.
    fn resolved_one_crate_at(manifest_dir: &Path) -> ResolvedProgram {
        ResolvedProgram {
            workspace_executions: vec![],
            crate_executions: vec![ResolvedCrateExecution {
                manifest_dir: manifest_dir.to_path_buf(),
                dependencies: vec![],
            }],
        }
    }

    #[tokio::test]
    async fn run_single_step_creates_waiting_barrier() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        write_task_fixture(
            &env,
            "t",
            r#"for crate { wait_for_continue "hold"; }"#,
            &resolved_one_crate_at(&PathBuf::from("/tmp")),
        )?;
        run_single_step_command(
            RunSingleStepParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;

        // The barrier's state dir now exists (pending -> waiting transition).
        let state_base = state_dir_for_task("t", &env)?;
        let barrier = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        assert!(state_base.join(barrier.to_path()).exists());
        Ok(())
    }

    #[tokio::test]
    async fn run_single_step_evaluates_crate_if() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        write_task_fixture(
            &env,
            "t",
            r#"for crate { if type == lib { run "true"; } }"#,
            &resolved_one_crate_at(&PathBuf::from("/tmp")),
        )?;
        run_single_step_command(
            RunSingleStepParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;

        // With an empty config the crate is not a lib, so no branch matches.
        let state_base = state_dir_for_task("t", &env)?;
        let if_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let chosen =
            fs_err::read_to_string(state_base.join(if_cursor.to_path()).join("chosen_branch"))?;
        assert_eq!(chosen.trim(), "none");
        Ok(())
    }

    #[tokio::test]
    async fn run_single_step_runs_command_to_completion() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        write_task_fixture(
            &env,
            "t",
            r#"for crate { run "true"; }"#,
            &resolved_one_crate_at(temp.path()),
        )?;
        run_single_step_command(
            RunSingleStepParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;

        // `true` exits 0, so the run statement is recorded as completed.
        let state_base = state_dir_for_task("t", &env)?;
        let run_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        assert!(is_run_completed(&state_base.join(run_cursor.to_path()))?);
        Ok(())
    }

    #[tokio::test]
    async fn run_single_step_reports_done_and_suspended() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        // Program: a run followed by a barrier.
        write_task_fixture(
            &env,
            "t",
            r#"for crate { run "true"; wait_for_continue "hold"; }"#,
            &resolved_one_crate_at(temp.path()),
        )?;
        let state_base = state_dir_for_task("t", &env)?;

        // Mark the run completed and put the barrier into the waiting state, so
        // find_next reports Suspended (exercises report_suspension).
        let run_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let run_dir = state_base.join(run_cursor.to_path());
        crate::utils::create_user_dir_all(&run_dir)?;
        crate::utils::write_user_file(run_dir.join("exit_status"), "0")?;
        let barrier_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(1));
        crate::utils::create_user_dir_all(state_base.join(barrier_cursor.to_path()))?;

        // Suspended path: returns Ok after printing the barrier list.
        run_single_step_command(
            RunSingleStepParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;

        // Done path: release the barrier, then everything is complete.
        crate::utils::write_user_file(
            state_base
                .join(barrier_cursor.to_path())
                .join("barrier_released"),
            "",
        )?;
        run_single_step_command(
            RunSingleStepParameters {
                name: "t".to_owned(),
            },
            env,
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn run_single_target_runs_crate_through_all_arms() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        fs_err::write(temp.path().join("vars.env"), "K=V\n")?;
        // Exercises run_crate_stmts_to_completion's Run, WithEnvFile, If and
        // WaitForContinue arms in one pass; the trailing barrier suspends.
        write_task_fixture(
            &env,
            "t",
            r#"for crate { run "true"; with_env_file "vars.env" { run "true"; } if type == lib { run "true"; } wait_for_continue "x"; }"#,
            &resolved_one_crate_at(temp.path()),
        )?;
        run_single_target_command(
            RunSingleTargetParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;

        let state_base = state_dir_for_task("t", &env)?;
        let c0 = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        // First run completed.
        assert!(is_run_completed(
            &state_base.join(c0.clone().with(CursorSegment::Statement(0)).to_path())
        )?);
        // Nested with_env_file run completed.
        let env_run = c0
            .clone()
            .with(CursorSegment::Statement(1))
            .with(CursorSegment::WithEnvFile)
            .with(CursorSegment::Statement(0));
        assert!(is_run_completed(&state_base.join(env_run.to_path()))?);
        // Execution suspended at the trailing barrier.
        let barrier = c0.with(CursorSegment::Statement(3));
        assert!(state_base.join(barrier.to_path()).exists());
        Ok(())
    }

    #[tokio::test]
    async fn run_single_target_runs_workspace_through_all_arms() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        fs_err::write(temp.path().join("vars.env"), "K=V\n")?;
        let resolved = ResolvedProgram {
            workspace_executions: vec![ResolvedWorkspaceExecution {
                manifest_dir: temp.path().to_path_buf(),
                dependencies: vec![],
                member_crates: vec![ResolvedCrateExecution {
                    manifest_dir: temp.path().to_path_buf(),
                    dependencies: vec![],
                }],
            }],
            crate_executions: vec![],
        };
        // Exercises Run, WithEnvFile, If, ForCrateInWorkspace and WaitForContinue.
        write_task_fixture(
            &env,
            "t",
            r#"for workspace { run "true"; with_env_file "vars.env" { run "true"; } if standalone { run "true"; } for crate in workspace { run "true"; } wait_for_continue "x"; }"#,
            &resolved,
        )?;
        run_single_target_command(
            RunSingleTargetParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;

        let state_base = state_dir_for_task("t", &env)?;
        let w0 = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(0));
        assert!(is_run_completed(
            &state_base.join(w0.clone().with(CursorSegment::Statement(0)).to_path())
        )?);
        // The for-crate member's run completed.
        let member_run = w0
            .clone()
            .with(CursorSegment::Statement(3))
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        assert!(is_run_completed(&state_base.join(member_run.to_path()))?);
        // Suspended at the trailing barrier.
        let barrier = w0.with(CursorSegment::Statement(4));
        assert!(state_base.join(barrier.to_path()).exists());
        Ok(())
    }

    #[tokio::test]
    async fn release_wait_barrier_writes_marker_and_validates_cursor() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        write_task_fixture(
            &env,
            "t",
            r#"for crate { run "true"; wait_for_continue "hold"; }"#,
            &resolved_one_crate_at(temp.path()),
        )?;
        let barrier = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(1));

        release_wait_barrier_command(
            ContinueBarrierParameters {
                name: "t".to_owned(),
                cursor: barrier.to_path_string(),
            },
            env.clone(),
        )
        .await?;
        let state_base = state_dir_for_task("t", &env)?;
        assert!(
            state_base
                .join(barrier.to_path())
                .join("barrier_released")
                .exists()
        );

        // Pointing at a non-barrier statement is rejected.
        let run = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        match release_wait_barrier_command(
            ContinueBarrierParameters {
                name: "t".to_owned(),
                cursor: run.to_path_string(),
            },
            env,
        )
        .await
        {
            Err(Error::CursorNotAtBarrier(_)) => {}
            other => return Err(format!("expected CursorNotAtBarrier, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_describe_runs_for_a_fixture_task() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        write_task_fixture(
            &env,
            "t",
            r#"for crate { run "true"; wait_for_continue "hold"; }"#,
            &resolved_one_crate_at(temp.path()),
        )?;
        // Smoke test: describe walks the program + state and prints without error.
        task_describe_command(
            DescribeTaskParameters {
                name: "t".to_owned(),
            },
            env,
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn rewind_single_step_removes_last_completed_state() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        write_task_fixture(
            &env,
            "t",
            r#"for crate { run "true"; }"#,
            &resolved_one_crate_at(temp.path()),
        )?;
        let state_base = state_dir_for_task("t", &env)?;
        let run_cursor = ProgramCursor::new()
            .with(CursorSegment::CrateIteration(0))
            .with(CursorSegment::Statement(0));
        let run_dir = state_base.join(run_cursor.to_path());
        crate::utils::create_user_dir_all(&run_dir)?;
        crate::utils::write_user_file(run_dir.join("exit_status"), "0")?;

        rewind_single_step_command(
            RewindSingleStepParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;
        assert!(!run_dir.exists());

        // Nothing left to rewind -> still Ok.
        rewind_single_step_command(
            RewindSingleStepParameters {
                name: "t".to_owned(),
            },
            env,
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn rewind_single_target_removes_last_completed_target() -> TestResult {
        let temp = tempdir()?;
        let env = crate::Environment::mock(&temp)?;
        write_task_fixture(
            &env,
            "t",
            r#"for crate { run "true"; }"#,
            &resolved_one_crate_at(temp.path()),
        )?;
        let state_base = state_dir_for_task("t", &env)?;
        let c0 = ProgramCursor::new().with(CursorSegment::CrateIteration(0));
        let run_dir = state_base.join(c0.clone().with(CursorSegment::Statement(0)).to_path());
        crate::utils::create_user_dir_all(&run_dir)?;
        crate::utils::write_user_file(run_dir.join("exit_status"), "0")?;

        rewind_single_target_command(
            RewindSingleTargetParameters {
                name: "t".to_owned(),
            },
            env.clone(),
        )
        .await?;
        // The whole crate-iteration state dir is removed.
        assert!(!state_base.join(c0.to_path()).exists());

        rewind_single_target_command(
            RewindSingleTargetParameters {
                name: "t".to_owned(),
            },
            env,
        )
        .await?;
        Ok(())
    }
}
