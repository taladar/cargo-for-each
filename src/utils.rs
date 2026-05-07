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
    fs_err::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// checks if the given path is an executable file
///
/// on unix this checks for the executable bit, on windows it checks
/// for valid extensions and on other platforms it just checks for
/// the presence of a file
#[cfg(windows)]
#[must_use]
pub fn is_executable(path: &std::path::Path) -> bool {
    use std::path::Path;

    // On Windows, executability is determined by file extension. The PATHEXT
    // environment variable lists which extensions are considered executable
    // (case-insensitively); when unset, fall back to the documented Windows
    // defaults so we don't accept arbitrary extensions like `.md`.
    let pathext = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    let pathext_str = pathext.to_string_lossy();
    let exts: Vec<&str> = pathext_str.split(';').filter(|s| !s.is_empty()).collect();

    // If the input already has an extension, only treat it as executable when
    // that extension is on the PATHEXT list. Otherwise non-executable files
    // like `README.md` would qualify.
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy();
        let matches = exts.iter().any(|e| {
            e.strip_prefix('.')
                .is_some_and(|e| e.eq_ignore_ascii_case(&ext_str))
        });
        return matches && path.is_file();
    }

    // No extension on the input — try appending each PATHEXT entry and accept
    // the first match. This is how `cmd` resolves to `cmd.exe`.
    for ext in &exts {
        let mut path_with_ext = path.as_os_str().to_owned();
        path_with_ext.push(ext);
        if Path::new(&path_with_ext).is_file() {
            return true;
        }
    }
    false
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

/// Writes `contents` to `path`, then tightens the file's permissions to mode 0o600 on Unix.
///
/// On non-Unix platforms this is equivalent to `fs_err::write` (the OS default ACL applies).
///
/// Use for any state, config, or snapshot file under the user's config or state directory —
/// these may contain command lines, env-file paths, or program contents that should not be
/// exposed to other local users.
///
/// # Errors
///
/// Returns the I/O error if the file cannot be written or its permissions cannot be set.
pub fn write_user_file(
    path: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    fs_err::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs_err::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Copies `src` to `dst`, then tightens `dst`'s permissions to mode 0o600 on Unix.
///
/// On non-Unix platforms this is equivalent to `fs_err::copy`.
///
/// # Errors
///
/// Returns the I/O error if the file cannot be copied or its permissions cannot be set.
pub fn copy_user_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<u64> {
    let dst = dst.as_ref();
    let n = fs_err::copy(src.as_ref(), dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs_err::set_permissions(dst, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(n)
}

/// Recursively creates all directories in `path`, setting mode 0o700 on each newly-created
/// directory on Unix.
///
/// Existing directories are left alone — we tighten only what we create, since users may
/// intentionally have looser permissions on ancestor directories like `~/.config`.
///
/// # Errors
///
/// Returns the I/O error if a directory cannot be created or have its permissions set.
pub fn create_user_dir_all(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    create_user_dir_all_inner(path.as_ref())
}

/// Inner recursive worker for [`create_user_dir_all`]; on Unix sets mode 0o700 on each
/// newly-created directory, on other platforms delegates to `fs_err::create_dir_all`.
#[cfg(unix)]
fn create_user_dir_all_inner(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    if path.as_os_str().is_empty() || path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        create_user_dir_all_inner(parent)?;
    }
    match fs_err::create_dir(path) {
        Ok(()) => {
            fs_err::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Inner worker for [`create_user_dir_all`] on non-Unix platforms; delegates straight to
/// `fs_err::create_dir_all`.
#[cfg(not(unix))]
fn create_user_dir_all_inner(path: &std::path::Path) -> std::io::Result<()> {
    fs_err::create_dir_all(path)
}

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
    #[cfg(unix)]
    use pretty_assertions::assert_eq;
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

    /// `write_user_file` creates a file with mode 0o600 on Unix.
    #[cfg(unix)]
    #[test]
    fn write_user_file_sets_owner_only_mode() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempdir()?;
        let path = temp.path().join("secret.toml");
        super::write_user_file(&path, b"data")?;
        let mode = fs_err::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
        Ok(())
    }

    /// `write_user_file` tightens an existing world-readable file to 0o600.
    #[cfg(unix)]
    #[test]
    fn write_user_file_tightens_existing_loose_file() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempdir()?;
        let path = temp.path().join("preexisting.toml");
        fs_err::write(&path, b"old")?;
        fs_err::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        super::write_user_file(&path, b"new")?;
        let mode = fs_err::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
        Ok(())
    }

    /// `create_user_dir_all` creates each new directory with mode 0o700 on Unix.
    #[cfg(unix)]
    #[test]
    fn create_user_dir_all_sets_owner_only_mode() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempdir()?;
        let nested = temp.path().join("a").join("b").join("c");
        super::create_user_dir_all(&nested)?;
        for p in [
            temp.path().join("a"),
            temp.path().join("a").join("b"),
            nested,
        ] {
            let mode = fs_err::metadata(&p)?.permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o700,
                "expected 0o700 on {}, got {mode:o}",
                p.display()
            );
        }
        Ok(())
    }

    /// `create_user_dir_all` does not modify already-existing directories' permissions.
    #[cfg(unix)]
    #[test]
    fn create_user_dir_all_leaves_existing_dirs_alone() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempdir()?;
        let outer = temp.path().join("outer");
        fs_err::create_dir(&outer)?;
        fs_err::set_permissions(&outer, std::fs::Permissions::from_mode(0o755))?;
        let inner = outer.join("inner");
        super::create_user_dir_all(&inner)?;
        let outer_mode = fs_err::metadata(&outer)?.permissions().mode() & 0o777;
        let inner_mode = fs_err::metadata(&inner)?.permissions().mode() & 0o777;
        assert_eq!(outer_mode, 0o755, "outer should be unchanged");
        assert_eq!(inner_mode, 0o700, "newly created inner should be 0o700");
        Ok(())
    }
}
