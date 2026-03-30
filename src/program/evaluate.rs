//! Runtime evaluation of program conditions against a target.
//!
//! Each context (workspace, crate) has its own condition type; this module
//! provides an `evaluate_*` function for each, which is called during task
//! execution to decide which branches to take in `if` blocks.

use std::io::Write as _;
use std::path::Path;

use crate::error::Error;
use crate::program::ast::common::CommonCondition;
use crate::program::ast::crate_ctx::{CrateCondition, CrateTypeFilter};
use crate::program::ast::workspace_ctx::WorkspaceCondition;
use crate::targets::CrateType;

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
#[expect(
    clippy::only_used_in_recursion,
    reason = "config is passed for extensibility; leaf arms may need it in future"
)]
pub fn evaluate_common_condition(
    cond: &CommonCondition,
    manifest_dir: &Path,
    environment: &crate::Environment,
    config: &crate::Config,
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
            let output = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?;
            Ok(output.status.success())
        }
        CommonCondition::FileExists(filename) => Ok(manifest_dir.join(filename).exists()),
        CommonCondition::WorkingDirectoryClean => {
            if !crate::utils::command_is_executable("git", environment) {
                return Err(Error::CommandNotFound("git".to_owned()));
            }
            let mut cmd = std::process::Command::new("git");
            cmd.args(["status", "--porcelain"])
                .current_dir(manifest_dir);
            let output = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?;
            Ok(output.stdout.is_empty())
        }
        CommonCondition::Not(inner) => Ok(!evaluate_common_condition(
            inner,
            manifest_dir,
            environment,
            config,
        )?),
        CommonCondition::And(conditions) => {
            for c in conditions {
                if !evaluate_common_condition(c, manifest_dir, environment, config)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        CommonCondition::Or(conditions) => {
            for c in conditions {
                if evaluate_common_condition(c, manifest_dir, environment, config)? {
                    return Ok(true);
                }
            }
            Ok(false)
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
) -> Result<bool, Error> {
    match cond {
        WorkspaceCondition::Common(inner) => {
            evaluate_common_condition(inner, manifest_dir, environment, config)
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
        )?),
        WorkspaceCondition::And(conditions) => {
            for c in conditions {
                if !evaluate_workspace_condition(c, manifest_dir, environment, config)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        WorkspaceCondition::Or(conditions) => {
            for c in conditions {
                if evaluate_workspace_condition(c, manifest_dir, environment, config)? {
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
) -> Result<bool, Error> {
    match cond {
        CrateCondition::Common(inner) => {
            evaluate_common_condition(inner, manifest_dir, environment, config)
        }
        CrateCondition::CrateType(filter) => {
            let required = match filter {
                CrateTypeFilter::Bin => CrateType::Bin,
                CrateTypeFilter::Lib => CrateType::Lib,
                CrateTypeFilter::ProcMacro => CrateType::ProcMacro,
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
        )?),
        CrateCondition::And(conditions) => {
            for c in conditions {
                if !evaluate_crate_condition(c, manifest_dir, environment, config)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        CrateCondition::Or(conditions) => {
            for c in conditions {
                if evaluate_crate_condition(c, manifest_dir, environment, config)? {
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
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::FileExists("hello.txt".to_owned()),
            dir,
            &env,
            &config,
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn common_file_exists_false() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::FileExists("missing.txt".to_owned()),
            temp.path(),
            &env,
            &config,
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn common_not() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::Not(Box::new(CommonCondition::FileExists("x".to_owned()))),
            temp.path(),
            &env,
            &config,
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }

    #[test]
    fn common_and_short_circuits() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::And(vec![
                CommonCondition::FileExists("missing".to_owned()),
                CommonCondition::FileExists("also_missing".to_owned()),
            ]),
            temp.path(),
            &env,
            &config,
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn common_or_short_circuits() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        fs_err::write(dir.join("exists.txt"), "").unwrap_or_else(|e| panic!("{e}"));
        let env = mock_env(&temp);
        let config = empty_config();
        let result = evaluate_common_condition(
            &CommonCondition::Or(vec![
                CommonCondition::FileExists("exists.txt".to_owned()),
                CommonCondition::FileExists("missing.txt".to_owned()),
            ]),
            dir,
            &env,
            &config,
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
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
            evaluate_workspace_condition(&WorkspaceCondition::Standalone, dir, &env, &config);
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
            evaluate_workspace_condition(&WorkspaceCondition::HasMembers, dir, &env, &config);
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
        );
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), false);
    }

    #[test]
    fn crate_standalone_true() {
        let temp = tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = temp.path();
        let env = mock_env(&temp);
        let config = config_with_bin_crate(dir);
        let result = evaluate_crate_condition(&CrateCondition::Standalone, dir, &env, &config);
        assert_eq!(result.unwrap_or_else(|e| panic!("{e}")), true);
    }
}
