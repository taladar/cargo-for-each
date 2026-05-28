//! Runtime evaluation of program conditions against a target.
//!
//! Each context (workspace, crate) has its own condition type; this module
//! provides an `evaluate_*` function for each, which is called during task
//! execution to decide which branches to take in `if` blocks.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use git2::Repository;

use crate::error::Error;
use crate::program::ast::common::CommonCondition;
use crate::program::ast::crate_ctx::{CrateCondition, CrateTypeFilter, TargetKindFilter};
use crate::program::ast::workspace_ctx::WorkspaceCondition;
use crate::targets::{CrateType, TargetKind};

/// Joins `rel_or_abs` to `base` and lexically normalizes `.`/`..` segments.
///
/// An absolute `rel_or_abs` overrides `base` (per `Path::join` semantics). The
/// result is *not* resolved against the filesystem, so symlinks are not
/// followed — callers that need anti-escape guarantees must canonicalize
/// independently.
fn lexically_resolve(base: &Path, rel_or_abs: &str) -> PathBuf {
    let joined = base.join(rel_or_abs);
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

/// Returns the workspace manifest directory for a target's `manifest_dir`.
///
/// `manifest_dir` is the dir of either a registered workspace (in which case
/// the workspace dir is itself) or a registered crate (in which case the
/// workspace dir is the crate's enclosing workspace).
fn workspace_boundary_for<'a>(manifest_dir: &Path, config: &'a crate::Config) -> Option<&'a Path> {
    if let Some(w) = config
        .workspaces
        .iter()
        .find(|w| w.manifest_dir == manifest_dir)
    {
        return Some(w.manifest_dir.as_path());
    }
    config
        .crates
        .iter()
        .find(|c| c.manifest_dir == manifest_dir)
        .map(|c| c.workspace_manifest_dir.as_path())
}

/// Looks up the actual value of `key` in the git config reachable from `manifest_dir`.
///
/// Returns `None` if the directory is not in a git repository or the key is absent.
fn lookup_git_config_value(key: &str, manifest_dir: &std::path::Path) -> Option<String> {
    let repo = Repository::discover(manifest_dir).ok()?;
    let config = repo.config().ok()?;
    config.get_string(key).ok()
}

/// Returns a human-readable string describing any runtime values embedded in a
/// [`CommonCondition`] that would not be obvious from the condition text alone.
///
/// Currently this surfaces the actual git config value for [`CommonCondition::GitConfigEquals`].
/// Returns `None` if there is nothing interesting to add.
#[must_use]
pub fn common_condition_runtime_detail(
    cond: &CommonCondition,
    manifest_dir: &std::path::Path,
) -> Option<String> {
    match cond {
        CommonCondition::GitConfigEquals { key, value: _ } => {
            let actual = lookup_git_config_value(key, manifest_dir)
                .map_or_else(|| "(not set)".to_owned(), |v| format!("{v:?}"));
            Some(format!("actual git_config.{key} = {actual}"))
        }
        CommonCondition::Not(inner) => common_condition_runtime_detail(inner, manifest_dir),
        CommonCondition::And(conditions) => join_runtime_details(conditions, " && ", |c| {
            common_condition_runtime_detail(c, manifest_dir)
        }),
        CommonCondition::Or(conditions) => join_runtime_details(conditions, " || ", |c| {
            common_condition_runtime_detail(c, manifest_dir)
        }),
        _ => None,
    }
}

/// Joins the runtime-detail strings of `items` with `separator`, filtering
/// out items that produce no detail. Returns `None` if no item produces a
/// detail string.
fn join_runtime_details<T, F: Fn(&T) -> Option<String>>(
    items: &[T],
    separator: &str,
    detail: F,
) -> Option<String> {
    let details: Vec<_> = items.iter().filter_map(detail).collect();
    if details.is_empty() {
        None
    } else {
        Some(details.join(separator))
    }
}

/// Returns runtime detail strings for a [`WorkspaceCondition`].
#[must_use]
pub fn workspace_condition_runtime_detail(
    cond: &WorkspaceCondition,
    manifest_dir: &std::path::Path,
) -> Option<String> {
    match cond {
        WorkspaceCondition::Common(inner) => common_condition_runtime_detail(inner, manifest_dir),
        WorkspaceCondition::Not(inner) => workspace_condition_runtime_detail(inner, manifest_dir),
        WorkspaceCondition::And(conditions) => join_runtime_details(conditions, " && ", |c| {
            workspace_condition_runtime_detail(c, manifest_dir)
        }),
        WorkspaceCondition::Or(conditions) => join_runtime_details(conditions, " || ", |c| {
            workspace_condition_runtime_detail(c, manifest_dir)
        }),
        _ => None,
    }
}

/// Returns runtime detail strings for a [`CrateCondition`].
#[must_use]
pub fn crate_condition_runtime_detail(
    cond: &CrateCondition,
    manifest_dir: &std::path::Path,
) -> Option<String> {
    match cond {
        CrateCondition::Common(inner) => common_condition_runtime_detail(inner, manifest_dir),
        CrateCondition::Not(inner) => crate_condition_runtime_detail(inner, manifest_dir),
        CrateCondition::And(conditions) => join_runtime_details(conditions, " && ", |c| {
            crate_condition_runtime_detail(c, manifest_dir)
        }),
        CrateCondition::Or(conditions) => join_runtime_details(conditions, " || ", |c| {
            crate_condition_runtime_detail(c, manifest_dir)
        }),
        _ => None,
    }
}

/// Evaluates a [`CommonCondition`] for the given target directory.
///
/// Common conditions are available in all execution contexts.
///
/// # Errors
///
/// Returns an error if a `RunCommand` condition's command cannot be found or
/// launched, or if `AskUser` I/O fails.
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the evaluate module"
)]
pub fn evaluate_common_condition(
    cond: &CommonCondition,
    manifest_dir: &Path,
    environment: &crate::Environment,
    config: &crate::Config,
    extra_env: &[(String, String)],
) -> Result<bool, Error> {
    match cond {
        CommonCondition::AskUser(question) => eval_ask_user(question),
        CommonCondition::RunCommand { command, args } => {
            eval_run_command(command, args, manifest_dir, environment, extra_env)
        }
        CommonCondition::FileExists(filename) => eval_file_exists(filename, manifest_dir, config),
        CommonCondition::WorkingDirectoryClean => {
            eval_working_directory_clean(manifest_dir, environment)
        }
        CommonCondition::GitConfigEquals { key, value } => {
            Ok(eval_git_config_equals(key, value, manifest_dir))
        }
        CommonCondition::Not(inner) => Ok(!evaluate_common_condition(
            inner,
            manifest_dir,
            environment,
            config,
            extra_env,
        )?),
        CommonCondition::And(conditions) => {
            for c in conditions {
                if !evaluate_common_condition(c, manifest_dir, environment, config, extra_env)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        CommonCondition::Or(conditions) => {
            for c in conditions {
                if evaluate_common_condition(c, manifest_dir, environment, config, extra_env)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Prompts the user with a yes/no `question` and returns `true` for `y`/`yes`.
#[expect(clippy::print_stdout, reason = "AskUser is part of the interactive UI")]
fn eval_ask_user(question: &str) -> Result<bool, Error> {
    print!("{question} (y/N) ");
    std::io::stdout().flush().map_err(Error::IoError)?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(Error::IoError)?;
    let answer = answer.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Runs `command` in `manifest_dir` and returns `true` if it exits with code 0.
///
/// # Errors
///
/// Returns [`Error::CommandNotFound`] if the command is not on `PATH`, or
/// [`Error::ConditionCommandKilledBySignal`] if the process was terminated by a
/// signal (so a signal kill is never silently read as a clean "exit 1").
fn eval_run_command(
    command: &str,
    args: &[String],
    manifest_dir: &Path,
    environment: &crate::Environment,
    extra_env: &[(String, String)],
) -> Result<bool, Error> {
    if !crate::utils::command_is_executable(command, environment) {
        return Err(Error::CommandNotFound(command.to_owned()));
    }
    let mut cmd = std::process::Command::new(command);
    cmd.args(args).current_dir(manifest_dir);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?;
    match output.status.code() {
        Some(code) => Ok(code == 0),
        None => Err(Error::ConditionCommandKilledBySignal(
            command.to_owned(),
            manifest_dir.to_path_buf(),
        )),
    }
}

/// Returns `true` if `filename` (resolved relative to `manifest_dir`) exists.
///
/// # Errors
///
/// Returns an error if the target is not registered, or if the resolved path
/// escapes the enclosing workspace boundary.
fn eval_file_exists(
    filename: &str,
    manifest_dir: &Path,
    config: &crate::Config,
) -> Result<bool, Error> {
    let workspace_dir = workspace_boundary_for(manifest_dir, config)
        .ok_or_else(|| Error::FileExistsTargetNotRegistered(manifest_dir.to_path_buf()))?;
    let resolved = lexically_resolve(manifest_dir, filename);
    if !resolved.starts_with(workspace_dir) {
        return Err(Error::FileExistsPathOutsideWorkspace(filename.to_owned()));
    }
    Ok(resolved.exists())
}

/// Returns `true` if `git status --porcelain` is empty for `manifest_dir`.
///
/// # Errors
///
/// Returns [`Error::CommandNotFound`] if `git` is missing, or
/// [`Error::CommandExecutionFailed`] if the invocation fails.
fn eval_working_directory_clean(
    manifest_dir: &Path,
    environment: &crate::Environment,
) -> Result<bool, Error> {
    if !crate::utils::command_is_executable("git", environment) {
        return Err(Error::CommandNotFound("git".to_owned()));
    }
    let mut cmd = std::process::Command::new("git");
    // Do not use the util function here since we need to capture output to
    // evaluate whether it is empty.
    cmd.args(["status", "--porcelain"])
        .current_dir(manifest_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = cmd.output().map_err(|e| {
        Error::CommandExecutionFailed(
            "git status -porcelain".to_string(),
            manifest_dir.to_path_buf(),
            e,
        )
    })?;
    Ok(output.stdout.is_empty())
}

/// Returns `true` if git config key `key` equals `value` in `manifest_dir`'s
/// repository. A missing repository or key is treated as "not equal".
fn eval_git_config_equals(key: &str, value: &str, manifest_dir: &Path) -> bool {
    lookup_git_config_value(key, manifest_dir).is_some_and(|actual| actual == value)
}

/// Evaluates a [`WorkspaceCondition`] for the given workspace target.
///
/// # Errors
///
/// Propagates errors from [`evaluate_common_condition`].
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the evaluate module"
)]
pub fn evaluate_workspace_condition(
    cond: &WorkspaceCondition,
    manifest_dir: &Path,
    environment: &crate::Environment,
    config: &crate::Config,
    extra_env: &[(String, String)],
) -> Result<bool, Error> {
    match cond {
        WorkspaceCondition::Common(inner) => {
            evaluate_common_condition(inner, manifest_dir, environment, config, extra_env)
        }
        WorkspaceCondition::Standalone => Ok(config
            .workspaces
            .iter()
            .any(|w| w.manifest_dir == manifest_dir && w.is_standalone)),
        WorkspaceCondition::HasMembers => Ok(config
            .workspaces
            .iter()
            .any(|w| w.manifest_dir == manifest_dir && !w.is_standalone)),
        WorkspaceCondition::Not(inner) => Ok(!evaluate_workspace_condition(
            inner,
            manifest_dir,
            environment,
            config,
            extra_env,
        )?),
        WorkspaceCondition::And(conditions) => {
            for c in conditions {
                if !evaluate_workspace_condition(c, manifest_dir, environment, config, extra_env)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        WorkspaceCondition::Or(conditions) => {
            for c in conditions {
                if evaluate_workspace_condition(c, manifest_dir, environment, config, extra_env)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Evaluates a [`CrateCondition`] for the given crate target.
///
/// # Errors
///
/// Propagates errors from [`evaluate_common_condition`].
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the evaluate module"
)]
pub fn evaluate_crate_condition(
    cond: &CrateCondition,
    manifest_dir: &Path,
    environment: &crate::Environment,
    config: &crate::Config,
    extra_env: &[(String, String)],
) -> Result<bool, Error> {
    match cond {
        CrateCondition::Common(inner) => {
            evaluate_common_condition(inner, manifest_dir, environment, config, extra_env)
        }
        CrateCondition::CrateType(filter) => Ok(crate_type_matches(config, manifest_dir, *filter)),
        CrateCondition::TargetKind(filter) => {
            Ok(target_kind_matches(config, manifest_dir, *filter))
        }
        CrateCondition::Standalone => Ok(crate_is_standalone(config, manifest_dir)),
        CrateCondition::Not(inner) => Ok(!evaluate_crate_condition(
            inner,
            manifest_dir,
            environment,
            config,
            extra_env,
        )?),
        CrateCondition::And(conditions) => {
            for c in conditions {
                if !evaluate_crate_condition(c, manifest_dir, environment, config, extra_env)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        CrateCondition::Or(conditions) => {
            for c in conditions {
                if evaluate_crate_condition(c, manifest_dir, environment, config, extra_env)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Returns `true` if the crate registered at `manifest_dir` produces the
/// compile-time output kind named by `filter`.
fn crate_type_matches(
    config: &crate::Config,
    manifest_dir: &Path,
    filter: CrateTypeFilter,
) -> bool {
    let required = CrateType::from(filter);
    config
        .crates
        .iter()
        .any(|c| c.manifest_dir == manifest_dir && c.crate_types.contains(&required))
}

/// Returns `true` if the crate registered at `manifest_dir` has the auxiliary
/// cargo target kind named by `filter`.
fn target_kind_matches(
    config: &crate::Config,
    manifest_dir: &Path,
    filter: TargetKindFilter,
) -> bool {
    let required = TargetKind::from(filter);
    config
        .crates
        .iter()
        .any(|c| c.manifest_dir == manifest_dir && c.target_kinds.contains(&required))
}

/// Returns `true` if the crate at `manifest_dir` lives in a standalone
/// (single-crate) workspace.
fn crate_is_standalone(config: &crate::Config, manifest_dir: &Path) -> bool {
    config
        .crates
        .iter()
        .find(|c| c.manifest_dir == manifest_dir)
        .is_some_and(|c| {
            config
                .workspaces
                .iter()
                .any(|w| w.manifest_dir == c.workspace_manifest_dir && w.is_standalone)
        })
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helper functions use panic! on unexpected failures"
    )]

    use std::collections::BTreeSet;

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;
    use crate::program::ast::common::AtLeastTwo;
    use crate::{Crate, Workspace};

    fn mock_env(temp: &tempfile::TempDir) -> crate::Environment {
        crate::Environment::mock(temp).unwrap_or_else(|e| panic!("mock env: {e}"))
    }

    fn config_with_bin_crate(dir: &Path) -> crate::Config {
        crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: true,
            }],
            crates: vec![Crate {
                manifest_dir: dir.to_path_buf(),
                workspace_manifest_dir: dir.to_path_buf(),
                crate_types: BTreeSet::from([CrateType::Bin]),
                target_kinds: BTreeSet::new(),
            }],
        }
    }

    fn empty_config() -> crate::Config {
        crate::Config {
            workspaces: vec![],
            crates: vec![],
        }
    }

    // ── CommonCondition ──────────────────────────────────────────────────────

    #[test]
    fn common_file_exists_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        fs_err::write(dir.join("hello.txt"), "").unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = config_with_bin_crate(dir);
        let result = evaluate_common_condition(
            &CommonCondition::FileExists("hello.txt".to_owned()),
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn common_file_exists_false() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = config_with_bin_crate(temp.path());
        let result = evaluate_common_condition(
            &CommonCondition::FileExists("missing.txt".to_owned()),
            temp.path(),
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn common_file_exists_rejects_absolute_path_outside_workspace() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = config_with_bin_crate(temp.path());
        let result = evaluate_common_condition(
            &CommonCondition::FileExists("/etc/passwd".to_owned()),
            temp.path(),
            &env,
            &config,
            &[],
        );
        match result {
            Err(Error::FileExistsPathOutsideWorkspace(p)) => assert_eq!(p, "/etc/passwd"),
            other => panic!("expected FileExistsPathOutsideWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn common_file_exists_rejects_parent_dir_traversal() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = config_with_bin_crate(temp.path());
        let result = evaluate_common_condition(
            &CommonCondition::FileExists("../../../etc/passwd".to_owned()),
            temp.path(),
            &env,
            &config,
            &[],
        );
        match result {
            Err(Error::FileExistsPathOutsideWorkspace(p)) => assert_eq!(p, "../../../etc/passwd"),
            other => panic!("expected FileExistsPathOutsideWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn common_file_exists_allows_crate_traversal_within_workspace() {
        // A crate at <ws>/sub may reference files at <ws>/target/... via "..".
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let ws = temp.path();
        let crate_dir = ws.join("sub");
        fs_err::create_dir_all(&crate_dir).unwrap_or_else(|e| panic!("{e}"));
        let target = ws.join("target");
        fs_err::create_dir_all(&target).unwrap_or_else(|e| panic!("{e}"));
        fs_err::write(target.join("artifact"), "").unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: ws.to_path_buf(),
                is_standalone: false,
            }],
            crates: vec![Crate {
                manifest_dir: crate_dir.clone(),
                workspace_manifest_dir: ws.to_path_buf(),
                crate_types: BTreeSet::from([CrateType::Bin]),
                target_kinds: BTreeSet::new(),
            }],
        };
        let result = evaluate_common_condition(
            &CommonCondition::FileExists("../target/artifact".to_owned()),
            &crate_dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn common_not() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = config_with_bin_crate(temp.path());
        let result = evaluate_common_condition(
            &CommonCondition::Not(Box::new(CommonCondition::FileExists("x".to_owned()))),
            temp.path(),
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn common_and_short_circuits() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = config_with_bin_crate(temp.path());
        let result = evaluate_common_condition(
            &CommonCondition::And(AtLeastTwo::from_pair(
                CommonCondition::FileExists("missing".to_owned()),
                CommonCondition::FileExists("also_missing".to_owned()),
            )),
            temp.path(),
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn common_or_short_circuits() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        fs_err::write(dir.join("exists.txt"), "").unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = config_with_bin_crate(dir);
        let result = evaluate_common_condition(
            &CommonCondition::Or(AtLeastTwo::from_pair(
                CommonCondition::FileExists("exists.txt".to_owned()),
                CommonCondition::FileExists("missing.txt".to_owned()),
            )),
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn common_git_config_equals_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let repo = git2::Repository::init(dir).unwrap_or_else(|e| panic!("{e}"));
        {
            // use a block to ensure `config` is dropped and written to disk
            let mut config = repo.config().unwrap_or_else(|e| panic!("{e}"));
            config
                .set_str("user.name", "Test User")
                .unwrap_or_else(|e| panic!("{e}"));
        }
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::GitConfigEquals {
                key: "user.name".to_owned(),
                value: "Test User".to_owned(),
            },
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn common_git_config_equals_false_mismatch() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let repo = git2::Repository::init(dir).unwrap_or_else(|e| panic!("{e}"));
        {
            let mut config = repo.config().unwrap_or_else(|e| panic!("{e}"));
            config
                .set_str("user.name", "Test User")
                .unwrap_or_else(|e| panic!("{e}"));
        }
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::GitConfigEquals {
                key: "user.name".to_owned(),
                value: "Another User".to_owned(),
            },
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn common_git_config_equals_false_no_repo() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        // No git repo initialized
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::GitConfigEquals {
                key: "user.name".to_owned(),
                value: "Test User".to_owned(),
            },
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn common_git_config_equals_false_key_not_found() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let _repo = git2::Repository::init(dir).unwrap_or_else(|e| panic!("{e}"));
        // Don't set user.name
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::GitConfigEquals {
                key: "user.name".to_owned(),
                value: "Test User".to_owned(),
            },
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    // ── WorkspaceCondition ───────────────────────────────────────────────────

    #[test]
    fn workspace_standalone_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: true,
            }],
            crates: vec![],
        };
        let result =
            evaluate_workspace_condition(&WorkspaceCondition::Standalone, dir, &env, &config, &[]);
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn workspace_has_members_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: false,
            }],
            crates: vec![],
        };
        let result =
            evaluate_workspace_condition(&WorkspaceCondition::HasMembers, dir, &env, &config, &[]);
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    // ── CrateCondition ───────────────────────────────────────────────────────

    #[test]
    fn crate_type_bin_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = config_with_bin_crate(dir);
        let result = evaluate_crate_condition(
            &CrateCondition::CrateType(CrateTypeFilter::Bin),
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn crate_type_lib_false_when_bin() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = config_with_bin_crate(dir);
        let result = evaluate_crate_condition(
            &CrateCondition::CrateType(CrateTypeFilter::Lib),
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn crate_standalone_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = config_with_bin_crate(dir);
        let result = evaluate_crate_condition(&CrateCondition::Standalone, dir, &env, &config, &[]);
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn crate_target_kind_test_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: true,
            }],
            crates: vec![Crate {
                manifest_dir: dir.to_path_buf(),
                workspace_manifest_dir: dir.to_path_buf(),
                crate_types: BTreeSet::from([CrateType::Bin]),
                target_kinds: BTreeSet::from([TargetKind::Test]),
            }],
        };
        let result = evaluate_crate_condition(
            &CrateCondition::TargetKind(TargetKindFilter::Test),
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn crate_target_kind_bench_false_when_only_test() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: true,
            }],
            crates: vec![Crate {
                manifest_dir: dir.to_path_buf(),
                workspace_manifest_dir: dir.to_path_buf(),
                crate_types: BTreeSet::from([CrateType::Bin]),
                target_kinds: BTreeSet::from([TargetKind::Test]),
            }],
        };
        let result = evaluate_crate_condition(
            &CrateCondition::TargetKind(TargetKindFilter::Bench),
            dir,
            &env,
            &config,
            &[],
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    // ── runtime detail ───────────────────────────────────────────────────────

    #[test]
    fn common_runtime_detail_git_config_not_set() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        // No git repo -> the actual value is reported as "(not set)".
        let detail = common_condition_runtime_detail(
            &CommonCondition::GitConfigEquals {
                key: "user.name".to_owned(),
                value: "whatever".to_owned(),
            },
            temp.path(),
        );
        assert_eq!(
            detail.as_deref(),
            Some("actual git_config.user.name = (not set)")
        );
    }

    #[test]
    fn common_runtime_detail_git_config_set() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let repo = git2::Repository::init(dir).unwrap_or_else(|e| panic!("{e}"));
        {
            let mut cfg = repo.config().unwrap_or_else(|e| panic!("{e}"));
            cfg.set_str("user.name", "Alice")
                .unwrap_or_else(|e| panic!("{e}"));
        }
        let detail = common_condition_runtime_detail(
            &CommonCondition::GitConfigEquals {
                key: "user.name".to_owned(),
                value: "whatever".to_owned(),
            },
            dir,
        );
        assert_eq!(
            detail.as_deref(),
            Some(r#"actual git_config.user.name = "Alice""#)
        );
    }

    #[test]
    fn common_runtime_detail_none_for_plain_conditions() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            common_condition_runtime_detail(&CommonCondition::WorkingDirectoryClean, temp.path()),
            None,
        );
        assert_eq!(
            common_condition_runtime_detail(
                &CommonCondition::FileExists("x".to_owned()),
                temp.path()
            ),
            None,
        );
    }

    #[test]
    fn common_runtime_detail_recurses_through_not_and_or() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let git = |key: &str| CommonCondition::GitConfigEquals {
            key: key.to_owned(),
            value: "x".to_owned(),
        };

        // Not recurses into its inner condition.
        assert_eq!(
            common_condition_runtime_detail(&CommonCondition::Not(Box::new(git("a.b"))), dir)
                .as_deref(),
            Some("actual git_config.a.b = (not set)"),
        );

        // And/Or filter out detail-less operands and join the rest.
        let mixed = CommonCondition::And(AtLeastTwo::from_pair(
            git("a.b"),
            CommonCondition::WorkingDirectoryClean,
        ));
        assert_eq!(
            common_condition_runtime_detail(&mixed, dir).as_deref(),
            Some("actual git_config.a.b = (not set)"),
        );

        let both = CommonCondition::Or(AtLeastTwo::from_pair(git("a.b"), git("c.d")));
        assert_eq!(
            common_condition_runtime_detail(&both, dir).as_deref(),
            Some("actual git_config.a.b = (not set) || actual git_config.c.d = (not set)"),
        );

        // No interesting operand -> None.
        let plain = CommonCondition::And(AtLeastTwo::from_pair(
            CommonCondition::WorkingDirectoryClean,
            CommonCondition::FileExists("x".to_owned()),
        ));
        assert_eq!(common_condition_runtime_detail(&plain, dir), None);
    }

    #[test]
    fn workspace_runtime_detail_delegates_to_common() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let git = WorkspaceCondition::Common(CommonCondition::GitConfigEquals {
            key: "user.email".to_owned(),
            value: "x".to_owned(),
        });
        assert_eq!(
            workspace_condition_runtime_detail(&git, dir).as_deref(),
            Some("actual git_config.user.email = (not set)"),
        );
        assert_eq!(
            workspace_condition_runtime_detail(&WorkspaceCondition::Standalone, dir),
            None,
        );
        assert_eq!(
            workspace_condition_runtime_detail(
                &WorkspaceCondition::Not(Box::new(git.clone())),
                dir,
            )
            .as_deref(),
            Some("actual git_config.user.email = (not set)"),
        );
        let or = WorkspaceCondition::Or(AtLeastTwo::from_pair(git, WorkspaceCondition::HasMembers));
        assert_eq!(
            workspace_condition_runtime_detail(&or, dir).as_deref(),
            Some("actual git_config.user.email = (not set)"),
        );
    }

    #[test]
    fn crate_runtime_detail_delegates_to_common() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let git = CrateCondition::Common(CommonCondition::GitConfigEquals {
            key: "core.editor".to_owned(),
            value: "x".to_owned(),
        });
        assert_eq!(
            crate_condition_runtime_detail(&git, dir).as_deref(),
            Some("actual git_config.core.editor = (not set)"),
        );
        assert_eq!(
            crate_condition_runtime_detail(&CrateCondition::Standalone, dir),
            None,
        );
        assert_eq!(
            crate_condition_runtime_detail(&CrateCondition::Not(Box::new(git.clone())), dir)
                .as_deref(),
            Some("actual git_config.core.editor = (not set)"),
        );
        let and = CrateCondition::And(AtLeastTwo::from_pair(git, CrateCondition::Standalone));
        assert_eq!(
            crate_condition_runtime_detail(&and, dir).as_deref(),
            Some("actual git_config.core.editor = (not set)"),
        );
    }

    // ── filter -> target-enum conversions ─────────────────────────────────────

    #[test]
    fn crate_type_from_filter_maps_every_variant() {
        let cases = [
            (CrateTypeFilter::Bin, CrateType::Bin),
            (CrateTypeFilter::Lib, CrateType::Lib),
            (CrateTypeFilter::ProcMacro, CrateType::ProcMacro),
            (CrateTypeFilter::CDyLib, CrateType::CDyLib),
            (CrateTypeFilter::DyLib, CrateType::DyLib),
            (CrateTypeFilter::RLib, CrateType::RLib),
            (CrateTypeFilter::StaticLib, CrateType::StaticLib),
        ];
        for (filter, expected) in cases {
            assert_eq!(CrateType::from(filter), expected);
        }
    }

    #[test]
    fn target_kind_from_filter_maps_every_variant() {
        let cases = [
            (TargetKindFilter::Bench, TargetKind::Bench),
            (TargetKindFilter::Test, TargetKind::Test),
            (TargetKindFilter::Example, TargetKind::Example),
            (TargetKindFilter::CustomBuild, TargetKind::CustomBuild),
        ];
        for (filter, expected) in cases {
            assert_eq!(TargetKind::from(filter), expected);
        }
    }

    // ── workspace condition combinators / leaves ──────────────────────────────

    #[test]
    fn workspace_standalone_false_when_not_standalone() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: false,
            }],
            crates: vec![],
        };
        assert_eq!(
            evaluate_workspace_condition(&WorkspaceCondition::Standalone, dir, &env, &config, &[])
                .unwrap_or_else(|e| panic!("{e}")),
            false,
        );
        assert_eq!(
            evaluate_workspace_condition(&WorkspaceCondition::HasMembers, dir, &env, &config, &[])
                .unwrap_or_else(|e| panic!("{e}")),
            true,
        );
    }

    #[test]
    fn workspace_not_and_or_combinators() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        // Empty repo so `git status --porcelain` is unambiguously clean.
        git2::Repository::init(dir).unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = crate::Config {
            workspaces: vec![Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: true,
            }],
            crates: vec![],
        };
        let eval = |c: &WorkspaceCondition| {
            evaluate_workspace_condition(c, dir, &env, &config, &[])
                .unwrap_or_else(|e| panic!("{e}"))
        };

        // Not.
        assert_eq!(
            eval(&WorkspaceCondition::Not(Box::new(
                WorkspaceCondition::Standalone
            ))),
            false,
        );
        // And short-circuits on the first false operand (HasMembers is false here).
        assert_eq!(
            eval(&WorkspaceCondition::And(AtLeastTwo::from_pair(
                WorkspaceCondition::HasMembers,
                WorkspaceCondition::Standalone,
            ))),
            false,
        );
        // Or short-circuits true on Standalone.
        assert_eq!(
            eval(&WorkspaceCondition::Or(AtLeastTwo::from_pair(
                WorkspaceCondition::HasMembers,
                WorkspaceCondition::Standalone,
            ))),
            true,
        );
        // Common delegates to the common evaluator.
        assert_eq!(
            eval(&WorkspaceCondition::Common(
                CommonCondition::WorkingDirectoryClean
            )),
            // No git repo in the temp dir -> `git status` is empty -> clean.
            true,
        );
    }

    // ── run_command leaf ──────────────────────────────────────────────────────

    #[test]
    fn run_command_true_false_and_missing() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = config_with_bin_crate(dir);
        let eval = |command: &str| {
            evaluate_common_condition(
                &CommonCondition::RunCommand {
                    command: command.to_owned(),
                    args: vec![],
                },
                dir,
                &env,
                &config,
                &[],
            )
        };
        // `true` exits 0 -> condition true; `false` exits 1 -> condition false.
        assert_eq!(eval("true").unwrap_or_else(|e| panic!("{e}")), true);
        assert_eq!(eval("false").unwrap_or_else(|e| panic!("{e}")), false);
        // A command that is not on PATH is a hard error, not a silent `false`.
        match eval("cfe_definitely_missing_command_xyz") {
            Err(Error::CommandNotFound(c)) => assert_eq!(c, "cfe_definitely_missing_command_xyz"),
            other => panic!("expected CommandNotFound, got {other:?}"),
        }
    }

    // ── crate condition combinators / arms ────────────────────────────────────

    #[test]
    fn crate_condition_arms_and_combinators() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        fs_err::write(dir.join("present"), "").unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        // crate_types = {Bin}, enclosing workspace is standalone, target_kinds empty.
        let config = config_with_bin_crate(dir);
        let eval = |c: &CrateCondition| {
            evaluate_crate_condition(c, dir, &env, &config, &[]).unwrap_or_else(|e| panic!("{e}"))
        };

        // Common delegation.
        assert_eq!(
            eval(&CrateCondition::Common(CommonCondition::FileExists(
                "present".to_owned()
            ))),
            true,
        );
        // CrateType arm (Bin present, Lib absent).
        assert_eq!(eval(&CrateCondition::CrateType(CrateTypeFilter::Bin)), true);
        assert_eq!(
            eval(&CrateCondition::CrateType(CrateTypeFilter::Lib)),
            false
        );
        // TargetKind arm (none registered).
        assert_eq!(
            eval(&CrateCondition::TargetKind(TargetKindFilter::Test)),
            false
        );
        // Standalone arm.
        assert_eq!(eval(&CrateCondition::Standalone), true);
        // Not.
        assert_eq!(
            eval(&CrateCondition::Not(Box::new(CrateCondition::CrateType(
                CrateTypeFilter::Lib
            )))),
            true,
        );
        // And: true when both hold, short-circuits false on the first.
        assert_eq!(
            eval(&CrateCondition::And(AtLeastTwo::from_pair(
                CrateCondition::CrateType(CrateTypeFilter::Bin),
                CrateCondition::Standalone,
            ))),
            true,
        );
        assert_eq!(
            eval(&CrateCondition::And(AtLeastTwo::from_pair(
                CrateCondition::CrateType(CrateTypeFilter::Lib),
                CrateCondition::Standalone,
            ))),
            false,
        );
        // Or: short-circuits true on the second.
        assert_eq!(
            eval(&CrateCondition::Or(AtLeastTwo::from_pair(
                CrateCondition::CrateType(CrateTypeFilter::Lib),
                CrateCondition::CrateType(CrateTypeFilter::Bin),
            ))),
            true,
        );
    }
}
