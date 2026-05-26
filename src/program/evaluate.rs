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
use crate::program::ast::crate_ctx::{CrateCondition, CrateTypeFilter};
use crate::program::ast::workspace_ctx::WorkspaceCondition;
use crate::targets::CrateType;

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
        CommonCondition::And(conditions) | CommonCondition::Or(conditions) => {
            let details: Vec<_> = conditions
                .iter()
                .filter_map(|c| common_condition_runtime_detail(c, manifest_dir))
                .collect();
            if details.is_empty() {
                None
            } else {
                Some(details.join(", "))
            }
        }
        _ => None,
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
        WorkspaceCondition::And(conditions) | WorkspaceCondition::Or(conditions) => {
            let details: Vec<_> = conditions
                .iter()
                .filter_map(|c| workspace_condition_runtime_detail(c, manifest_dir))
                .collect();
            if details.is_empty() {
                None
            } else {
                Some(details.join(", "))
            }
        }
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
        CrateCondition::And(conditions) | CrateCondition::Or(conditions) => {
            let details: Vec<_> = conditions
                .iter()
                .filter_map(|c| crate_condition_runtime_detail(c, manifest_dir))
                .collect();
            if details.is_empty() {
                None
            } else {
                Some(details.join(", "))
            }
        }
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
#[expect(clippy::print_stdout, reason = "AskUser is part of the interactive UI")]
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
        CommonCondition::AskUser(question) => {
            print!("{question} (y/N) ");
            std::io::stdout().flush().map_err(Error::IoError)?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(Error::IoError)?;
            let answer = answer.trim().to_lowercase();
            Ok(answer == "y" || answer == "yes")
        }
        CommonCondition::RunCommand { command, args } => {
            if !crate::utils::command_is_executable(command, environment) {
                return Err(Error::CommandNotFound(command.clone()));
            }
            let mut cmd = std::process::Command::new(command);
            cmd.args(args).current_dir(manifest_dir);
            for (k, v) in extra_env {
                cmd.env(k, v);
            }
            let output = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?;
            // A signal-killed process (status.code() == None on Unix) used to
            // be reported as `false`, indistinguishable from a clean "exit 1"
            // — making the surrounding `if` branch silently go the wrong way.
            // Surface signal kills as an error instead.
            match output.status.code() {
                Some(code) => Ok(code == 0),
                None => Err(Error::ConditionCommandKilledBySignal(
                    command.clone(),
                    manifest_dir.to_path_buf(),
                )),
            }
        }
        CommonCondition::FileExists(filename) => {
            let workspace_dir = workspace_boundary_for(manifest_dir, config)
                .ok_or_else(|| Error::FileExistsTargetNotRegistered(manifest_dir.to_path_buf()))?;
            let resolved = lexically_resolve(manifest_dir, filename);
            if !resolved.starts_with(workspace_dir) {
                return Err(Error::FileExistsPathOutsideWorkspace(filename.clone()));
            }
            Ok(resolved.exists())
        }
        CommonCondition::WorkingDirectoryClean => {
            if !crate::utils::command_is_executable("git", environment) {
                return Err(Error::CommandNotFound("git".to_owned()));
            }
            let mut cmd = std::process::Command::new("git");
            // do not use the util function here since we need to capture output to evaluate
            // if it is empty
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
        CommonCondition::GitConfigEquals { key, value } => {
            let repo = Repository::discover(manifest_dir);
            match repo {
                Ok(repo) => {
                    let config = repo.config().map_err(Error::GitError)?;
                    let git_value = config.get_string(key);
                    match git_value {
                        Ok(git_value) => Ok(git_value == *value),
                        Err(_) => Ok(false), // Key not found or other error, treat as not equal
                    }
                }
                Err(_) => Ok(false), // Not a git repository, treat as not equal
            }
        }
    }
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
        CrateCondition::CrateType(filter) => {
            let required = match filter {
                CrateTypeFilter::Bin => CrateType::Bin,
                CrateTypeFilter::Lib => CrateType::Lib,
                CrateTypeFilter::ProcMacro => CrateType::ProcMacro,
                CrateTypeFilter::CDyLib => CrateType::CDyLib,
                CrateTypeFilter::DyLib => CrateType::DyLib,
                CrateTypeFilter::RLib => CrateType::RLib,
                CrateTypeFilter::StaticLib => CrateType::StaticLib,
                CrateTypeFilter::Bench => CrateType::Bench,
                CrateTypeFilter::Test => CrateType::Test,
                CrateTypeFilter::Example => CrateType::Example,
                CrateTypeFilter::CustomBuild => CrateType::CustomBuild,
            };
            Ok(config
                .crates
                .iter()
                .any(|c| c.manifest_dir == manifest_dir && c.types.contains(&required)))
        }
        CrateCondition::Standalone => {
            // A crate is "standalone" if its workspace is a standalone workspace.
            let ws_dir = config
                .crates
                .iter()
                .find(|c| c.manifest_dir == manifest_dir)
                .map(|c| c.workspace_manifest_dir.clone());
            match ws_dir {
                None => Ok(false),
                Some(ws) => Ok(config
                    .workspaces
                    .iter()
                    .any(|w| w.manifest_dir == ws && w.is_standalone)),
            }
        }
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
                types: BTreeSet::from([CrateType::Bin]),
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
                types: BTreeSet::from([CrateType::Bin]),
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
            &CommonCondition::And(vec![
                CommonCondition::FileExists("missing".to_owned()),
                CommonCondition::FileExists("also_missing".to_owned()),
            ]),
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
            &CommonCondition::Or(vec![
                CommonCondition::FileExists("exists.txt".to_owned()),
                CommonCondition::FileExists("missing.txt".to_owned()),
            ]),
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
}
