//! `cargo-for-each` is a tool to run commands on multiple cargo projects.
//!
//! This library provides the core logic for managing workspaces, crates, and
//! tasks for the `cargo-for-each` CLI.  Programs are expressed as `.cfe`
//! (cargo-for-each) text files and executed against registered target
//! workspaces and crates.

/// Handles application-specific errors.
pub mod error;
/// Implements the `.cfe` program language: AST, parser, evaluation, and resolution.
pub mod program;
/// Defines target-related structures and resolution logic.
pub mod targets;
/// Implements functionality for managing tasks.
pub mod tasks;
/// Implements utility functions.
pub mod utils;

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// which subcommand to call
#[derive(clap::Parser, Debug)]
pub enum Command {
    /// Manage workspaces and crates (add, remove, list, refresh).
    Target(crate::targets::TargetParameters),
    /// manage tasks
    Task(crate::tasks::TaskParameters),

    /// Generate man page
    GenerateManpage {
        /// target dir for man page generation
        #[clap(long)]
        output_dir: PathBuf,
    },
    /// Generate shell completion
    GenerateShellCompletion {
        /// output file for shell completion generation
        #[clap(long)]
        output_file: PathBuf,
        /// which shell
        #[clap(long)]
        shell: clap_complete::aot::Shell,
    },
}

/// The Clap type for all the commandline parameters
#[derive(clap::Parser, Debug)]
#[clap(name = "cargo-for-each",
       about = clap::crate_description!(),
       author = clap::crate_authors!(),
       version = clap::crate_version!(),
       )]
pub struct Options {
    /// which subcommand to use
    #[clap(subcommand)]
    command: Command,
}

/// stores the information we get from environment variables
/// so we can easily mock them for testing
#[derive(Debug, Clone)]
pub struct Environment {
    /// user config dir (`XDG_CONFIG_HOME` on Linux, `~/Library/Application Support`
    /// on macOS, `%APPDATA%` on Windows — see the `dirs` crate)
    pub config_dir: std::path::PathBuf,
    /// user state dir (`XDG_STATE_HOME` on Linux; macOS/Windows fall back to
    /// platform conventions — see the `dirs` crate)
    pub state_dir: std::path::PathBuf,
    /// paths from PATH
    pub paths: Vec<std::path::PathBuf>,
    /// if true, sub-processes stdout and stderr are suppressed and traced
    pub suppress_subprocess_output: bool,
}

impl Environment {
    /// create an environment for production use
    ///
    /// # Errors
    ///
    /// fails if we can not retrieve the information from the environment
    pub fn new() -> Result<Self, crate::error::Error> {
        let path_var = std::env::var("PATH")?;
        Ok(Self {
            config_dir: dirs::config_dir()
                .ok_or(crate::error::Error::CouldNotDetermineUserConfigDir)?,
            state_dir: dirs::state_dir().ok_or(crate::error::Error::CouldNotDetermineStateDir)?,
            paths: std::env::split_paths(&path_var).collect(),
            suppress_subprocess_output: false,
        })
    }

    /// create a mock environment for testing
    ///
    /// # Errors
    ///
    /// fails if creating the temporary directories fails
    #[cfg(test)]
    pub fn mock(temp_dir: &tempfile::TempDir) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_path = temp_dir.path();

        // Create 'bin', 'config', and 'state' subdirectories
        let config_dir = temp_path.join("config");
        let state_dir = temp_path.join("state");
        let bin_dir = temp_path.join("bin");

        fs_err::create_dir_all(&config_dir)?;
        fs_err::create_dir_all(&state_dir)?;
        fs_err::create_dir_all(&bin_dir)?;

        // Start with the test-specific bin_dir, then append the real system PATH so
        // standard commands like `cargo` are also found via command_is_executable.
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin_dir];
        paths.extend(std::env::split_paths(&path_var));

        Ok(Self {
            config_dir,
            state_dir,
            paths,
            suppress_subprocess_output: true,
        })
    }
}

/// the main function of the app
///
/// # Errors
///
/// fails if the main app fails
pub async fn run_app(
    options: Options,
    environment: Environment,
) -> Result<(), crate::error::Error> {
    match options.command {
        Command::Target(target_parameters) => {
            crate::targets::target_command(target_parameters, environment).await?;
        }
        Command::Task(task_parameters) => {
            crate::tasks::task_command(task_parameters, environment).await?;
        }

        Command::GenerateManpage { output_dir } => {
            // generate man pages
            clap_mangen::generate_to(<Options as clap::CommandFactory>::command(), output_dir)
                .map_err(crate::error::Error::GenerateManpageError)?;
        }
        Command::GenerateShellCompletion { output_file, shell } => {
            let mut f = std::fs::File::create(output_file)
                .map_err(crate::error::Error::GenerateShellCompletionError)?;
            let mut c = <Options as clap::CommandFactory>::command();
            clap_complete::generate(shell, &mut c, "cargo-for-each", &mut f);
        }
    }

    Ok(())
}

/// represents a Rust workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// the directory that contains the workspace Cargo.toml file
    pub manifest_dir: PathBuf,
    /// is this a standalone crate workspace
    pub is_standalone: bool,
}

/// represents a Rust crate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crate {
    /// the directory that contains the crate Cargo.toml file
    pub manifest_dir: PathBuf,
    /// the directory that contains the workspace Cargo.toml file for this crate
    pub workspace_manifest_dir: PathBuf,
    /// the compile-time output kinds of this crate (e.g. `bin`, `lib`,
    /// `proc-macro`). May be empty for packages that only declare auxiliary
    /// targets.
    pub crate_types: BTreeSet<crate::targets::CrateType>,
    /// the auxiliary cargo target kinds attached to this package (e.g. `test`,
    /// `bench`, `example`, `custom_build`). Almost every normal package has at
    /// least `Test`.
    pub target_kinds: BTreeSet<crate::targets::TargetKind>,
}

/// represents the cargo-for-each configuration file
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// represents all the workspaces we know about
    pub workspaces: Vec<Workspace>,
    /// presents all the crates we know about
    pub crates: Vec<Crate>,
}

impl Config {
    /// adds a workspace to the config if it is not already present
    pub fn add_workspace(&mut self, workspace: Workspace) {
        if self
            .workspaces
            .iter()
            .any(|w| w.manifest_dir == workspace.manifest_dir)
        {
            tracing::debug!(
                "Workspace at {} already exists, not adding.",
                workspace.manifest_dir.display()
            );
        } else {
            tracing::debug!(
                "Adding new workspace at {}",
                workspace.manifest_dir.display()
            );
            self.workspaces.push(workspace);
        }
    }

    /// adds a crate to the config, ignoring the new one if one with the same manifest directory already exists
    pub fn add_crate(&mut self, krate: Crate) {
        if self
            .crates
            .iter()
            .any(|c| c.manifest_dir == krate.manifest_dir)
        {
            tracing::debug!(
                "Crate at {} already exists, not adding.",
                krate.manifest_dir.display()
            );
        } else {
            tracing::debug!("Adding new crate at {}", krate.manifest_dir.display());
            self.crates.push(krate);
        }
    }

    /// Load the config file
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or if its content cannot be parsed.
    pub fn load(environment: &Environment) -> Result<Self, crate::error::Error> {
        let config_file_path = config_file(environment);
        if fs_err::exists(&config_file_path).map_err(crate::error::Error::CouldNotReadConfigFile)? {
            let file_content = fs_err::read_to_string(&config_file_path)
                .map_err(crate::error::Error::CouldNotReadConfigFile)?;
            toml::from_str(&file_content).map_err(crate::error::Error::CouldNotParseConfigFile)
        } else {
            Ok(Self::default())
        }
    }

    /// Save the config file
    ///
    /// # Errors
    ///
    /// Returns an error if parent directories cannot be created, if the config
    /// cannot be serialized, or if the config file cannot be written.
    pub fn save(&self, environment: &Environment) -> Result<(), crate::error::Error> {
        let config_file_path = config_file(environment);
        if let Some(config_dir_path) = config_file_path.parent() {
            crate::utils::create_user_dir_all(config_dir_path)
                .map_err(crate::error::Error::CouldNotCreateConfigFileParentDirs)?;
        }
        crate::utils::write_user_file(
            &config_file_path,
            toml::to_string(self).map_err(crate::error::Error::CouldNotSerializeConfigFile)?,
        )
        .map_err(crate::error::Error::CouldNotWriteConfigFile)
    }
}

/// RAII guard holding an exclusive advisory lock on the sidecar lock file
/// at `<config_dir>/cargo-for-each.lock` for the lifetime of the guard.
///
/// Wrap any load-modify-save sequence on the global config (`target add`,
/// `target remove`, `target refresh`, future commands) in:
///
/// ```ignore
/// let _lock = ConfigLock::acquire(&environment)?;
/// let mut config = Config::load(&environment)?;
/// // … mutate …
/// config.save(&environment)?;
/// ```
///
/// Concurrent invocations block at `acquire` until the previous one drops
/// its guard, eliminating the last-writer-wins race where two `target add`
/// calls both read the same baseline and one's entry gets dropped.
///
/// The lock is advisory (`std::fs::File::lock`, stabilised in Rust 1.89) —
/// processes that bypass `ConfigLock` are not blocked. Releasing happens
/// automatically on drop; no explicit unlock is required.
#[derive(Debug)]
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct ConfigLock {
    /// Held only to keep the OS-level lock alive; never read.
    _file: fs_err::File,
}

impl ConfigLock {
    /// Acquire an exclusive lock on the per-config lock file, blocking until
    /// the lock is available.
    ///
    /// Creates the lock file (and any missing parent directories) on first
    /// use with mode 0o600 / 0o700 respectively.
    ///
    /// # Errors
    ///
    /// Returns an error if the config directory cannot be created, the lock
    /// file cannot be opened, or the kernel lock call fails.
    pub fn acquire(environment: &Environment) -> Result<Self, crate::error::Error> {
        let lock_path = config_dir_path(environment).join("cargo-for-each.lock");
        if let Some(parent) = lock_path.parent() {
            crate::utils::create_user_dir_all(parent)
                .map_err(crate::error::Error::CouldNotCreateConfigFileParentDirs)?;
        }
        // An empty lock file is sufficient as a sync primitive; we never read
        // or write its contents. `truncate(false)` keeps it empty across runs
        // without destroying anything users might place there.
        let file = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| crate::error::Error::CouldNotAcquireConfigLock(lock_path.clone(), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Best-effort tightening; an error here (e.g. the lock file
            // pre-exists with non-modifiable perms) is not worth aborting
            // the whole load/modify/save flow over.
            let _result = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        file.file()
            .lock()
            .map_err(|e| crate::error::Error::CouldNotAcquireConfigLock(lock_path, e))?;
        Ok(Self { _file: file })
    }
}

/// returns the config dir path
#[must_use]
pub fn config_dir_path(environment: &Environment) -> PathBuf {
    environment.config_dir.join("cargo-for-each")
}

/// returns the config file path
#[must_use]
pub fn config_file(environment: &Environment) -> PathBuf {
    config_dir_path(environment).join("cargo-for-each.toml")
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helpers panic on unexpected match arms; clearer than assert with message"
    )]

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{
        targets::{
            AddParameters, ListParameters, TargetFilter, TargetParameters, TargetSubCommand,
            WorkspaceFilterParameters,
        },
        tasks::{
            CreateTaskParameters, RunAllTargetsParameters, TaskParameters, TaskRunParameters,
            TaskRunSubCommand, TaskSubCommand,
        },
        utils::execute_command,
    };

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_target_list() -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory for the test environment
        // needs to be done here since it cleans up when it goes
        // out of scope
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;

        // Create Options for the "targets list" command
        let options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::List(ListParameters {
                    target_filter: TargetFilter::Workspaces(WorkspaceFilterParameters::default()),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment).await;
        assert!(
            result.is_ok(),
            "run_app failed with error: {:?}",
            result.err()
        );

        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_full_workflow_crates() -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory for the test environment
        // needs to be done here since it cleans up when it goes
        // out of scope
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();
        let workspaces_dir = temp_path.join("workspaces");
        fs_err::create_dir_all(&workspaces_dir)?;

        tracing::debug!("Creating library crate test1");

        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspaces_dir)
            .arg("new")
            .arg("--lib")
            .arg("test1");

        let output = execute_command(&mut cmd, &environment, &workspaces_dir)?;
        assert!(
            output.status.success(),
            "Creating test crate test1 failed with status {} stdout {} stderr {}",
            output.status,
            std::str::from_utf8(&output.stdout)?,
            std::str::from_utf8(&output.stderr)?,
        );

        tracing::debug!("Adding test1 as a target");

        let options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: workspaces_dir.join("test1").join("Cargo.toml"),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment.clone()).await;
        assert!(
            result.is_ok(),
            "run_app for adding test1 target failed with error: {:?}",
            result.err()
        );

        tracing::debug!("Creating binary crate test2");

        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspaces_dir)
            .arg("new")
            .arg("--bin")
            .arg("test2");

        let output = execute_command(&mut cmd, &environment, &workspaces_dir)?;
        assert!(
            output.status.success(),
            "Creating test crate test2 failed with status {} stdout {} stderr {}",
            output.status,
            std::str::from_utf8(&output.stdout)?,
            std::str::from_utf8(&output.stderr)?,
        );

        tracing::debug!("Adding test2 as a target");

        let options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: workspaces_dir.join("test2").join("Cargo.toml"),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment.clone()).await;
        assert!(
            result.is_ok(),
            "run_app for adding test2 target failed with error: {:?}",
            result.err()
        );

        tracing::debug!("Writing test.cfe program file");

        let cfe_path = temp_path.join("test.cfe");
        fs_err::write(
            &cfe_path,
            "select crates;\nfor crate {\n    run \"cargo\" \"build\";\n}\n",
        )?;

        tracing::debug!("Creating task test-task from test.cfe");

        let options = Options {
            command: Command::Task(TaskParameters {
                sub_command: TaskSubCommand::Create(CreateTaskParameters {
                    name: "test-task".to_string(),
                    program: cfe_path,
                    workspaces: vec![],
                    crates: vec![],
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment.clone()).await;
        assert!(
            result.is_ok(),
            "run_app for creating plan failed with error: {:?}",
            result.err()
        );

        tracing::debug!("Running task test-task");

        let options = Options {
            command: Command::Task(TaskParameters {
                sub_command: TaskSubCommand::Run(TaskRunParameters {
                    sub_command: TaskRunSubCommand::AllTargets(RunAllTargetsParameters {
                        name: "test-task".to_string(),
                        jobs: None,
                        keep_going: false,
                    }),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment).await;
        assert!(
            result.is_ok(),
            "run_app for creating plan failed with error: {:?}",
            result.err()
        );

        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_full_workflow_workspaces() -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory for the test environment
        // needs to be done here since it cleans up when it goes
        // out of scope
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();
        let workspaces_dir = temp_path.join("workspaces");
        fs_err::create_dir_all(&workspaces_dir)?;

        tracing::debug!("Creating workspace1");

        let workspace1_dir = workspaces_dir.join("workspace1");
        fs_err::create_dir_all(&workspace1_dir)?;
        fs_err::write(
            workspace1_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [ \"test1\", \"test2\" ]\nresolver = \"2\"\n",
        )?;

        tracing::debug!("Creating library crate test1");

        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace1_dir)
            .arg("new")
            .arg("--lib")
            .arg("test1");

        let output = execute_command(&mut cmd, &environment, &workspace1_dir)?;
        assert!(
            output.status.success(),
            "Creating test crate test1 failed with status {} stdout {} stderr {}",
            output.status,
            std::str::from_utf8(&output.stdout)?,
            std::str::from_utf8(&output.stderr)?,
        );

        tracing::debug!("Creating binary crate test2");

        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace1_dir)
            .arg("new")
            .arg("--bin")
            .arg("test2");

        let output = execute_command(&mut cmd, &environment, &workspace1_dir)?;
        assert!(
            output.status.success(),
            "Creating test crate test2 failed with status {} stdout {} stderr {}",
            output.status,
            std::str::from_utf8(&output.stdout)?,
            std::str::from_utf8(&output.stderr)?,
        );

        tracing::debug!("Adding workspace1 as a target");

        let options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: workspace1_dir.join("Cargo.toml"),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment.clone()).await;
        assert!(
            result.is_ok(),
            "run_app for adding workspace1 target failed with error: {:?}",
            result.err()
        );

        tracing::debug!("Creating workspace2");

        let workspace2_dir = workspaces_dir.join("workspace2");
        fs_err::create_dir_all(&workspace2_dir)?;
        fs_err::write(
            workspace2_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [ \"test3\", \"test4\" ]\nresolver = \"2\"\n",
        )?;

        tracing::debug!("Creating library crate test3");

        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace2_dir)
            .arg("new")
            .arg("--lib")
            .arg("test3");

        let output = execute_command(&mut cmd, &environment, &workspace2_dir)?;
        assert!(
            output.status.success(),
            "Creating test crate test3 failed with status {} stdout {} stderr {}",
            output.status,
            std::str::from_utf8(&output.stdout)?,
            std::str::from_utf8(&output.stderr)?,
        );

        tracing::debug!("Creating binary crate test4");

        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace2_dir)
            .arg("new")
            .arg("--bin")
            .arg("test4");

        let output = execute_command(&mut cmd, &environment, &workspace2_dir)?;
        assert!(
            output.status.success(),
            "Creating test crate test4 failed with status {} stdout {} stderr {}",
            output.status,
            std::str::from_utf8(&output.stdout)?,
            std::str::from_utf8(&output.stderr)?,
        );

        tracing::debug!("Adding workspace2 as a target");

        let options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: workspace2_dir.join("Cargo.toml"),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment.clone()).await;
        assert!(
            result.is_ok(),
            "run_app for adding workspace1 target failed with error: {:?}",
            result.err()
        );

        tracing::debug!("Writing test.cfe program file");

        let cfe_path = temp_path.join("test.cfe");
        fs_err::write(
            &cfe_path,
            "select workspaces;\nfor workspace {\n    run \"cargo\" \"build\";\n}\n",
        )?;

        tracing::debug!("Creating task test-task from test.cfe");

        let options = Options {
            command: Command::Task(TaskParameters {
                sub_command: TaskSubCommand::Create(CreateTaskParameters {
                    name: "test-task".to_string(),
                    program: cfe_path,
                    workspaces: vec![],
                    crates: vec![],
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment.clone()).await;
        assert!(
            result.is_ok(),
            "run_app for creating plan failed with error: {:?}",
            result.err()
        );

        tracing::debug!("Running task test-task");

        let options = Options {
            command: Command::Task(TaskParameters {
                sub_command: TaskSubCommand::Run(TaskRunParameters {
                    sub_command: TaskRunSubCommand::AllTargets(RunAllTargetsParameters {
                        name: "test-task".to_string(),
                        jobs: None,
                        keep_going: false,
                    }),
                }),
            }),
        };

        // Call run_app and assert it completes successfully
        let result = run_app(options, environment).await;
        assert!(
            result.is_ok(),
            "run_app for creating plan failed with error: {:?}",
            result.err()
        );

        Ok(())
    }

    /// A task whose only step always fails must terminate when run with
    /// `keep_going = true` and return `SomeStepsFailed`, not loop forever and
    /// not return `CircularDependency`.
    ///
    /// Regression test for Bug 1 (infinite loop) and Bug 3 (wrong error kind).
    ///
    /// The `.cfe` program uses a nonexistent command so that execution fails at
    /// run time reliably regardless of installed tooling.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_run_all_targets_keep_going_terminates_with_some_steps_failed()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();
        let workspaces_dir = temp_path.join("workspaces");
        fs_err::create_dir_all(&workspaces_dir)?;

        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspaces_dir)
            .arg("new")
            .arg("--lib")
            .arg("failing_target");
        execute_command(&mut cmd, &environment, &workspaces_dir)?;

        let options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: workspaces_dir.join("failing_target").join("Cargo.toml"),
                }),
            }),
        };
        run_app(options, environment.clone()).await?;

        // Write a .cfe program with a command that is guaranteed not to exist in
        // environment.paths, so that execution fails at run time.
        let cfe_path = temp_path.join("failing.cfe");
        fs_err::write(
            &cfe_path,
            "select crates;\nfor crate {\n    run \"nonexistent_command_cargo_for_each_test\";\n}\n",
        )?;

        let options = Options {
            command: Command::Task(TaskParameters {
                sub_command: TaskSubCommand::Create(CreateTaskParameters {
                    name: "failing-task".to_string(),
                    program: cfe_path,
                    workspaces: vec![],
                    crates: vec![],
                }),
            }),
        };
        run_app(options, environment.clone()).await?;

        // Run with keep_going=true — must terminate and report SomeStepsFailed,
        // not loop forever (Bug 1) and not return CircularDependency (Bug 3).
        let options = Options {
            command: Command::Task(TaskParameters {
                sub_command: TaskSubCommand::Run(TaskRunParameters {
                    sub_command: TaskRunSubCommand::AllTargets(RunAllTargetsParameters {
                        name: "failing-task".to_string(),
                        jobs: None,
                        keep_going: true,
                    }),
                }),
            }),
        };
        let result = run_app(options, environment).await;

        assert!(
            matches!(result, Err(crate::error::Error::SomeStepsFailed)),
            "expected SomeStepsFailed with keep_going=true on a failing step, got {result:?}"
        );

        Ok(())
    }

    // ── ConfigLock ─────────────────────────────────────────────────────────────

    /// While a `ConfigLock` is held, a second `try_lock` on the same lock
    /// file path returns `WouldBlock`. Once the first guard is dropped, the
    /// second attempt succeeds.
    ///
    /// This is the property that makes concurrent `target add` invocations
    /// serialize: the second process blocks at `ConfigLock::acquire` until
    /// the first finishes its load → modify → save cycle.
    #[test]
    fn config_lock_blocks_concurrent_acquire() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;

        let lock_path = config_dir_path(&environment).join("cargo-for-each.lock");
        // First guard takes the lock.
        let guard = ConfigLock::acquire(&environment)?;

        // Open the same lock file with a separate handle and confirm we
        // cannot acquire the exclusive lock — i.e. the OS reports
        // contention, mirroring what a second process would see.
        let probe = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        match probe.file().try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => {
                // Expected: another holder already has the exclusive lock.
            }
            Ok(()) => panic!("expected try_lock to report WouldBlock while ConfigLock is held"),
            Err(std::fs::TryLockError::Error(e)) => {
                panic!("unexpected try_lock I/O error: {e}")
            }
        }

        // Drop the first guard and confirm the lock becomes acquirable.
        drop(guard);
        // The probe handle is still open; another try_lock must now succeed.
        match probe.file().try_lock() {
            Ok(()) => {
                // Expected: we got the lock.
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                panic!("lock should be available after the first guard was dropped")
            }
            Err(std::fs::TryLockError::Error(e)) => {
                panic!("unexpected try_lock I/O error: {e}")
            }
        }
        Ok(())
    }

    /// `ConfigLock::acquire` creates the config directory and the lock file
    /// on first use, so callers don't need to pre-create anything.
    #[test]
    fn config_lock_creates_lock_file_and_parent_dirs() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let lock_path = config_dir_path(&environment).join("cargo-for-each.lock");
        assert!(
            !lock_path.exists(),
            "precondition: lock file should not exist yet"
        );
        let _guard = ConfigLock::acquire(&environment)?;
        assert!(
            lock_path.exists(),
            "ConfigLock::acquire should have created the lock file"
        );
        Ok(())
    }

    /// Regression test for KNOWN_ISSUES.md §13: when a standalone crate is
    /// edited to become a multi-crate workspace, `target refresh` must both
    /// (a) pick up the new sibling members and (b) flip the workspace's
    /// `is_standalone` flag from `true` to `false` so filters like
    /// `select workspaces where !standalone` see the change.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_refresh_picks_up_standalone_to_workspace_transition()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        // 1. Create a standalone crate.
        let ws_dir = temp_path.join("ws");
        fs_err::create_dir_all(&ws_dir)?;
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&ws_dir)
            .args(["init", "--name", "ws", "--lib"]);
        execute_command(&mut cmd, &environment, &ws_dir)?;

        // 2. Register it via `target add`.
        let add_options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: ws_dir.join("Cargo.toml"),
                }),
            }),
        };
        run_app(add_options, environment.clone()).await?;

        let config_before = Config::load(&environment)?;
        let canonical_ws_dir = fs_err::canonicalize(&ws_dir)?;
        let ws_before = config_before
            .workspaces
            .iter()
            .find(|w| w.manifest_dir == canonical_ws_dir)
            .ok_or("workspace should be registered after target add")?;
        assert!(
            ws_before.is_standalone,
            "precondition: workspace should be registered as standalone"
        );

        // 3. Convert the standalone crate into a multi-crate workspace by
        //    rewriting Cargo.toml and creating sibling member crates.
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"sub_a\", \"sub_b\"]\nresolver = \"2\"\n",
        )?;
        for name in &["sub_a", "sub_b"] {
            let mut cmd = std::process::Command::new("cargo");
            cmd.current_dir(&ws_dir).args(["new", "--lib", name]);
            execute_command(&mut cmd, &environment, &ws_dir)?;
        }

        // 4. Run refresh.
        let refresh_options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Refresh,
            }),
        };
        run_app(refresh_options, environment.clone()).await?;

        // 5. Verify both transitions happened.
        let config_after = Config::load(&environment)?;
        let ws_entry = config_after
            .workspaces
            .iter()
            .find(|w| w.manifest_dir == canonical_ws_dir)
            .ok_or("workspace entry should still exist after refresh")?;
        assert!(
            !ws_entry.is_standalone,
            "is_standalone should have flipped to false after the transition"
        );

        let sub_a_dir = fs_err::canonicalize(ws_dir.join("sub_a"))?;
        let sub_b_dir = fs_err::canonicalize(ws_dir.join("sub_b"))?;
        assert!(
            config_after
                .crates
                .iter()
                .any(|c| c.manifest_dir == sub_a_dir),
            "sub_a should have been added to config.crates"
        );
        assert!(
            config_after
                .crates
                .iter()
                .any(|c| c.manifest_dir == sub_b_dir),
            "sub_b should have been added to config.crates"
        );
        Ok(())
    }

    /// Regression test for KNOWN_ISSUES.md §13 (opposite direction): when a
    /// multi-crate workspace is edited to drop a member (but the member's
    /// directory and Cargo.toml remain on disk), `target refresh` should
    /// re-register the orphan as its own standalone workspace so users
    /// don't silently lose it from filters. Symmetric to the
    /// standalone → workspace transition test above.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_refresh_re_registers_orphan_member_as_standalone()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        // 1. Create a multi-crate workspace with two members.
        let ws_dir = temp_path.join("multi");
        fs_err::create_dir_all(&ws_dir)?;
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"keep\", \"drop_me\"]\nresolver = \"2\"\n",
        )?;
        for name in &["keep", "drop_me"] {
            let mut cmd = std::process::Command::new("cargo");
            cmd.current_dir(&ws_dir).args(["new", "--lib", name]);
            execute_command(&mut cmd, &environment, &ws_dir)?;
        }

        // 2. Register the workspace.
        let add_options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: ws_dir.join("Cargo.toml"),
                }),
            }),
        };
        run_app(add_options, environment.clone()).await?;

        let canonical_ws_dir = fs_err::canonicalize(&ws_dir)?;
        let canonical_drop_me_dir = fs_err::canonicalize(ws_dir.join("drop_me"))?;
        let canonical_keep_dir = fs_err::canonicalize(ws_dir.join("keep"))?;

        // 3. Edit Cargo.toml to remove `drop_me` from members, but leave the
        //    drop_me directory (and its Cargo.toml) on disk — that's the
        //    case the orphan handling is meant for.
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"keep\"]\nresolver = \"2\"\n",
        )?;

        // 4. Run refresh.
        let refresh_options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Refresh,
            }),
        };
        run_app(refresh_options, environment.clone()).await?;

        // 5. Verify outcomes:
        //    - The kept member is still attached to its workspace.
        //    - The dropped member was re-registered as its own standalone
        //      workspace; its config.crates entry's workspace_manifest_dir
        //      now points at itself.
        let config_after = Config::load(&environment)?;

        let keep_entry = config_after
            .crates
            .iter()
            .find(|c| c.manifest_dir == canonical_keep_dir)
            .ok_or("keep should still be in config.crates")?;
        assert_eq!(
            keep_entry.workspace_manifest_dir, canonical_ws_dir,
            "keep should still belong to the multi workspace"
        );

        let orphan_entry = config_after
            .crates
            .iter()
            .find(|c| c.manifest_dir == canonical_drop_me_dir)
            .ok_or("drop_me should still be in config.crates (re-registered as standalone)")?;
        assert_eq!(
            orphan_entry.workspace_manifest_dir, canonical_drop_me_dir,
            "orphan's workspace_manifest_dir should point at itself after re-registration"
        );

        let orphan_ws = config_after
            .workspaces
            .iter()
            .find(|w| w.manifest_dir == canonical_drop_me_dir)
            .ok_or("a standalone workspace entry should exist for the orphan")?;
        assert!(
            orphan_ws.is_standalone,
            "newly created workspace entry for orphan should be is_standalone=true"
        );
        Ok(())
    }

    /// Companion to the above: if the orphan's directory was *also* removed
    /// from disk, refresh should drop the stale config.crates entry rather
    /// than try to re-register it.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_refresh_drops_orphan_when_its_cargo_toml_is_gone()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        let ws_dir = temp_path.join("multi");
        fs_err::create_dir_all(&ws_dir)?;
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"keep\", \"deleted\"]\nresolver = \"2\"\n",
        )?;
        for name in &["keep", "deleted"] {
            let mut cmd = std::process::Command::new("cargo");
            cmd.current_dir(&ws_dir).args(["new", "--lib", name]);
            execute_command(&mut cmd, &environment, &ws_dir)?;
        }

        let add_options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Add(AddParameters {
                    manifest_path: ws_dir.join("Cargo.toml"),
                }),
            }),
        };
        run_app(add_options, environment.clone()).await?;

        let canonical_deleted_dir = fs_err::canonicalize(ws_dir.join("deleted"))?;

        // Remove `deleted` from the workspace AND wipe its directory.
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"keep\"]\nresolver = \"2\"\n",
        )?;
        fs_err::remove_dir_all(ws_dir.join("deleted"))?;

        let refresh_options = Options {
            command: Command::Target(TargetParameters {
                sub_command: TargetSubCommand::Refresh,
            }),
        };
        run_app(refresh_options, environment.clone()).await?;

        let config_after = Config::load(&environment)?;
        assert!(
            !config_after
                .crates
                .iter()
                .any(|c| c.manifest_dir == canonical_deleted_dir),
            "the deleted-on-disk orphan should have been removed"
        );
        assert!(
            !config_after
                .workspaces
                .iter()
                .any(|w| w.manifest_dir == canonical_deleted_dir),
            "no new workspace entry should have been added for a directory that no longer exists"
        );
        Ok(())
    }
}
