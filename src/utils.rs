//! Utility functions

/// check if a command is executable either as an absolute path or
/// with any of the paths from PATH prepended
#[must_use]
pub fn command_is_executable(command: &str) -> bool {
    // Check if command exists and is executable before adding it
    let command_path = std::path::Path::new(&command);
    if command_path.is_absolute() {
        crate::utils::is_executable(command_path)
    } else {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .find(|p| crate::utils::is_executable(&p.join(command)))
            })
            .is_some()
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
