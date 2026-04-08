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
use crate::program::ast::common::{ManualStepNode, RunStep, SnapshotMetadataNode};
use crate::program::ast::crate_ctx::{CrateIfBlock, CrateStatement};
use crate::program::ast::workspace_ctx::{WorkspaceIfBlock, WorkspaceStatement};
use crate::program::cursor::{CursorSegment, ProgramCursor};
use crate::program::evaluate::{evaluate_crate_condition, evaluate_workspace_condition};
use crate::program::resolve::{
    ResolvedCrateExecution, ResolvedProgram, ResolvedWorkspaceExecution,
};
use crate::program::{GlobalStatement, Program};
use crate::{Config, Environment};
use clap::Parser;

// ── Path helpers ───────────────────────────────────────────────────────────────

/// Returns the tasks configuration directory path.
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn dir_path(environment: &crate::Environment) -> Result<PathBuf, Error> {
    Ok(crate::config_dir_path(environment)?.join("tasks"))
}

/// Returns the path to a specific task's configuration directory.
///
/// # Errors
///
/// Returns an error if the tasks directory path cannot be determined.
pub fn named_dir_path(name: &str, environment: &crate::Environment) -> Result<PathBuf, Error> {
    Ok(dir_path(environment)?.join(name))
}

/// Returns the path to a specific task's execution state directory.
///
/// # Errors
///
/// Returns an error if the state directory path cannot be determined.
pub fn state_dir_for_task(name: &str, environment: &crate::Environment) -> Result<PathBuf, Error> {
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

/// Returns `true` if the `run` step at `state_dir` has a recorded non-zero exit status.
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
    /// Env file paths (relative to `manifest_dir`) from enclosing `with_env_file` blocks,
    /// ordered from outermost to innermost.
    pub env_file_paths: Vec<String>,
}

/// Finds the first uncompleted crate statement in `stmts` starting at `prefix`.
///
/// Returns `None` if all statements are completed.
fn find_next_in_crate_stmts<'a>(
    stmts: &'a [CrateStatement],
    prefix: &ProgramCursor,
    manifest_dir: &'a Path,
    state_base: &Path,
    env_file_paths: &[String],
) -> Option<NextStatement<'a>> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            CrateStatement::Run(step) => {
                if !is_run_completed(&state_dir) {
                    return Some(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::RunCommand(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            CrateStatement::ManualStep(step) => {
                if !is_manual_completed(&state_dir) {
                    return Some(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::ManualStep(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            CrateStatement::SnapshotMetadata(step) => {
                if !is_snapshot_metadata_completed(&state_dir) {
                    return Some(NextStatement {
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
                        return Some(NextStatement {
                            cursor,
                            manifest_dir,
                            action: StatementAction::EvaluateCrateIf(block),
                            env_file_paths: env_file_paths.to_vec(),
                        });
                    }
                    Ok(chosen) => {
                        let nested = match chosen.trim() {
                            "none" => None,
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
                            s => s.parse::<usize>().ok().and_then(|n| {
                                block.branches.get(n).and_then(|branch| {
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
                        if nested.is_some() {
                            return nested;
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
                if nested.is_some() {
                    return nested;
                }
            }
        }
    }
    None
}

/// Finds the first uncompleted workspace statement in `stmts` starting at `prefix`.
///
/// Returns `None` if all statements (including nested `for crate in workspace`) are done.
fn find_next_in_workspace_stmts<'a>(
    stmts: &'a [WorkspaceStatement],
    prefix: &ProgramCursor,
    manifest_dir: &'a Path,
    member_crates: &'a [ResolvedCrateExecution],
    state_base: &Path,
    env_file_paths: &[String],
) -> Option<NextStatement<'a>> {
    for (i, stmt) in stmts.iter().enumerate() {
        let cursor = prefix.clone().with(CursorSegment::Statement(i));
        let state_dir = state_base.join(cursor.to_path());

        match stmt {
            WorkspaceStatement::Run(step) => {
                if !is_run_completed(&state_dir) {
                    return Some(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::RunCommand(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            WorkspaceStatement::ManualStep(step) => {
                if !is_manual_completed(&state_dir) {
                    return Some(NextStatement {
                        cursor,
                        manifest_dir,
                        action: StatementAction::ManualStep(step),
                        env_file_paths: env_file_paths.to_vec(),
                    });
                }
            }
            WorkspaceStatement::SnapshotMetadata(step) => {
                if !is_snapshot_metadata_completed(&state_dir) {
                    return Some(NextStatement {
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
                        return Some(NextStatement {
                            cursor,
                            manifest_dir,
                            action: StatementAction::EvaluateWorkspaceIf(block),
                            env_file_paths: env_file_paths.to_vec(),
                        });
                    }
                    Ok(chosen) => {
                        let nested = match chosen.trim() {
                            "none" => None,
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
                            s => s.parse::<usize>().ok().and_then(|n| {
                                block.branches.get(n).and_then(|branch| {
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
                        if nested.is_some() {
                            return nested;
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
                if nested.is_some() {
                    return nested;
                }
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                let crate_map: HashMap<PathBuf, usize> = member_crates
                    .iter()
                    .enumerate()
                    .map(|(ci, c)| (c.manifest_dir.clone(), ci))
                    .collect();

                for (c_idx, crate_exec) in member_crates.iter().enumerate() {
                    if !are_member_crate_deps_completed(
                        crate_exec,
                        &crate_map,
                        &cursor,
                        &block.statements,
                        state_base,
                    ) {
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
                    if nested.is_some() {
                        return nested;
                    }
                }
                // All member crates done — continue to next workspace statement.
            }
        }
    }
    None
}

/// Finds the next uncompleted statement across all workspaces and standalone crates,
/// respecting inter-target dependency ordering.
///
/// Returns `None` when every statement in every target has been completed.
#[must_use]
pub fn find_next_statement<'a>(
    program: &'a Program,
    resolved: &'a ResolvedProgram,
    state_base: &Path,
) -> Option<NextStatement<'a>> {
    let ws_stmts = first_workspace_stmts(program);
    let ws_map: HashMap<PathBuf, usize> = resolved
        .workspace_executions
        .iter()
        .enumerate()
        .map(|(i, w)| (w.manifest_dir.clone(), i))
        .collect();

    for (ws_idx, ws_exec) in resolved.workspace_executions.iter().enumerate() {
        if !are_workspace_deps_completed(ws_exec, &ws_map, ws_stmts, resolved, state_base) {
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
        if next.is_some() {
            return next;
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
        if next.is_some() {
            return next;
        }
    }

    None
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

/// Looks up a single `${name.field_path}` reference and returns its string value.
///
/// The lookup first checks for a per-manifest snapshot captured when the current
/// `manifest_dir` ran the step, then falls back to `latest.json` for cross-context
/// scenarios.  The package for the current crate is found in the snapshot by matching
/// its `manifest_path` against `manifest_dir/Cargo.toml`, and the dot-separated
/// `field_path` is then navigated within that package's JSON.
///
/// # Errors
///
/// Returns an error if no snapshot named `snapshot_name` exists, if the current
/// crate's package cannot be found in the snapshot, or if `field_path` does not
/// exist or is not navigable within the package JSON.
fn resolve_interpolation(
    snapshot_name: &str,
    field_path: &str,
    manifest_dir: &Path,
    state_base: &Path,
) -> Result<String, Error> {
    let name_dir = state_base.join("snapshots").join(snapshot_name);
    let hex_key: String = manifest_dir
        .to_string_lossy()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .concat();
    let mut filename = hex_key;
    filename.push_str(".json");
    let per_manifest_path = name_dir.join("by_manifest").join(&filename);
    let latest_path = name_dir.join("latest.json");
    let json_path = if per_manifest_path.exists() {
        per_manifest_path
    } else if latest_path.exists() {
        latest_path
    } else {
        return Err(Error::SnapshotNotFound(snapshot_name.to_owned()));
    };
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
/// The snapshot is written to two locations within `state_base/snapshots/{name}/`:
/// - `by_manifest/{hex_encoded_manifest_dir}.json`: for exact per-context lookup.
/// - `latest.json`: overwritten on every capture to serve as a cross-context fallback.
///
/// A completion marker is written to `state_dir/snapshot_metadata_completed`.
///
/// # Errors
///
/// Returns an error if `cargo metadata` fails, if the JSON cannot be serialized,
/// or if any filesystem operation fails.
async fn execute_snapshot_metadata_step(
    step: &SnapshotMetadataNode,
    cursor: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
) -> Result<(), Error> {
    let state_dir = state_base.join(cursor.to_path());
    fs_err::create_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_dir.join("Cargo.toml"))
        .exec()
        .map_err(|e| Error::CargoMetadataError(manifest_dir.to_path_buf(), e))?;
    let json = serde_json::to_string_pretty(&metadata)
        .map_err(Error::CouldNotSerializeMetadataSnapshot)?;
    let name_dir = state_base.join("snapshots").join(&step.name);
    let by_manifest_dir = name_dir.join("by_manifest");
    fs_err::create_dir_all(&by_manifest_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(by_manifest_dir.clone(), e))?;
    let hex_key: String = manifest_dir
        .to_string_lossy()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .concat();
    let mut filename = hex_key;
    filename.push_str(".json");
    let per_manifest_path = by_manifest_dir.join(&filename);
    fs_err::write(&per_manifest_path, &json)
        .map_err(|e| Error::CouldNotWriteStateFile(per_manifest_path.clone(), e))?;
    let latest_path = name_dir.join("latest.json");
    fs_err::write(&latest_path, &json)
        .map_err(|e| Error::CouldNotWriteStateFile(latest_path.clone(), e))?;
    let marker = state_dir.join("snapshot_metadata_completed");
    fs_err::write(&marker, "done").map_err(|e| Error::CouldNotWriteStateFile(marker.clone(), e))?;
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
    fs_err::create_dir_all(&state_dir)
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

    let command_str = format!(
        "{} {}",
        command,
        args.iter()
            .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" ")
    );

    println!("Running: {command_str}");

    let wrapper_path = state_dir.join("run_wrapper.sh");
    let exit_status_path = state_dir.join("exit_status");
    let script = format!(
        "#!/bin/sh\n{command_str}\nrc=$?\nprintf '%d' \"$rc\" > \"$CARGO_FOR_EACH_EXIT_STATUS_PATH\"\nexit \"$rc\"\n"
    );
    fs_err::write(&wrapper_path, &script)
        .map_err(|e| Error::CouldNotWriteStateFile(wrapper_path.clone(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(0o755);
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
            fs_err::write(&exit_status_path, "")
                .map_err(|we| Error::CouldNotWriteStateFile(exit_status_path, we))?;
            Err(e)
        }
        Ok(_) => {
            let exit_code: i32 = fs_err::read_to_string(&exit_status_path)
                .ok()
                .as_deref()
                .map(str::trim)
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1);

            if !exit_status_path.exists() {
                fs_err::write(&exit_status_path, exit_code.to_string())
                    .map_err(|e| Error::CouldNotWriteStateFile(exit_status_path, e))?;
            }

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
    fs_err::create_dir_all(&state_dir)
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
    fs_err::write(
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
    fs_err::create_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;

    let mut chosen: Option<usize> = None;
    for (i, branch) in block.branches.iter().enumerate() {
        if evaluate_workspace_condition(
            &branch.condition,
            manifest_dir,
            environment,
            config,
            extra_env,
        )? && chosen.is_none()
        {
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
    let chosen_branch_path = state_dir.join("chosen_branch");
    fs_err::write(&chosen_branch_path, &chosen_str)
        .map_err(|e| Error::CouldNotWriteStateFile(chosen_branch_path, e))?;
    Ok(())
}

/// Evaluates the branch conditions of a crate `if` block and writes `chosen_branch`.
///
/// # Errors
///
/// Returns an error if condition evaluation fails or the state file cannot be written.
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
    fs_err::create_dir_all(&state_dir)
        .map_err(|e| Error::CouldNotCreateStateDir(state_dir.clone(), e))?;

    let mut chosen: Option<usize> = None;
    for (i, branch) in block.branches.iter().enumerate() {
        if evaluate_crate_condition(
            &branch.condition,
            manifest_dir,
            environment,
            config,
            extra_env,
        )? && chosen.is_none()
        {
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
    let chosen_branch_path = state_dir.join("chosen_branch");
    fs_err::write(&chosen_branch_path, &chosen_str)
        .map_err(|e| Error::CouldNotWriteStateFile(chosen_branch_path, e))?;
    Ok(())
}

/// Runs all crate statements to completion, skipping already-completed ones.
///
/// Handles `if` blocks by evaluating conditions if not yet done, then running
/// the chosen branch's statements recursively.
///
/// # Errors
///
/// Returns an error if any statement fails.
async fn run_crate_stmts_to_completion(
    stmts: &[CrateStatement],
    prefix: &ProgramCursor,
    manifest_dir: &Path,
    state_base: &Path,
    environment: &Environment,
    config: &Config,
    extra_env: &[(String, String)],
) -> Result<(), Error> {
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
                    .unwrap_or_else(|_| "none".to_owned());
                match chosen.trim() {
                    "none" => {}
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
                        ))
                        .await?;
                    }
                    s => {
                        if let Ok(n) = s.trim().parse::<usize>()
                            && let Some(branch) = block.branches.get(n)
                        {
                            let p = cursor.clone().with(CursorSegment::IfBranch(n));
                            Box::pin(run_crate_stmts_to_completion(
                                &branch.statements,
                                &p,
                                manifest_dir,
                                state_base,
                                environment,
                                config,
                                extra_env,
                            ))
                            .await?;
                        }
                    }
                }
            }
            CrateStatement::WithEnvFile(block) => {
                let file_vars = load_env_file(&manifest_dir.join(&block.env_file))?;
                let mut combined = extra_env.to_vec();
                combined.extend(file_vars);
                let inner_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                Box::pin(run_crate_stmts_to_completion(
                    &block.statements,
                    &inner_prefix,
                    manifest_dir,
                    state_base,
                    environment,
                    config,
                    &combined,
                ))
                .await?;
            }
        }
    }
    Ok(())
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
) -> Result<(), Error> {
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
                    .unwrap_or_else(|_| "none".to_owned());
                match chosen.trim() {
                    "none" => {}
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
                        ))
                        .await?;
                    }
                    s => {
                        if let Ok(n) = s.trim().parse::<usize>()
                            && let Some(branch) = block.branches.get(n)
                        {
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
                            ))
                            .await?;
                        }
                    }
                }
            }
            WorkspaceStatement::WithEnvFile(block) => {
                let file_vars = load_env_file(&manifest_dir.join(&block.env_file))?;
                let mut combined = extra_env.to_vec();
                combined.extend(file_vars);
                let inner_prefix = cursor.clone().with(CursorSegment::WithEnvFile);
                Box::pin(run_workspace_stmts_to_completion(
                    &block.statements,
                    &inner_prefix,
                    manifest_dir,
                    member_crates,
                    state_base,
                    environment,
                    config,
                    &combined,
                ))
                .await?;
            }
            WorkspaceStatement::ForCrateInWorkspace(block) => {
                // Member crates are already in intra-workspace dependency order.
                for (c_idx, crate_exec) in member_crates.iter().enumerate() {
                    let c_prefix = cursor.clone().with(CursorSegment::CrateIteration(c_idx));
                    run_crate_stmts_to_completion(
                        &block.statements,
                        &c_prefix,
                        &crate_exec.manifest_dir,
                        state_base,
                        environment,
                        config,
                        extra_env,
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
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
            | CrateStatement::SnapshotMetadata(_) => {}
        }
        if is_crate_stmt_completed(stmt, &cursor, state_base) {
            return Some(cursor);
        }
    }
    None
}

/// Finds the cursor of the last completed workspace statement (searched in reverse).
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
            | WorkspaceStatement::SnapshotMetadata(_) => {}
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

    let config = Config::load(&environment)?;
    let resolved = crate::program::resolve::resolve_program(&program, &config)?;

    let task_dir = named_dir_path(&params.name, &environment)?;
    if task_dir.exists() {
        return Err(Error::AlreadyExists(format!("task {}", params.name)));
    }
    fs_err::create_dir_all(&task_dir)
        .map_err(|e| Error::CouldNotCreateTaskDir(task_dir.clone(), e))?;

    fs_err::copy(&params.program, task_dir.join("program.cfe")).map_err(|e| {
        Error::CouldNotCopyFile(params.program.clone(), task_dir.join("program.cfe"), e)
    })?;

    let resolved_path = task_dir.join("resolved-program.toml");
    fs_err::write(
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

    if let Some(next) = find_next_statement(&program, &resolved, &state_base) {
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
                execute_snapshot_metadata_step(step, &next.cursor, next.manifest_dir, &state_base)
                    .await?;
            }
        }
    } else {
        println!("All statements for all targets completed successfully.");
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
        run_workspace_stmts_to_completion(
            ws_stmts,
            &prefix,
            &ws_exec.manifest_dir,
            &ws_exec.member_crates,
            &state_base,
            &environment,
            &config,
            &[],
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
        run_crate_stmts_to_completion(
            crate_stmts,
            &prefix,
            &crate_exec.manifest_dir,
            &state_base,
            &environment,
            &config,
            &[],
        )
        .await?;
        return Ok(());
    }

    println!("All targets are either completed or waiting for dependencies.");
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
/// circular dependency is detected.
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
    let resolved = Arc::new(resolved);

    let ws_stmts: Arc<Vec<WorkspaceStatement>> = Arc::new(first_workspace_stmts(&program).to_vec());
    let crate_stmts: Arc<Vec<CrateStatement>> = Arc::new(first_crate_stmts(&program).to_vec());

    // Phase 1: workspaces
    {
        let n = resolved.workspace_executions.len();
        let mut completed = vec![false; n];
        let mut failed = vec![false; n];
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

            let results: Vec<(usize, Result<(), Error>)> = stream::iter(ready)
                .map(|(ws_idx, manifest_dir, member_crates)| {
                    let ws_stmts = Arc::clone(&ws_stmts);
                    let config = Arc::clone(&config);
                    let state_base = Arc::clone(&state_base);
                    let environment = environment.clone();
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
                    Ok(()) => {
                        if let Some(slot) = completed.get_mut(idx) {
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
        if !completed.iter().all(|&c| c) {
            return Err(Error::CircularDependency);
        }
    }

    // Phase 2: standalone crates
    {
        let n = resolved.crate_executions.len();
        let mut completed = vec![false; n];
        let mut failed = vec![false; n];
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

            let results: Vec<(usize, Result<(), Error>)> = stream::iter(ready)
                .map(|(c_idx, manifest_dir)| {
                    let crate_stmts = Arc::clone(&crate_stmts);
                    let config = Arc::clone(&config);
                    let state_base = Arc::clone(&state_base);
                    let environment = environment.clone();
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
                    Ok(()) => {
                        if let Some(slot) = completed.get_mut(idx) {
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
        if !completed.iter().all(|&c| c) {
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
            _ => {
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
            _ => {
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
            fs_err::remove_dir_all(&task_dir)
                .map_err(|e| Error::CouldNotRemoveTaskDir(task_dir.clone(), e))?;
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::{find_next_statement, is_crate_stmt_completed, is_run_completed};
    use crate::Environment;
    use crate::program::ast::common::RunStep;
    use crate::program::ast::crate_ctx::CrateStatement;
    use crate::program::ast::crate_ctx::ForCrateBlock;
    use crate::program::ast::workspace_ctx::ForWorkspaceBlock;
    use crate::program::ast::workspace_ctx::WorkspaceStatement;
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
        fs_err::create_dir_all(&dir)?;
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
        fs_err::create_dir_all(&state_dir)?;
        assert!(!is_run_completed(&state_dir));
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_zero() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        fs_err::create_dir_all(&state_dir)?;
        fs_err::write(state_dir.join("exit_status"), "0")?;
        assert!(is_run_completed(&state_dir));
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_nonzero() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        fs_err::create_dir_all(&state_dir)?;
        fs_err::write(state_dir.join("exit_status"), "1")?;
        assert!(!is_run_completed(&state_dir));
        Ok(())
    }

    #[test]
    fn run_completed_exit_status_empty_is_failed() -> TestResult {
        let temp = tempdir()?;
        let state_dir = temp.path().join("w0").join("s0");
        fs_err::create_dir_all(&state_dir)?;
        fs_err::write(state_dir.join("exit_status"), "")?;
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
        fs_err::create_dir_all(&state_dir)?;
        fs_err::write(state_dir.join("exit_status"), "0")?;

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
        fs_err::write(stmt_dir.join("exit_status"), "0")?;

        let program = crate_program(vec![CrateStatement::Run(RunStep {
            command: "echo".to_owned(),
            args: vec![],
        })]);
        let resolved = resolved_with_one_crate(dir);
        assert!(find_next_statement(&program, &resolved, &state_base).is_none());
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
        let next = find_next_statement(&program, &resolved, &state_base);
        assert!(next.is_some());
        let next = next.ok_or("expected Some")?;
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
        fs_err::write(stmt0_dir.join("exit_status"), "0")?;

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
        let next = find_next_statement(&program, &resolved, &state_base);
        assert!(next.is_some());
        let next = next.ok_or("expected Some")?;
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
        fs_err::write(stmt_dir.join("exit_status"), "1")?; // failed

        let program = crate_program(vec![CrateStatement::Run(RunStep {
            command: "echo".to_owned(),
            args: vec![],
        })]);
        let resolved = resolved_with_one_crate(dir);
        let next = find_next_statement(&program, &resolved, &state_base);
        assert!(
            next.is_some(),
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
        let next = find_next_statement(&program, &resolved, &state_base);
        assert!(next.is_some());
        let next = next.ok_or("expected Some")?;
        assert_eq!(
            next.cursor,
            ProgramCursor::new()
                .with(CursorSegment::WorkspaceIteration(0))
                .with(CursorSegment::Statement(0))
        );
        Ok(())
    }
}
