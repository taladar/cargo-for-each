//! Utility functions

/// check if a command is executable either as an absolute path or
/// with any of the paths from environment.paths prepended
#[must_use]
pub fn command_is_executable(command: &str, environment: &crate::Environment) -> bool {
    // Check if command exists and is executable before adding it
    let command_path = std::path::Path::new(command);
    if command_path.is_absolute() {
        crate::utils::is_executable(command_path)
    } else {
        environment
            .paths
            .iter()
            .any(|p| crate::utils::is_executable(&p.join(command)))
    }
}

/// checks if the given path is an executable file
///
/// on unix this checks for the executable bit, on windows it checks
/// for valid extensions and on other platforms it just checks for
/// the presence of a file
#[cfg(unix)]
#[must_use]
pub fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs_err::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// checks if the given path is an executable file
///
/// on unix this checks for the executable bit, on windows it checks
/// for valid extensions and on other platforms it just checks for
/// the presence of a file
#[cfg(windows)]
#[must_use]
pub fn is_executable(path: &std::path::Path) -> bool {
    // On Windows, executability is determined by file extension.
    // We check against PATHEXT environment variable.
    if path.extension().is_some() && path.is_file() {
        return true;
    }
    if let Some(pathext) = std::env::var_os("PATHEXT") {
        let pathexts = pathext.to_string_lossy();
        for ext in pathexts.split(';').filter(|s| !s.is_empty()) {
            let mut path_with_ext = path.as_os_str().to_owned();
            path_with_ext.push(ext);
            if Path::new(&path_with_ext).is_file() {
                return true;
            }
        }
    }
    path.is_file()
}

/// checks if the given path is an executable file
///
/// on unix this checks for the executable bit, on windows it checks
/// for valid extensions and on other platforms it just checks for
/// the presence of a file
#[cfg(all(not(unix), not(windows)))]
#[must_use]
pub fn is_executable(path: &std::path::Path) -> bool {
    // Fallback for non-unix, non-windows systems.
    path.is_file()
}

use crate::Environment;
use crate::error::Error;
use std::process::{Command, Output, Stdio};

/// Executes a command, optionally suppressing its stdout/stderr and tracing them instead.
///
/// If `environment.suppress_subprocess_output` is `true`, the command's stdout and stderr
/// are captured and logged at `tracing::trace` level. Otherwise, they are inherited
/// from the parent process.
///
/// # Arguments
///
/// * `command` - A mutable reference to the `std::process::Command` to execute.
/// * `environment` - A reference to the application's `Environment` configuration.
/// * `cwd` - The current working directory in which to execute the command.
///
/// # Errors
///
/// returns an error if the command execution fails
pub fn execute_command(
    command: &mut Command,
    environment: &Environment,
    cwd: &std::path::Path,
) -> Result<Output, Error> {
    if environment.suppress_subprocess_output {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = command.output().map_err(|e| {
            Error::CommandExecutionFailed(format!("{command:?}"), cwd.to_path_buf(), e)
        })?;

        tracing::trace!(
            "Command stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        tracing::trace!(
            "Command stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(output)
    } else {
        let output = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| {
                Error::CommandExecutionFailed(format!("{command:?}"), cwd.to_path_buf(), e)
            })?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::command_is_executable;
    use crate::Environment;
    use tempfile::tempdir;

    fn env_with_paths(paths: Vec<std::path::PathBuf>) -> Environment {
        Environment {
            config_dir: std::path::PathBuf::new(),
            state_dir: std::path::PathBuf::new(),
            paths,
            suppress_subprocess_output: true,
        }
    }

    /// A command that lives in `environment.paths` is found.
    ///
    /// Regression test for Bug 5: previously `command_is_executable` used the
    /// system PATH env var and completely ignored `environment.paths`.
    #[test]
    fn test_command_found_in_env_paths() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let bin = temp.path().join("my_test_cmd");
        fs_err::write(&bin, "#!/bin/sh\nexit 0\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs_err::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        }
        let env = env_with_paths(vec![temp.path().to_path_buf()]);
        assert!(
            command_is_executable("my_test_cmd", &env),
            "command in environment.paths should be found"
        );
        Ok(())
    }

    /// A command that is NOT in `environment.paths` is not found, even if it
    /// would be found via the system PATH.
    ///
    /// This verifies that the function exclusively uses `environment.paths` and
    /// does not fall back to the process-level PATH environment variable.
    #[test]
    fn test_command_not_found_when_absent_from_env_paths() {
        // Use an empty path list — nothing should be found.
        let env = env_with_paths(vec![]);
        assert!(
            !command_is_executable("cargo", &env),
            "command should not be found when environment.paths is empty"
        );
    }

    /// An absolute path to an existing executable is accepted.
    #[test]
    fn test_absolute_path_executable_is_found() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let bin = temp.path().join("abs_cmd");
        fs_err::write(&bin, "#!/bin/sh\nexit 0\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs_err::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        }
        // Absolute path lookup ignores environment.paths, so pass an empty list.
        let env = env_with_paths(vec![]);
        let bin_str = bin.to_str().ok_or("non-UTF8 path")?;
        assert!(
            command_is_executable(bin_str, &env),
            "absolute path to an executable should be found"
        );
        Ok(())
    }

    /// An absolute path to a non-existent file is rejected.
    #[test]
    fn test_absolute_path_nonexistent_is_not_found() {
        let env = env_with_paths(vec![]);
        assert!(
            !command_is_executable("/nonexistent/path/to/nothing", &env),
            "absolute path to non-existent file should not be found"
        );
    }
}
