//! Defines the `Condition` type for evaluating boolean conditions in plan control flow.

use std::path::Path;

use crate::error::Error;

/// A boolean condition that can be evaluated per-target during task execution.
///
/// All variants are struct variants (even when empty) to ensure consistent
/// serialization as TOML inline tables within arrays. This allows `Vec<Condition>`
/// to serialize as a TOML array of inline tables rather than a mix of strings
/// and tables.
#[expect(
    clippy::empty_enum_variants_with_brackets,
    reason = "Struct variants (even empty ones) are required so that Vec<Condition> serializes as a TOML array of inline tables"
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Condition {
    // ── User interaction ──────────────────────────────────────────────────────
    /// Ask the user a yes/no question. True if the user answers yes/y.
    AskUser {
        /// The question to display to the user.
        question: String,
    },

    // ── Command-based ─────────────────────────────────────────────────────────
    /// Run a command in the target's directory. True if the command exits with code 0.
    RunCommand {
        /// The command to execute.
        command: String,
        /// The arguments to pass to the command.
        #[serde(default)]
        args: Vec<String>,
    },

    // ── Crate type conditions ─────────────────────────────────────────────────
    /// True if the current target is a binary crate.
    IsBinaryCrate {},
    /// True if the current target is a library crate.
    IsLibraryCrate {},
    /// True if the current target is a procedural macro crate.
    IsProcMacroCrate {},

    // ── Workspace type conditions ─────────────────────────────────────────────
    /// True if the current target is a standalone crate workspace (single-crate).
    IsStandaloneWorkspace {},
    /// True if the current target is a workspace with multiple member crates.
    IsWorkspaceWithMembers {},

    // ── Logical combinators ───────────────────────────────────────────────────
    /// True if the inner condition evaluates to false.
    Not {
        /// The condition to negate.
        condition: Box<Self>,
    },
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    And {
        /// The conditions that must all be true.
        conditions: Vec<Self>,
    },
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    Or {
        /// The conditions of which at least one must be true.
        conditions: Vec<Self>,
    },
}

impl Condition {
    /// Evaluate this condition for the given target.
    ///
    /// # Errors
    ///
    /// Returns an error if a `RunCommand` condition's command cannot be launched
    /// (not found, IO error), or if `AskUser` IO fails.
    #[expect(
        clippy::print_stdout,
        reason = "AskUser is part of the UI, not logging"
    )]
    pub fn evaluate(
        &self,
        manifest_dir: &Path,
        environment: &crate::Environment,
        config: &crate::Config,
    ) -> Result<bool, Error> {
        match self {
            Self::AskUser { question } => {
                use std::io::Write as _;
                print!("{question} (y/N) ");
                std::io::stdout().flush().map_err(Error::IoError)?;
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(Error::IoError)?;
                let answer = answer.trim().to_lowercase();
                Ok(answer == "y" || answer == "yes")
            }
            Self::RunCommand { command, args } => {
                if !crate::utils::command_is_executable(command, environment) {
                    return Err(Error::CommandNotFound(command.to_owned()));
                }
                let mut cmd = std::process::Command::new(command);
                cmd.args(args).current_dir(manifest_dir);
                let output = crate::utils::execute_command(&mut cmd, environment, manifest_dir)?;
                Ok(output.status.success())
            }
            Self::IsBinaryCrate {} => Ok(config.crates.iter().any(|c| {
                c.manifest_dir == manifest_dir && c.types.contains(&crate::targets::CrateType::Bin)
            })),
            Self::IsLibraryCrate {} => Ok(config.crates.iter().any(|c| {
                c.manifest_dir == manifest_dir && c.types.contains(&crate::targets::CrateType::Lib)
            })),
            Self::IsProcMacroCrate {} => Ok(config.crates.iter().any(|c| {
                c.manifest_dir == manifest_dir
                    && c.types.contains(&crate::targets::CrateType::ProcMacro)
            })),
            Self::IsStandaloneWorkspace {} => Ok(config
                .workspaces
                .iter()
                .any(|w| w.manifest_dir == manifest_dir && w.is_standalone)),
            Self::IsWorkspaceWithMembers {} => Ok(config
                .workspaces
                .iter()
                .any(|w| w.manifest_dir == manifest_dir && !w.is_standalone)),
            Self::Not { condition } => {
                Ok(!condition.evaluate(manifest_dir, environment, config)?)
            }
            Self::And { conditions } => {
                for condition in conditions {
                    if !condition.evaluate(manifest_dir, environment, config)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::Or { conditions } => {
                for condition in conditions {
                    if condition.evaluate(manifest_dir, environment, config)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// Save a boolean condition result to the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save_result(result: bool, path: &Path) -> Result<(), Error> {
        fs_err::write(path, if result { "true" } else { "false" })
            .map_err(|e| Error::CouldNotWriteStateFile(path.to_path_buf(), e))
    }

    /// Load a previously saved condition result from the file at `path`.
    ///
    /// Returns `None` if the file does not exist (condition not yet evaluated).
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or has unexpected content.
    pub fn load_result(path: &Path) -> Result<Option<bool>, Error> {
        if !path.exists() {
            return Ok(None);
        }
        let content = fs_err::read_to_string(path).map_err(Error::IoError)?;
        match content.trim() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            other => Err(Error::InvalidConditionResult(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn config_with_bin_crate(manifest_dir: &std::path::Path) -> crate::Config {
        crate::Config {
            crates: vec![crate::Crate {
                manifest_dir: manifest_dir.to_path_buf(),
                workspace_manifest_dir: manifest_dir.to_path_buf(),
                types: [crate::targets::CrateType::Bin].into(),
            }],
            workspaces: vec![],
        }
    }

    fn empty_config() -> crate::Config {
        crate::Config {
            crates: vec![],
            workspaces: vec![],
        }
    }

    fn mock_environment(
        temp_dir: &tempfile::TempDir,
    ) -> Result<crate::Environment, Box<dyn std::error::Error>> {
        crate::Environment::mock(temp_dir)
    }

    #[test]
    fn test_and_empty_is_true() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = empty_config();
        let result = Condition::And { conditions: vec![] }.evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_and_all_true() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = crate::Config {
            crates: vec![crate::Crate {
                manifest_dir: dir.to_path_buf(),
                workspace_manifest_dir: dir.to_path_buf(),
                types: [
                    crate::targets::CrateType::Bin,
                    crate::targets::CrateType::Lib,
                ]
                .into(),
            }],
            workspaces: vec![],
        };
        let result = Condition::And {
            conditions: vec![Condition::IsBinaryCrate {}, Condition::IsLibraryCrate {}],
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_and_with_false_short_circuits() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = config_with_bin_crate(dir);
        // IsBinaryCrate is true, IsLibraryCrate is false — And should return false
        let result = Condition::And {
            conditions: vec![Condition::IsBinaryCrate {}, Condition::IsLibraryCrate {}],
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }

    #[test]
    fn test_or_empty_is_false() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = empty_config();
        let result = Condition::Or { conditions: vec![] }.evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }

    #[test]
    fn test_or_all_false() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = empty_config();
        let result = Condition::Or {
            conditions: vec![Condition::IsBinaryCrate {}, Condition::IsLibraryCrate {}],
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }

    #[test]
    fn test_or_with_true_short_circuits() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = config_with_bin_crate(dir);
        let result = Condition::Or {
            conditions: vec![Condition::IsBinaryCrate {}, Condition::IsLibraryCrate {}],
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_not_of_true_is_false() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = config_with_bin_crate(dir);
        let result = Condition::Not {
            condition: Box::new(Condition::IsBinaryCrate {}),
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }

    #[test]
    fn test_not_of_false_is_true() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = empty_config();
        let result = Condition::Not {
            condition: Box::new(Condition::IsBinaryCrate {}),
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_is_binary_crate_matching() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = config_with_bin_crate(dir);
        let result = Condition::IsBinaryCrate {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_is_binary_crate_no_match() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = empty_config();
        let result = Condition::IsBinaryCrate {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }

    #[test]
    fn test_is_library_crate() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = crate::Config {
            crates: vec![crate::Crate {
                manifest_dir: dir.to_path_buf(),
                workspace_manifest_dir: dir.to_path_buf(),
                types: [crate::targets::CrateType::Lib].into(),
            }],
            workspaces: vec![],
        };
        let result = Condition::IsLibraryCrate {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_is_proc_macro_crate() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = crate::Config {
            crates: vec![crate::Crate {
                manifest_dir: dir.to_path_buf(),
                workspace_manifest_dir: dir.to_path_buf(),
                types: [crate::targets::CrateType::ProcMacro].into(),
            }],
            workspaces: vec![],
        };
        let result = Condition::IsProcMacroCrate {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_is_standalone_workspace_true() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = crate::Config {
            crates: vec![],
            workspaces: vec![crate::Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: true,
            }],
        };
        let result = Condition::IsStandaloneWorkspace {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_is_standalone_workspace_false_for_non_standalone() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = crate::Config {
            crates: vec![],
            workspaces: vec![crate::Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: false,
            }],
        };
        let result = Condition::IsStandaloneWorkspace {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }

    #[test]
    fn test_is_workspace_with_members_true() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = crate::Config {
            crates: vec![],
            workspaces: vec![crate::Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: false,
            }],
        };
        let result = Condition::IsWorkspaceWithMembers {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[test]
    fn test_is_workspace_with_members_false_for_standalone() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = crate::Config {
            crates: vec![],
            workspaces: vec![crate::Workspace {
                manifest_dir: dir.to_path_buf(),
                is_standalone: true,
            }],
        };
        let result = Condition::IsWorkspaceWithMembers {}.evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }

    #[test]
    fn test_save_and_load_result_true() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir.path().join("result");
        Condition::save_result(true, &path)?;
        let loaded = Condition::load_result(&path)?;
        assert_eq!(loaded, Some(true));
        Ok(())
    }

    #[test]
    fn test_save_and_load_result_false() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir.path().join("result");
        Condition::save_result(false, &path)?;
        let loaded = Condition::load_result(&path)?;
        assert_eq!(loaded, Some(false));
        Ok(())
    }

    #[test]
    fn test_load_result_nonexistent_returns_none() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir.path().join("nonexistent");
        let loaded = Condition::load_result(&path)?;
        assert_eq!(loaded, None);
        Ok(())
    }

    #[test]
    fn test_toml_roundtrip_is_binary_crate() -> TestResult {
        let condition = Condition::IsBinaryCrate {};
        let serialized = toml::to_string(&condition)?;
        let deserialized: Condition = toml::from_str(&serialized)?;
        assert!(matches!(deserialized, Condition::IsBinaryCrate {}));
        Ok(())
    }

    #[test]
    fn test_toml_roundtrip_and_with_conditions() -> TestResult {
        let condition = Condition::And {
            conditions: vec![Condition::IsBinaryCrate {}, Condition::IsLibraryCrate {}],
        };
        let serialized = toml::to_string(&condition)?;
        let deserialized: Condition = toml::from_str(&serialized)?;
        assert!(matches!(deserialized, Condition::And { conditions } if conditions.len() == 2));
        Ok(())
    }

    #[test]
    fn test_toml_roundtrip_not() -> TestResult {
        let condition = Condition::Not {
            condition: Box::new(Condition::IsBinaryCrate {}),
        };
        let serialized = toml::to_string(&condition)?;
        let deserialized: Condition = toml::from_str(&serialized)?;
        assert!(matches!(deserialized, Condition::Not { .. }));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_run_command_true() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = empty_config();
        let result = Condition::RunCommand {
            command: "true".to_string(),
            args: vec![],
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, true);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_run_command_false() -> TestResult {
        let temp_dir = tempfile::tempdir()?;
        let env = mock_environment(&temp_dir)?;
        let dir = temp_dir.path();
        let config = empty_config();
        let result = Condition::RunCommand {
            command: "false".to_string(),
            args: vec![],
        }
        .evaluate(dir, &env, &config)?;
        assert_eq!(result, false);
        Ok(())
    }
}
