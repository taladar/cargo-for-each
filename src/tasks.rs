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
    WorkspaceCondition, WorkspaceIfBlock, WorkspaceStatement,
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
/// Returns an error if `name` fails [`validate_task_name`] or if the tasks
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
/// Returns an error if `name` fails [`validate_task_name`] or if the state
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

/// Returns `true` if the `run` statement recorded at `state_dir` succeeded.
fn is_run_completed(state_dir: &Path) -> bool {
    if !state_dir.exists() {
        return false;
    }
    fs_err::read_to_string(state_dir.join("exit_status"))
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("0")
}

/// Returns `true` if the `run` step at `state_dir` has any recorded non-success
/// status — i.e. the `exit_status` file exists and its trimmed contents are
/// anything other than `"0"`, including the empty string written on
/// launch-failure paths.
///
/// Distinct from `is_run_completed`: a step that has not been started at all returns `false`.
fn is_run_failed(state_dir: &Path) -> bool {
    if !state_dir.exists() {
        return false;
    }
    match fs_err::read_to_string(state_dir.join("exit_status"))
        .ok()
        .as_deref()
        .map(str::trim)
    {
        None | Some("0") => false,
        Some(_) => true,
    }
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

/// Returns `true` if all crate statements in `stmts` under `prefix` are completed.
fn is_crate_stmts_completed(
    stmts: &[CrateStatement],
    prefix: &ProgramCursor,
    state_base: &Path,
) -> bool {
    stmts.iter().enumerate().all(|(i, stmt)| {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        is_crate_stmt_completed(stmt, &cursor, state_base)
    })
}

/// Returns `true` if the given crate statement at `cursor` is completed.
fn is_crate_stmt_completed(
    stmt: &CrateStatement,
    cursor: &ProgramCursor,
    state_base: &Path,
) -> bool {
    let state_dir = state_base.join(cursor.to_path());
    match stmt {
        CrateStatement::Run(_) => is_run_completed(&state_dir),
        CrateStatement::ManualStep(_) => is_manual_completed(&state_dir),
        CrateStatement::SnapshotMetadata(_) => is_snapshot_metadata_completed(&state_dir),
        CrateStatement::If(block) => {
            let Ok(chosen) = fs_err::read_to_string(state_dir.join("chosen_branch")) else {
                return false;
            };
            match chosen.trim() {
                "none" => true,
                "else" => {
                    let p = cursor.clone().with(CursorSegment::ElseBranch);
                    is_crate_stmts_completed(&block.else_statements, &p, state_base)
                }
                s => s.parse::<usize>().is_ok_and(|n| {
                    block.branches.get(n).is_some_and(|branch| {
                        let p = cursor.clone().with(CursorSegment::IfBranch(n));
                        is_crate_stmts_completed(&branch.statements, &p, state_base)
                    })
                }),
            }
        }
        CrateStatement::WithEnvFile(block) => {
            let p = cursor.clone().with(CursorSegment::WithEnvFile);
            is_crate_stmts_completed(&block.statements, &p, state_base)
        }
        CrateStatement::WaitForContinue(_) => is_wait_barrier_released(&state_dir),
    }
}

/// Returns `true` if all workspace statements in `stmts` under `prefix` are completed.
///
/// `member_crates` is required to evaluate `ForCrateInWorkspace` blocks.
fn is_workspace_stmts_completed(
    stmts: &[WorkspaceStatement],
    prefix: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
) -> bool {
    stmts.iter().enumerate().all(|(i, stmt)| {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        is_workspace_stmt_completed(stmt, &cursor, member_crates, state_base)
    })
}

/// Returns `true` if the given workspace statement at `cursor` is completed.
fn is_workspace_stmt_completed(
    stmt: &WorkspaceStatement,
    cursor: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
) -> bool {
    let state_dir = state_base.join(cursor.to_path());
    match stmt {
        WorkspaceStatement::Run(_) => is_run_completed(&state_dir),
        WorkspaceStatement::ManualStep(_) => is_manual_completed(&state_dir),
        WorkspaceStatement::SnapshotMetadata(_) => is_snapshot_metadata_completed(&state_dir),
        WorkspaceStatement::If(block) => {
            let Ok(chosen) = fs_err::read_to_string(state_dir.join("chosen_branch")) else {
                return false;
            };
            match chosen.trim() {
                "none" => true,
                "else" => {
                    let p = cursor.clone().with(CursorSegment::ElseBranch);
                    is_workspace_stmts_completed(
                        &block.else_statements,
                        &p,
                        member_crates,
                        state_base,
                    )
                }
                s => s.parse::<usize>().is_ok_and(|n| {
                    block.branches.get(n).is_some_and(|branch| {
                        let p = cursor.clone().with(CursorSegment::IfBranch(n));
                        is_workspace_stmts_completed(
                            &branch.statements,
                            &p,
                            member_crates,
                            state_base,
                        )
                    })
                }),
            }
        }
        WorkspaceStatement::WithEnvFile(block) => {
            let p = cursor.clone().with(CursorSegment::WithEnvFile);
            is_workspace_stmts_completed(&block.statements, &p, member_crates, state_base)
        }
        WorkspaceStatement::ForCrateInWorkspace(block) => {
            member_crates.iter().enumerate().all(|(c_idx, _)| {
                let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
                is_crate_stmts_completed(&block.statements, &c_prefix, state_base)
            })
        }
        WorkspaceStatement::WaitForContinue(_) => is_wait_barrier_released(&state_dir),
    }
}

/// Returns `true` if all workspace statements for `ws_idx` are completed.
fn is_workspace_completed(
    ws_idx: usize,
    ws_exec: &ResolvedWorkspaceExecution,
    ws_stmts: &[WorkspaceStatement],
    state_base: &Path,
) -> bool {
    let prefix = ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
    is_workspace_stmts_completed(ws_stmts, &prefix, &ws_exec.member_crates, state_base)
}

/// Returns `true` if all statements for standalone crate `c_idx` are completed.
fn is_standalone_crate_completed(
    c_idx: usize,
    crate_stmts: &[CrateStatement],
    state_base: &Path,
) -> bool {
    let prefix = ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
    is_crate_stmts_completed(crate_stmts, &prefix, state_base)
}

/// Returns `true` if all inter-workspace dependencies of `ws_exec` are completed.
fn are_workspace_deps_completed(
    ws_exec: &ResolvedWorkspaceExecution,
    ws_map: &HashMap<PathBuf, usize>,
    ws_stmts: &[WorkspaceStatement],
    resolved: &ResolvedProgram,
    state_base: &Path,
) -> bool {
    ws_exec.dependencies.iter().all(|dep_path| {
        let Some(&dep_idx) = ws_map.get(dep_path) else {
            return true; // Dep not in selected set — treat as satisfied.
        };
        let Some(dep_exec) = resolved.workspace_executions.get(dep_idx) else {
            return true;
        };
        is_workspace_completed(dep_idx, dep_exec, ws_stmts, state_base)
    })
}

/// Returns `true` if all dependencies of a standalone crate have completed.
fn are_standalone_crate_deps_completed(
    crate_exec: &ResolvedCrateExecution,
    crate_map: &HashMap<PathBuf, usize>,
    crate_stmts: &[CrateStatement],
    state_base: &Path,
) -> bool {
    crate_exec.dependencies.iter().all(|dep_path| {
        let Some(&dep_idx) = crate_map.get(dep_path) else {
            return true;
        };
        is_standalone_crate_completed(dep_idx, crate_stmts, state_base)
    })
}

/// Returns `true` if all intra-workspace dependencies of a member crate are
/// completed for the given `for crate in workspace` block.
fn are_member_crate_deps_completed(
    crate_exec: &ResolvedCrateExecution,
    crate_map: &HashMap<PathBuf, usize>,
    for_crate_prefix: &ProgramCursor,
    for_crate_stmts: &[CrateStatement],
    state_base: &Path,
) -> bool {
    crate_exec.dependencies.iter().all(|dep_path| {
        let Some(&dep_idx) = crate_map.get(dep_path) else {
            return true;
        };
        let c_prefix = for_crate_prefix
            .clone()
            .with(CursorSegment::CrateIteration(dep_idx));
        is_crate_stmts_completed(for_crate_stmts, &c_prefix, state_base)
    })
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

/// Finds the first uncompleted crate statement in `stmts` starting at `prefix`.
///
/// Returns [`NextOutcome::Done`] if every statement is complete,
/// [`NextOutcome::Suspended`] if a `wait_for_continue` barrier in this scope
/// (or in a nested scope) is in the *waiting* state with no executable
/// statement available before it, or [`NextOutcome::Next`] with the next
/// action.
fn find_next_in_crate_stmts<'a>(
    stmts: &'a [CrateStatement],
    prefix: &ProgramCursor,
    manifest_dir: &'a Path,
    state_base: &Path,
    env_file_paths: &[String],
) -> NextOutcome<'a> {
    let mut suspended = false;
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            CrateStatement::Run(step) => {
                if !is_run_completed(&state_dir) {
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::RunCommand(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            CrateStatement::ManualStep(step) => {
                if !is_manual_completed(&state_dir) {
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::ManualStep(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            CrateStatement::SnapshotMetadata(step) => {
                if !is_snapshot_metadata_completed(&state_dir) {
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::SnapshotMetadata(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            CrateStatement::If(block) => {
                match fs_err::read_to_string(state_dir.join("chosen_branch")) {
                    Err(_) => {
                        return NextOutcome::Next(NextStatement {
                            cursor,
                            manifest_dir,
                            action: StatementAction::EvaluateCrateIf(block),
                            env_file_paths: env_file_paths.to_vec(),
                        });
                    }
                    Ok(chosen) => {
                        let nested = match chosen.trim() {
                            "none" => NextOutcome::Done,
                            "else" => {
                                let p = cursor.clone().with(CursorSegment::ElseBranch);
                                find_next_in_crate_stmts(
                                    &block.else_statements,
                                    &p,
                                    manifest_dir,
                                    state_base,
                                    env_file_paths,
                                )
                            }
                            s => s.parse::<usize>().ok().map_or(NextOutcome::Done, |n| {
                                block.branches.get(n).map_or(NextOutcome::Done, |branch| {
                                    let p = cursor.clone().with(CursorSegment::IfBranch(n));
                                    find_next_in_crate_stmts(
                                        &branch.statements,
                                        &p,
                                        manifest_dir,
                                        state_base,
                                        env_file_paths,
                                    )
                                })
                            }),
                        };
                        match nested {
                            NextOutcome::Next(_) => return nested,
                            NextOutcome::Suspended => suspended = true,
                            NextOutcome::Done => {}
                        }
                    }
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
                );
                match nested {
                    NextOutcome::Next(_) => return nested,
                    NextOutcome::Suspended => suspended = true,
                    NextOutcome::Done => {}
                }
            }
            CrateStatement::WaitForContinue(node) => {
                if is_wait_barrier_released(&state_dir) {
                    // Already released — fall through to the next statement.
                } else if is_wait_barrier_waiting(&state_dir) {
                    // Waiting for release — this scope is suspended.
                    return NextOutcome::Suspended;
                } else {
                    // Pending — surface it as the next action.
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::WaitForContinue(node),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
        }
    }
    if suspended {
        NextOutcome::Suspended
    } else {
        NextOutcome::Done
    }
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
) -> NextOutcome<'a> {
    let mut suspended = false;
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            WorkspaceStatement::Run(step) => {
                if !is_run_completed(&state_dir) {
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::RunCommand(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            WorkspaceStatement::ManualStep(step) => {
                if !is_manual_completed(&state_dir) {
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::ManualStep(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            WorkspaceStatement::SnapshotMetadata(step) => {
                if !is_snapshot_metadata_completed(&state_dir) {
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::SnapshotMetadata(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            WorkspaceStatement::If(block) => {
                match fs_err::read_to_string(state_dir.join("chosen_branch")) {
                    Err(_) => {
                        return NextOutcome::Next(NextStatement {
                            cursor,
                            manifest_dir,
                            action: StatementAction::EvaluateWorkspaceIf(block),
                            env_file_paths: env_file_paths.to_vec(),
                        });
                    }
                    Ok(chosen) => {
                        let nested = match chosen.trim() {
                            "none" => NextOutcome::Done,
                            "else" => {
                                let p = cursor.clone().with(CursorSegment::ElseBranch);
                                find_next_in_workspace_stmts(
                                    &block.else_statements,
                                    &p,
                                    manifest_dir,
                                    member_crates,
                                    state_base,
                                    env_file_paths,
                                )
                            }
                            s => s.parse::<usize>().ok().map_or(NextOutcome::Done, |n| {
                                block.branches.get(n).map_or(NextOutcome::Done, |branch| {
                                    let p = cursor.clone().with(CursorSegment::IfBranch(n));
                                    find_next_in_workspace_stmts(
                                        &branch.statements,
                                        &p,
                                        manifest_dir,
                                        member_crates,
                                        state_base,
                                        env_file_paths,
                                    )
                                })
                            }),
                        };
                        match nested {
                            NextOutcome::Next(_) => return nested,
                            NextOutcome::Suspended => suspended = true,
                            NextOutcome::Done => {}
                        }
                    }
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
                );
                match nested {
                    NextOutcome::Next(_) => return nested,
                    NextOutcome::Suspended => suspended = true,
                    NextOutcome::Done => {}
                }
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
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
                        &cursor,
                        &block.statements,
                        state_base,
                    ) {
                        // An intra-workspace dep is incomplete; the dep itself
                        // either is suspended (caught below via the dep's own
                        // iteration) or will surface its own Next action when
                        // we reach it. Don't mark "Done" prematurely.
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
                    );
                    match nested {
                        NextOutcome::Next(_) => return nested,
                        NextOutcome::Suspended => block_suspended = true,
                        NextOutcome::Done => {}
                    }
                }
                if block_suspended {
                    // Don't walk past a `for crate in workspace` block that
                    // still has suspended (or dep-blocked-by-suspended)
                    // members — downstream workspace statements may depend
                    // on the work those members will do after the barrier.
                    return NextOutcome::Suspended;
                }
            }
            WorkspaceStatement::WaitForContinue(node) => {
                if is_wait_barrier_released(&state_dir) {
                    // Already released — fall through to the next statement.
                } else if is_wait_barrier_waiting(&state_dir) {
                    // Waiting for release — this scope is suspended.
                    return NextOutcome::Suspended;
                } else {
                    // Pending — surface it as the next action.
                    return NextOutcome::Next(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::WaitForContinue(node),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
        }
    }
    if suspended {
        NextOutcome::Suspended
    } else {
        NextOutcome::Done
    }
}

/// Finds the next uncompleted statement across all workspaces and standalone crates,
/// respecting inter-target dependency ordering.
///
/// Returns [`NextOutcome::Done`] when every statement in every target has
/// completed, [`NextOutcome::Suspended`] when no statement is currently
/// executable because at least one target is blocked at a `wait_for_continue`
/// barrier (or transitively blocked by one), or [`NextOutcome::Next`] with
/// the next action.
#[must_use]
pub fn find_next_statement<'a>(
    program: &'a Program,
    resolved: &'a ResolvedProgram,
    state_base: &Path,
) -> NextOutcome<'a> {
    let mut suspended = false;
    let ws_stmts = first_workspace_stmts(program);
    let ws_map: HashMap<PathBuf, usize> = resolved
        .workspace_executions
        .iter()
        .enumerate()
        .map(|(i, w)| (w.manifest_dir.clone(), i))
        .collect();

    for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate() {
        if !are_workspace_deps_completed(ws_exec, &ws_map, ws_stmts, resolved, state_base) {
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
        );
        match next {
            NextOutcome::Next(_) => return next,
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
        if !are_standalone_crate_deps_completed(crate_exec, &crate_map, crate_stmts, state_base) {
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
        );
        match next {
            NextOutcome::Next(_) => return next,
            NextOutcome::Suspended => suspended = true,
            NextOutcome::Done => {}
        }
    }

    if suspended {
        NextOutcome::Suspended
    } else {
        NextOutcome::Done
    }
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
            crate::utils::write_user_file(&exit_status_path, "")
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
#[expect(clippy::print_stdout, reason = "barrier message is part of the UI")]
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
                if !is_run_completed(&state_dir) {
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
                let chosen_branch_path = state_dir.join("chosen_branch");
                if !chosen_branch_path.exists() {
                    evaluate_crate_if_block(
                        block,
                        &cursor,
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
                let inner = match trimmed {
                    "none" => StepOutcome::Done,
                    "else" => {
                        let p = cursor.clone().with(CursorSegment::ElseBranch);
                        Box::pin(run_crate_stmts_to_completion(
                            &block.else_statements,
                            &p,
                            manifest_dir,
                            state_base,
                            environment,
                            config,
                            extra_env,
                            task_name,
                        ))
                        .await?
                    }
                    s => {
                        let n: usize = s
                            .parse()
                            .map_err(|_parse_err| Error::InvalidChosenBranch(trimmed.to_owned()))?;
                        let branch = block
                            .branches
                            .get(n)
                            .ok_or_else(|| Error::InvalidChosenBranch(trimmed.to_owned()))?;
                        let p = cursor.clone().with(CursorSegment::IfBranch(n));
                        Box::pin(run_crate_stmts_to_completion(
                            &branch.statements,
                            &p,
                            manifest_dir,
                            state_base,
                            environment,
                            config,
                            extra_env,
                            task_name,
                        ))
                        .await?
                    }
                };
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
                if is_wait_barrier_released(&state_dir) {
                    // Released — continue to next statement.
                } else {
                    // Pending or waiting — create state_dir (pending → waiting) and stop.
                    if !state_dir.exists() {
                        crate::utils::create_user_dir_all(&state_dir)
                            .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;
                    }
                    println!(
                        "Wait barrier reached at {}: \"{}\". Release with `cargo-for-each task continue --name {} --cursor {}`.",
                        cursor.to_path_string(),
                        node.description,
                        task_name,
                        cursor.to_path_string()
                    );
                    return Ok(StepOutcome::Suspended);
                }
            }
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
#[expect(clippy::print_stdout, reason = "barrier message is part of the UI")]
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
                if !is_run_completed(&state_dir) {
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
                let chosen_branch_path = state_dir.join("chosen_branch");
                if !chosen_branch_path.exists() {
                    evaluate_workspace_if_block(
                        block,
                        &cursor,
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
                let inner = match trimmed {
                    "none" => StepOutcome::Done,
                    "else" => {
                        let p = cursor.clone().with(CursorSegment::ElseBranch);
                        Box::pin(run_workspace_stmts_to_completion(
                            &block.else_statements,
                            &p,
                            manifest_dir,
                            member_crates,
                            state_base,
                            environment,
                            config,
                            extra_env,
                            task_name,
                        ))
                        .await?
                    }
                    s => {
                        let n: usize = s
                            .parse()
                            .map_err(|_parse_err| Error::InvalidChosenBranch(trimmed.to_owned()))?;
                        let branch = block
                            .branches
                            .get(n)
                            .ok_or_else(|| Error::InvalidChosenBranch(trimmed.to_owned()))?;
                        let p = cursor.clone().with(CursorSegment::IfBranch(n));
                        Box::pin(run_workspace_stmts_to_completion(
                            &branch.statements,
                            &p,
                            manifest_dir,
                            member_crates,
                            state_base,
                            environment,
                            config,
                            extra_env,
                            task_name,
                        ))
                        .await?
                    }
                };
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
                // Member crates are already in intra-workspace dependency order.
                // If any member suspends at a barrier, halt before moving on
                // to later members or later workspace statements — those may
                // depend on the work the suspended member would do after the
                // barrier is released.
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
            }
            WorkspaceStatement::WaitForContinue(node) => {
                if is_wait_barrier_released(&state_dir) {
                    // Released — continue to next statement.
                } else {
                    // Pending or waiting — create state_dir (pending → waiting) and stop.
                    if !state_dir.exists() {
                        crate::utils::create_user_dir_all(&state_dir)
                            .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;
                    }
                    println!(
                        "Wait barrier reached at {}: \"{}\". Release with `cargo-for-each task continue --name {} --cursor {}`.",
                        cursor.to_path_string(),
                        node.description,
                        task_name,
                        cursor.to_path_string()
                    );
                    return Ok(StepOutcome::Suspended);
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
) -> Option<ProgramCursor> {
    for (i, stmt) in stmts.iter().enumerate().rev() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        // Check inside IfBlocks and WithEnvFile blocks for nested completed statements first.
        match stmt {
            CrateStatement::If(block) => {
                let state_dir = state_base.join(cursor.to_path());
                if let Ok(chosen) = fs_err::read_to_string(state_dir.join("chosen_branch")) {
                    let nested = match chosen.trim() {
                        "else" => {
                            let p = cursor.clone().with(CursorSegment::ElseBranch);
                            find_last_completed_crate_stmt(&block.else_statements, &p, state_base)
                        }
                        s => s.parse::<usize>().ok().and_then(|n| {
                            block.branches.get(n).and_then(|branch| {
                                let p = cursor.clone().with(CursorSegment::IfBranch(n));
                                find_last_completed_crate_stmt(&branch.statements, &p, state_base)
                            })
                        }),
                    };
                    if nested.is_some() {
                        return nested;
                    }
                }
            }
            CrateStatement::WithEnvFile(block) => {
                let p = cursor.clone().with(CursorSegment::WithEnvFile);
                let nested = find_last_completed_crate_stmt(&block.statements, &p, state_base);
                if nested.is_some() {
                    return nested;
                }
            }
            CrateStatement::Run(_)
            | CrateStatement::ManualStep(_)
            | CrateStatement::SnapshotMetadata(_)
            | CrateStatement::WaitForContinue(_) => {}
        }
        if is_crate_stmt_completed(stmt, &cursor, state_base) {
            return Some(cursor);
        }
    }
    None
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
) -> Option<ProgramCursor> {
    for (i, stmt) in stmts.iter().enumerate().rev() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        match stmt {
            WorkspaceStatement::If(block) => {
                let state_dir = state_base.join(cursor.to_path());
                if let Ok(chosen) = fs_err::read_to_string(state_dir.join("chosen_branch")) {
                    let nested = match chosen.trim() {
                        "else" => {
                            let p = cursor.clone().with(CursorSegment::ElseBranch);
                            find_last_completed_workspace_stmt(
                                &block.else_statements,
                                &p,
                                member_crates,
                                state_base,
                            )
                        }
                        s => s.parse::<usize>().ok().and_then(|n| {
                            block.branches.get(n).and_then(|branch| {
                                let p = cursor.clone().with(CursorSegment::IfBranch(n));
                                find_last_completed_workspace_stmt(
                                    &branch.statements,
                                    &p,
                                    member_crates,
                                    state_base,
                                )
                            })
                        }),
                    };
                    if nested.is_some() {
                        return nested;
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
                );
                if nested.is_some() {
                    return nested;
                }
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                for (c_idx, _) in member_crates.iter().enumerate().rev() {
                    let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
                    let nested =
                        find_last_completed_crate_stmt(&block.statements, &c_prefix, state_base);
                    if nested.is_some() {
                        return nested;
                    }
                }
            }
            WorkspaceStatement::Run(_)
            | WorkspaceStatement::ManualStep(_)
            | WorkspaceStatement::SnapshotMetadata(_)
            | WorkspaceStatement::WaitForContinue(_) => {}
        }
        if is_workspace_stmt_completed(stmt, &cursor, member_crates, state_base) {
            return Some(cursor);
        }
    }
    None
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

    match find_next_statement(&program, &resolved, &state_base) {
        NextOutcome::Next(next) => {
            println!(
                "Running statement at {} for {}",
                next.cursor,
                next.manifest_dir.display()
            );
            let extra_env = load_env_vars_from_files(&next.env_file_paths, next.manifest_dir)?;
            match next.action {
                StatementAction::RunCommand(step) => {
                    execute_run_step(
                        step,
                        &next.cursor,
                        next.manifest_dir,
                        &state_base,
                        &environment,
                        &extra_env,
                    )
                    .await?;
                }
                StatementAction::ManualStep(step) => {
                    execute_manual_step(
                        step,
                        &next.cursor,
                        next.manifest_dir,
                        &state_base,
                        &environment,
                        &extra_env,
                    )
                    .await?;
                }
                StatementAction::EvaluateWorkspaceIf(block) => {
                    evaluate_workspace_if_block(
                        block,
                        &next.cursor,
                        next.manifest_dir,
                        &state_base,
                        &environment,
                        &config,
                        &extra_env,
                    )?;
                }
                StatementAction::EvaluateCrateIf(block) => {
                    evaluate_crate_if_block(
                        block,
                        &next.cursor,
                        next.manifest_dir,
                        &state_base,
                        &environment,
                        &config,
                        &extra_env,
                    )?;
                }
                StatementAction::SnapshotMetadata(step) => {
                    execute_snapshot_metadata_step(
                        step,
                        &next.cursor,
                        next.manifest_dir,
                        &state_base,
                    )
                    .await?;
                }
                StatementAction::WaitForContinue(node) => {
                    let state_dir = state_base.join(next.cursor.to_path());
                    crate::utils::create_user_dir_all(&state_dir)
                        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;
                    println!(
                        "Wait barrier reached at {}: \"{}\". Release with `cargo-for-each task continue --name {} --cursor {}`.",
                        next.cursor.to_path_string(),
                        node.description,
                        params.name,
                        next.cursor.to_path_string()
                    );
                }
            }
        }
        NextOutcome::Suspended => {
            let barriers = find_waiting_barriers(&program, &resolved, &state_base);
            if barriers.is_empty() {
                // No barrier surfaced on disk, but find_next reported the
                // tree as blocked — fall back to a generic message.
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
                        "  {cursor_str}: \"{description}\" — release with `cargo-for-each task continue --name {} --cursor {cursor_str}`",
                        params.name
                    );
                }
            }
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
        if !are_workspace_deps_completed(ws_exec, &ws_map, ws_stmts, &resolved, &state_base) {
            continue;
        }
        if is_workspace_completed(ws_idx, ws_exec, ws_stmts, &state_base) {
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
        if !are_standalone_crate_deps_completed(crate_exec, &crate_map, crate_stmts, &state_base) {
            continue;
        }
        if is_standalone_crate_completed(c_idx, crate_stmts, &state_base) {
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

    // Phase 1: workspaces
    {
        let n = resolved.workspace_executions.len();
        let mut completed = vec![false; n];
        let mut failed = vec![false; n];
        // `suspended[idx]` marks a workspace that returned
        // `StepOutcome::Suspended`: its work isn't done, but it can't progress
        // until the user releases a barrier with `task continue`. Dependents
        // must therefore NOT see it as completed.
        let mut suspended = vec![false; n];
        let mut has_errors = false;

        loop {
            let ws_map: HashMap<PathBuf, usize> = resolved
                .workspace_executions
                .iter()
                .enumerate()
                .map(|(i, w)| (w.manifest_dir.clone(), i))
                .collect();

            let ready: Vec<(usize, PathBuf, Vec<ResolvedCrateExecution>)> = resolved
                .workspace_executions
                .iter()
                .enumerate()
                .filter(|(idx, ws_exec)| {
                    !completed.get(*idx).copied().unwrap_or(false)
                        && !failed.get(*idx).copied().unwrap_or(false)
                        && !suspended.get(*idx).copied().unwrap_or(false)
                        && ws_exec.dependencies.iter().all(|dep| {
                            ws_map.get(dep).is_none_or(|&dep_idx| {
                                completed.get(dep_idx).copied().unwrap_or(false)
                            })
                        })
                })
                .map(|(idx, ws_exec)| {
                    (
                        idx,
                        ws_exec.manifest_dir.clone(),
                        ws_exec.member_crates.clone(),
                    )
                })
                .collect();

            if ready.is_empty() {
                break;
            }

            let results: Vec<(usize, Result<StepOutcome, Error>)> = stream::iter(ready)
                .map(|(ws_idx, manifest_dir, member_crates)| {
                    let ws_stmts = Arc::clone(&ws_stmts);
                    let config = Arc::clone(&config);
                    let state_base = Arc::clone(&state_base);
                    let environment = environment.clone();
                    let task_name = params.name.clone();
                    async move {
                        let prefix =
                            ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx));
                        let result = run_workspace_stmts_to_completion(
                            &ws_stmts,
                            &prefix,
                            &manifest_dir,
                            &member_crates,
                            &state_base,
                            &environment,
                            &config,
                            &[],
                            &task_name,
                        )
                        .await;
                        (ws_idx, result)
                    }
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
                            tracing::error!("Workspace failed: {}", e);
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
        // When some targets are suspended (or transitively blocked by a
        // suspended/failed upstream), it is not a circular dependency; the
        // user can release barriers with `task continue` and re-run.
        if !suspended.iter().any(|&s| s) && !completed.iter().all(|&c| c) {
            return Err(Error::CircularDependency);
        }
    }

    // Phase 2: standalone crates
    {
        let n = resolved.crate_executions.len();
        let mut completed = vec![false; n];
        let mut failed = vec![false; n];
        let mut suspended = vec![false; n];
        let mut has_errors = false;

        loop {
            let crate_map: HashMap<PathBuf, usize> = resolved
                .crate_executions
                .iter()
                .enumerate()
                .map(|(i, c)| (c.manifest_dir.clone(), i))
                .collect();

            let ready: Vec<(usize, PathBuf)> = resolved
                .crate_executions
                .iter()
                .enumerate()
                .filter(|(idx, crate_exec)| {
                    !completed.get(*idx).copied().unwrap_or(false)
                        && !failed.get(*idx).copied().unwrap_or(false)
                        && !suspended.get(*idx).copied().unwrap_or(false)
                        && crate_exec.dependencies.iter().all(|dep| {
                            crate_map.get(dep).is_none_or(|&dep_idx| {
                                completed.get(dep_idx).copied().unwrap_or(false)
                            })
                        })
                })
                .map(|(idx, crate_exec)| (idx, crate_exec.manifest_dir.clone()))
                .collect();

            if ready.is_empty() {
                break;
            }

            let results: Vec<(usize, Result<StepOutcome, Error>)> = stream::iter(ready)
                .map(|(c_idx, manifest_dir)| {
                    let crate_stmts = Arc::clone(&crate_stmts);
                    let config = Arc::clone(&config);
                    let state_base = Arc::clone(&state_base);
                    let environment = environment.clone();
                    let task_name = params.name.clone();
                    async move {
                        let prefix =
                            ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx));
                        let result = run_crate_stmts_to_completion(
                            &crate_stmts,
                            &prefix,
                            &manifest_dir,
                            &state_base,
                            &environment,
                            &config,
                            &[],
                            &task_name,
                        )
                        .await;
                        (c_idx, result)
                    }
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
                            tracing::error!("Crate execution failed: {}", e);
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
        if !suspended.iter().any(|&s| s) && !completed.iter().all(|&c| c) {
            return Err(Error::CircularDependency);
        }
    }

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
        if is_standalone_crate_completed(c_idx, crate_stmts, &state_base) {
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
        if is_workspace_completed(ws_idx, ws_exec, ws_stmts, &state_base) {
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
        if let Some(cursor) = find_last_completed_crate_stmt(crate_stmts, &prefix, &state_base) {
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
        ) {
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

/// Recursively prints crate statements with their cursor, completion icon, and label.
#[expect(clippy::print_stdout, reason = "part of the describe UI")]
fn print_crate_stmts_describe(
    stmts: &[CrateStatement],
    prefix: &ProgramCursor,
    state_base: &Path,
    indent: &str,
) {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());
        let cursor_str = cursor.to_path_string();

        match stmt {
            CrateStatement::If(block) => {
                let chosen = fs_err::read_to_string(state_dir.join("chosen_branch"))
                    .ok()
                    .unwrap_or_default();
                let chosen = chosen.trim();
                let (icon, label) = if chosen.is_empty() {
                    ("\u{2B1C}", "if [not yet evaluated]")
                } else if chosen == "none" {
                    ("\u{2705}", "if [no branch matched]")
                } else if chosen == "else" {
                    ("\u{2705}", "if [else branch taken]")
                } else {
                    ("\u{2705}", "if [branch taken]")
                };
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
                if chosen == "else" {
                    let nested_indent = format!("{indent}  ");
                    print_crate_stmts_describe(
                        &block.else_statements,
                        &cursor.with(CursorSegment::ElseBranch),
                        state_base,
                        &nested_indent,
                    );
                } else if let Ok(n) = chosen.parse::<usize>()
                    && let Some(branch) = block.branches.get(n)
                {
                    let nested_indent = format!("{indent}  ");
                    print_crate_stmts_describe(
                        &branch.statements,
                        &cursor.with(CursorSegment::IfBranch(n)),
                        state_base,
                        &nested_indent,
                    );
                }
            }
            CrateStatement::WithEnvFile(block) => {
                let env_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                let icon = if is_crate_stmts_completed(&block.statements, &env_prefix, state_base) {
                    "\u{2705}"
                } else {
                    "\u{2B1C}"
                };
                let label = format!("with_env_file \"{}\"", block.env_file);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
                let nested_indent = format!("{indent}  ");
                print_crate_stmts_describe(
                    &block.statements,
                    &env_prefix,
                    state_base,
                    &nested_indent,
                );
            }
            CrateStatement::Run(_) => {
                let state_dir = state_base.join(cursor.to_path());
                let icon = if is_run_completed(&state_dir) {
                    "\u{2705}"
                } else if is_run_failed(&state_dir) {
                    "\u{274C}"
                } else {
                    "\u{2B1C}"
                };
                let label = crate_stmt_label(stmt);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
            }
            CrateStatement::WaitForContinue(node) => {
                let icon = if is_wait_barrier_released(&state_dir) {
                    "\u{2705}"
                } else if is_wait_barrier_waiting(&state_dir) {
                    "\u{23F3}"
                } else {
                    "\u{2B1C}"
                };
                let label = format!("wait_for_continue \"{}\"", node.description);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
            }
            CrateStatement::ManualStep(_) | CrateStatement::SnapshotMetadata(_) => {
                let icon = if is_crate_stmt_completed(stmt, &cursor, state_base) {
                    "\u{2705}"
                } else {
                    "\u{2B1C}"
                };
                let label = crate_stmt_label(stmt);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
            }
        }
    }
}

/// Recursively prints workspace statements with their cursor, completion icon, and label.
#[expect(clippy::print_stdout, reason = "part of the describe UI")]
fn print_workspace_stmts_describe(
    stmts: &[WorkspaceStatement],
    prefix: &ProgramCursor,
    member_crates: &[ResolvedCrateExecution],
    state_base: &Path,
    indent: &str,
) {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());
        let cursor_str = cursor.to_path_string();

        match stmt {
            WorkspaceStatement::If(block) => {
                let chosen = fs_err::read_to_string(state_dir.join("chosen_branch"))
                    .ok()
                    .unwrap_or_default();
                let chosen = chosen.trim();
                let (icon, label) = if chosen.is_empty() {
                    ("\u{2B1C}", "if [not yet evaluated]")
                } else if chosen == "none" {
                    ("\u{2705}", "if [no branch matched]")
                } else if chosen == "else" {
                    ("\u{2705}", "if [else branch taken]")
                } else {
                    ("\u{2705}", "if [branch taken]")
                };
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
                if chosen == "else" {
                    let nested_indent = format!("{indent}  ");
                    print_workspace_stmts_describe(
                        &block.else_statements,
                        &cursor.with(CursorSegment::ElseBranch),
                        member_crates,
                        state_base,
                        &nested_indent,
                    );
                } else if let Ok(n) = chosen.parse::<usize>()
                    && let Some(branch) = block.branches.get(n)
                {
                    let nested_indent = format!("{indent}  ");
                    print_workspace_stmts_describe(
                        &branch.statements,
                        &cursor.with(CursorSegment::IfBranch(n)),
                        member_crates,
                        state_base,
                        &nested_indent,
                    );
                }
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                let icon = if is_workspace_stmt_completed(stmt, &cursor, member_crates, state_base)
                {
                    "\u{2705}"
                } else {
                    "\u{2B1C}"
                };
                println!("{indent}{cursor_str:<20}  {icon}  for crate in workspace");
                let crate_indent = format!("{indent}  ");
                let nested_indent = format!("{indent}    ");
                for (c_idx, crate_exec) in member_crates.iter().enumerate() {
                    let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
                    let c_prefix_str = c_prefix.to_path_string();
                    let crate_icon =
                        if is_crate_stmts_completed(&block.statements, &c_prefix, state_base) {
                            "\u{2705}"
                        } else {
                            "\u{2B1C}"
                        };
                    println!(
                        "{crate_indent}{c_prefix_str:<20}  {crate_icon}  crate {}",
                        crate_exec.manifest_dir.display()
                    );
                    print_crate_stmts_describe(
                        &block.statements,
                        &c_prefix,
                        state_base,
                        &nested_indent,
                    );
                }
            }
            WorkspaceStatement::WithEnvFile(block) => {
                let env_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                let icon = if is_workspace_stmts_completed(
                    &block.statements,
                    &env_prefix,
                    member_crates,
                    state_base,
                ) {
                    "\u{2705}"
                } else {
                    "\u{2B1C}"
                };
                let label = format!("with_env_file \"{}\"", block.env_file);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
                let nested_indent = format!("{indent}  ");
                print_workspace_stmts_describe(
                    &block.statements,
                    &env_prefix,
                    member_crates,
                    state_base,
                    &nested_indent,
                );
            }
            WorkspaceStatement::Run(_) => {
                let state_dir = state_base.join(cursor.to_path());
                let icon = if is_run_completed(&state_dir) {
                    "\u{2705}"
                } else if is_run_failed(&state_dir) {
                    "\u{274C}"
                } else {
                    "\u{2B1C}"
                };
                let label = workspace_stmt_label(stmt);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
            }
            WorkspaceStatement::WaitForContinue(node) => {
                let icon = if is_wait_barrier_released(&state_dir) {
                    "\u{2705}"
                } else if is_wait_barrier_waiting(&state_dir) {
                    "\u{23F3}"
                } else {
                    "\u{2B1C}"
                };
                let label = format!("wait_for_continue \"{}\"", node.description);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
            }
            WorkspaceStatement::ManualStep(_) | WorkspaceStatement::SnapshotMetadata(_) => {
                let icon = if is_workspace_stmt_completed(stmt, &cursor, member_crates, state_base)
                {
                    "\u{2705}"
                } else {
                    "\u{2B1C}"
                };
                let label = workspace_stmt_label(stmt);
                println!("{indent}{cursor_str:<20}  {icon}  {label}");
            }
        }
    }
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
            let done = is_workspace_completed(ws_idx, ws_exec, ws_stmts, &state_base);
            let icon = if done { "\u{2705}" } else { "\u{2B1C}" };
            println!("  {} {}", icon, ws_exec.manifest_dir.display());
            print_workspace_stmts_describe(
                ws_stmts,
                &ProgramCursor::new().with(CursorSegment::WorkspaceIteration(ws_idx)),
                &ws_exec.member_crates,
                &state_base,
                "    ",
            );
        }
    }

    let crate_stmts = first_crate_stmts(&program);
    if !resolved.crate_executions.is_empty() {
        println!("Standalone crates:");
        for (c_idx, crate_exec) in resolved.crate_executions.iter().enumerate() {
            let done = is_standalone_crate_completed(c_idx, crate_stmts, &state_base);
            let icon = if done { "\u{2705}" } else { "\u{2B1C}" };
            println!("  {} {}", icon, crate_exec.manifest_dir.display());
            print_crate_stmts_describe(
                crate_stmts,
                &ProgramCursor::new().with(CursorSegment::CrateIteration(c_idx)),
                &state_base,
                "    ",
            );
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

/// Walks `cursor` through `program` and returns whether the statement it
/// addresses is a `wait_for_continue` (in either workspace or crate context).
///
/// Returns `false` when the cursor structure does not match the program (the
/// terminal statement is not reachable) or when the terminal statement is a
/// different kind.
fn cursor_targets_wait_for_continue(program: &Program, cursor: &ProgramCursor) -> bool {
    fn walk_crate(stmts: &[CrateStatement], segs: &[CursorSegment]) -> bool {
        let Some((first, rest)) = segs.split_first() else {
            return false;
        };
        let CursorSegment::Statement(n) = *first else {
            return false;
        };
        let Some(stmt) = stmts.get(n) else {
            return false;
        };
        if rest.is_empty() {
            return matches!(stmt, CrateStatement::WaitForContinue(_));
        }
        match (stmt, rest.split_first()) {
            (CrateStatement::If(block), Some((CursorSegment::IfBranch(b), rest))) => block
                .branches
                .get(*b)
                .is_some_and(|branch| walk_crate(&branch.statements, rest)),
            (CrateStatement::If(block), Some((CursorSegment::ElseBranch, rest))) => {
                walk_crate(&block.else_statements, rest)
            }
            (CrateStatement::WithEnvFile(block), Some((CursorSegment::WithEnvFile, rest))) => {
                walk_crate(&block.statements, rest)
            }
            _ => false,
        }
    }
    fn walk_workspace(stmts: &[WorkspaceStatement], segs: &[CursorSegment]) -> bool {
        let Some((first, rest)) = segs.split_first() else {
            return false;
        };
        let CursorSegment::Statement(n) = *first else {
            return false;
        };
        let Some(stmt) = stmts.get(n) else {
            return false;
        };
        if rest.is_empty() {
            return matches!(stmt, WorkspaceStatement::WaitForContinue(_));
        }
        match (stmt, rest.split_first()) {
            (WorkspaceStatement::If(block), Some((CursorSegment::IfBranch(b), rest))) => block
                .branches
                .get(*b)
                .is_some_and(|branch| walk_workspace(&branch.statements, rest)),
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
            _ => false,
        }
    }

    let Some((first, rest)) = cursor.segments().split_first() else {
        return false;
    };
    match first {
        CursorSegment::WorkspaceIteration(_) => program.statements.iter().any(|s| match s {
            GlobalStatement::ForWorkspace(b) => walk_workspace(&b.statements, rest),
            _ => false,
        }),
        CursorSegment::CrateIteration(_) => program.statements.iter().any(|s| match s {
            GlobalStatement::ForCrate(b) => walk_crate(&b.statements, rest),
            _ => false,
        }),
        _ => false,
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
    if !cursor_targets_wait_for_continue(&program, &cursor) {
        return Err(Error::CursorNotAtBarrier(cursor.to_path_string()));
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
    use std::path::{Path, PathBuf};

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::{
        NextOutcome, find_next_statement, is_crate_stmt_completed, is_run_completed,
        validate_task_name,
    };
    use crate::Environment;
    use crate::error::Error;
    use crate::program::ast::common::{RunStep, WaitForContinueNode};
    use crate::program::ast::crate_ctx::CrateStatement;
    use crate::program::ast::crate_ctx::ForCrateBlock;
    use crate::program::ast::workspace_ctx::{
        ForCrateInWorkspaceBlock, ForWorkspaceBlock, WorkspaceStatement,
    };
    use crate::program::cursor::{CursorSegment, ProgramCursor};
    use crate::program::resolve::{
        ResolvedCrateExecution, ResolvedProgram, ResolvedWorkspaceExecution,
    };
    use crate::program::{GlobalStatement, Program};

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

    // ── is_run_completed ──────────────────────────────────────────────────────

    #[test]
    fn run_completed_no_state_dir() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        assert!(!is_run_completed(&state_dir));
        Ok(())
    }

    #[test]
    fn run_completed_no_exit_status_file() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        assert!(!is_run_completed(&state_dir));
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_zero() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        crate::utils::write_user_file(state_dir.join("exit_status"), "0")?;
        assert!(is_run_completed(&state_dir));
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_nonzero() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        crate::utils::write_user_file(state_dir.join("exit_status"), "1")?;
        assert!(!is_run_completed(&state_dir));
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_empty_is_failed() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        crate::utils::create_user_dir_all(&state_dir)?;
        crate::utils::write_user_file(state_dir.join("exit_status"), "")?;
        assert!(!is_run_completed(&state_dir));
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
        assert!(is_crate_stmt_completed(&stmt, &cursor, temp.path()));
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
        assert!(!is_crate_stmt_completed(&stmt, &cursor, temp.path()));
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
            find_next_statement(&program, &resolved, &state_base),
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
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base) else {
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
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base) else {
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
        let outcome = find_next_statement(&program, &resolved, &state_base);
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
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base) else {
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

        let outcome = find_next_statement(&program, &resolved, &state_base);
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
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base) else {
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
        let NextOutcome::Next(next) = find_next_statement(&program, &resolved, &state_base) else {
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
}
