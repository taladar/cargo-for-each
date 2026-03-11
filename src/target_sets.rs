//! This module defines the structures and functions for managing target sets,
//! including their resolution logic.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use cargo_metadata::PackageId; // Used by resolve_target_set
use tracing::instrument;
// fs_err; // Used by resolve_target_set and load_target_set - no, fs_err is not needed here explicitly
// as its methods are used with qualified names.

use crate::error::Error;
use crate::targets::Target; // Target and CrateType will be used
use clap::Parser;

/// represents a resolved target set
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedTargetSet {
    /// the targets of the resolved target set
    pub targets: Vec<Target>,
}

/// an enum that describes a set of targets
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, clap::Parser)]
pub enum TargetSet {
    /// a set of crates
    Crates(crate::targets::CrateFilterParameters),
    /// a set of workspaces
    Workspaces(crate::targets::WorkspaceFilterParameters),
}

/// resolves a target set to a list of manifest directories with dependencies
///
/// # Errors
///
/// Returns an error if `cargo metadata` fails for any manifest path,
/// if a package is not found in `cargo metadata` output,
/// if a manifest path has no parent directory, or if canonicalization fails.
pub fn resolve_target_set(
    target_set: &TargetSet,
    config: &crate::Config,
) -> Result<ResolvedTargetSet, Error> {
    let initial_manifest_dirs: Vec<PathBuf> = match target_set {
        TargetSet::Workspaces(params) => {
            let mut manifest_dirs = Vec::new();
            for w in config
                .workspaces
                .iter()
                .filter(|w| !params.no_standalone || !w.is_standalone)
            {
                manifest_dirs.push(w.manifest_dir.clone());
            }
            manifest_dirs
        }
        TargetSet::Crates(params) => {
            let workspace_standalone_map: HashMap<_, _> = config
                .workspaces
                .iter()
                .map(|w| (w.manifest_dir.clone(), w.is_standalone))
                .collect();
            config
                .crates
                .iter()
                .filter(|krate| {
                    if let Some(t) = &params.r#type
                        && !krate.types.contains(t)
                    {
                        return false;
                    }
                    if let Some(standalone) = params.standalone
                        && workspace_standalone_map
                            .get(&krate.workspace_manifest_dir)
                            .is_none_or(|&is_standalone| is_standalone != standalone)
                    {
                        return false;
                    }
                    true
                })
                .map(|c| c.manifest_dir.clone())
                .collect()
        }
    };

    let mut targets: Vec<Target> = Vec::new();

    match target_set {
        TargetSet::Workspaces(_) => {
            // If it's a Workspace target set, the initial_manifest_dirs are already the targets
            for manifest_dir in initial_manifest_dirs {
                let canonical_manifest_dir = fs_err::canonicalize(&manifest_dir).map_err(|e| {
                    Error::CouldNotDetermineCanonicalManifestPath(manifest_dir.clone(), e)
                })?;
                targets.push(Target {
                    manifest_dir: canonical_manifest_dir,
                    dependencies: Vec::new(), // Workspaces don't have dependencies in this context
                });
            }
        }
        TargetSet::Crates(_) => {
            // For Crates, we need to collect metadata and resolve dependencies
            let mut all_packages: HashMap<PackageId, cargo_metadata::Package> = HashMap::new();
            let mut package_name_to_id: HashMap<String, PackageId> = HashMap::new();

            let unique_workspace_roots: HashSet<PathBuf> = config
                .workspaces
                .iter()
                .map(|w| w.manifest_dir.clone())
                .collect();

            for workspace_root in &unique_workspace_roots {
                let manifest_path_for_metadata = workspace_root.join("Cargo.toml");
                let metadata = cargo_metadata::MetadataCommand::new()
                    .manifest_path(&manifest_path_for_metadata)
                    .no_deps()
                    .exec()
                    .map_err(|e| Error::CargoMetadataError(workspace_root.clone(), e))?;

                for package in metadata.packages {
                    all_packages.insert(package.id.clone(), package.clone());
                    package_name_to_id.insert(package.name.to_string(), package.id.clone());
                }
            }

            // Build a set of canonical paths so dependency lookups match regardless of symlinks
            let target_canonical_paths_set: HashSet<PathBuf> = initial_manifest_dirs
                .iter()
                .map(|d| {
                    fs_err::canonicalize(d)
                        .map_err(|e| Error::CouldNotDetermineCanonicalManifestPath(d.clone(), e))
                })
                .collect::<Result<HashSet<PathBuf>, Error>>()?;

            for manifest_dir in initial_manifest_dirs {
                // Compute canonical form once; reuse in the package search and when storing
                let canonical_manifest_dir = fs_err::canonicalize(&manifest_dir).map_err(|e| {
                    Error::CouldNotDetermineCanonicalManifestPath(manifest_dir.clone(), e)
                })?;

                let current_package_id = package_name_to_id
                    .iter()
                    .find_map(|(_name, id)| {
                        let package = all_packages.get(id)?;
                        let package_manifest_dir = package
                            .manifest_path
                            .parent()
                            .ok_or_else(|| {
                                Error::ManifestPathHasNoParentDir(
                                    package.manifest_path.clone().into_std_path_buf(),
                                )
                            })
                            .ok()?;
                        let canonical_package_manifest_dir =
                            fs_err::canonicalize(package_manifest_dir).ok()?;

                        if canonical_package_manifest_dir == canonical_manifest_dir {
                            Some(id.clone())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| {
                        Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(
                            manifest_dir.clone(),
                        )
                    })?;

                let current_package = all_packages.get(&current_package_id).ok_or_else(|| {
                    Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(manifest_dir.clone())
                })?;

                let mut dependencies: Vec<PathBuf> = Vec::new();

                for dep in &current_package.dependencies {
                    if let Some(dep_package_id) = package_name_to_id.get(&dep.name)
                        && let Some(dep_package) = all_packages.get(dep_package_id)
                    {
                        let dep_manifest_path = dep_package
                            .manifest_path
                            .parent()
                            .ok_or_else(|| {
                                Error::ManifestPathHasNoParentDir(
                                    dep_package.manifest_path.clone().into_std_path_buf(),
                                )
                            })?
                            .canonicalize()
                            .map_err(|e| {
                                Error::CouldNotDetermineCanonicalManifestPath(
                                    dep_package.manifest_path.clone().into_std_path_buf(),
                                    e,
                                )
                            })?;

                        if target_canonical_paths_set.contains(&dep_manifest_path) {
                            dependencies.push(dep_manifest_path);
                        }
                    }
                }
                targets.push(Target {
                    manifest_dir: canonical_manifest_dir,
                    dependencies,
                });
            }
        }
    }
    Ok(ResolvedTargetSet { targets })
}

/// returns the target sets dir path
///
/// # Errors
///
/// Returns an error if the config directory path cannot be determined.
pub fn dir_path(environment: &crate::Environment) -> Result<PathBuf, Error> {
    Ok(crate::config_dir_path(environment)?.join("target-sets"))
}

/// loads a target set from a file
///
/// # Errors
///
/// Returns an error if the configuration directory cannot be determined, if the target set file cannot be checked for existence, read, or parsed, or if the target set is not found.
pub fn load_target_set(name: &str, environment: &crate::Environment) -> Result<TargetSet, Error> {
    let target_set_path = dir_path(environment)?.join(format!("{name}.toml"));
    if fs_err::exists(&target_set_path)
        .map_err(|e| Error::CouldNotCheckTargetSetExistence(target_set_path.clone(), e))?
    {
        let file_content =
            fs_err::read_to_string(&target_set_path).map_err(Error::CouldNotReadConfigFile)?;
        toml::from_str(&file_content).map_err(Error::CouldNotParseConfigFile)
    } else {
        Err(Error::TargetSetNotFound(name.to_string()))
    }
}

/// Subcommands for the target-set command
#[derive(Parser, Debug, Clone)]
pub enum TargetSetSubCommand {
    /// List existing target sets
    List,
    /// Create a new target set
    Create(CreateTargetSetParameters),
    /// Remove a target set
    Remove(RemoveTargetSetParameters),
    /// Describe a target set by listing its resolved targets (crates/workspaces)
    Describe(DescribeTargetSetParameters),
}

/// Parameters for describing a target set
#[derive(Parser, Debug, Clone)]
pub struct DescribeTargetSetParameters {
    /// the name of the target set to describe
    #[clap(long)]
    pub name: String,
}

/// Parameters for target-set subcommand
#[derive(Parser, Debug, Clone)]
pub struct TargetSetParameters {
    /// the `target-set` subcommand to run
    #[clap(subcommand)]
    pub sub_command: TargetSetSubCommand,
}

/// implementation of the target-set subcommand
///
/// # Errors
///
/// This command can fail due to errors in its subcommands (list, create, remove), such as issues with configuration, file system operations, or if a target set is not found.
#[instrument]
pub async fn target_set_command(
    target_set_parameters: TargetSetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    match target_set_parameters.sub_command {
        TargetSetSubCommand::List => {
            list_command(environment).await?;
        }
        TargetSetSubCommand::Create(create_parameters) => {
            create_command(create_parameters, environment).await?;
        }
        TargetSetSubCommand::Remove(remove_parameters) => {
            remove_command(remove_parameters, environment).await?;
        }
        TargetSetSubCommand::Describe(describe_parameters) => {
            describe_command(describe_parameters, environment).await?;
        }
    }
    Ok(())
}

/// implementation of the target-set describe subcommand
///
/// # Errors
///
/// This command can fail if the configuration cannot be loaded, if the target set is not found,
/// or if there are issues resolving the target set to actual targets.
#[instrument]
pub async fn describe_command(
    describe_parameters: DescribeTargetSetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let config = crate::Config::load(&environment)?;
    let target_set = load_target_set(&describe_parameters.name, &environment)?;
    let resolved_target_set = resolve_target_set(&target_set, &config)?;

    #[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
    for target in resolved_target_set.targets {
        println!("{}", target.manifest_dir.display());
    }

    Ok(())
}

/// implementation of the target-set list subcommand
///
/// # Errors
///
/// This command can fail if the configuration directory cannot be determined, or if there are issues reading the target sets directory or individual target set files.
pub async fn list_command(environment: crate::Environment) -> Result<(), Error> {
    let target_sets_dir = dir_path(&environment)?;
    if !target_sets_dir.exists() {
        return Ok(());
    }
    for entry in fs_err::read_dir(target_sets_dir).map_err(Error::CouldNotReadTargetSetsDir)? {
        let entry = entry.map_err(Error::CouldNotReadTargetSetsDir)?;
        let path = entry.path();
        #[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
        if path.is_file()
            && let Some(extension) = path.extension()
            && extension == "toml"
            && let Some(name) = path.file_stem()
        {
            println!("{}", name.to_string_lossy());
            let content = fs_err::read_to_string(&path).map_err(Error::CouldNotReadConfigFile)?;
            println!("{content}");
        }
    }
    Ok(())
}

/// Parameters for creating a new target set
#[derive(Parser, Debug, Clone)]
pub struct CreateTargetSetParameters {
    /// the name of the target set
    #[clap(long)]
    pub name: String,
    /// the type of target set to create
    #[clap(subcommand)]
    pub target_set: TargetSet,
}

/// implementation of the target-set create subcommand
///
/// # Errors
///
/// This command can fail if the configuration directory cannot be determined, if a target set with the given name already exists, if parent directories for the target set file cannot be created, if the target set cannot be serialized, or if the target set file cannot be written.
pub async fn create_command(
    create_parameters: CreateTargetSetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let target_set = create_parameters.target_set;
    let target_set_path = dir_path(&environment)?.join(format!("{}.toml", create_parameters.name));
    if target_set_path.exists() {
        return Err(Error::AlreadyExists(format!(
            "target set {}",
            create_parameters.name
        )));
    }
    if let Some(target_set_dir_path) = target_set_path.parent() {
        fs_err::create_dir_all(target_set_dir_path)
            .map_err(Error::CouldNotCreateTargetSetFileParentDirs)?;
    }
    fs_err::write(
        &target_set_path,
        toml::to_string(&target_set).map_err(Error::CouldNotSerializeTargetSetFile)?,
    )
    .map_err(Error::CouldNotWriteTargetSetFile)?;
    Ok(())
}

/// Parameters for deleting a target set
#[derive(Parser, Debug, Clone)]
pub struct RemoveTargetSetParameters {
    /// the name of the target set
    #[clap(long)]
    pub name: String,
}

/// implementation of the target-set remove subcommand
///
/// # Errors
///
/// This command can fail if the configuration directory cannot be determined, or if the target set file cannot be removed.
pub async fn remove_command(
    remove_parameters: RemoveTargetSetParameters,
    environment: crate::Environment,
) -> Result<(), Error> {
    let target_set_path = dir_path(&environment)?.join(format!("{}.toml", remove_parameters.name));
    fs_err::remove_file(target_set_path).map_err(Error::CouldNotRemoveTargetSetFile)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Environment, targets::WorkspaceFilterParameters, utils::execute_command};
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_describe_target_set_workspaces() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();
        let workspaces_dir = temp_path.join("workspaces");
        fs_err::create_dir_all(&workspaces_dir)?;

        let workspace_root_dir = workspaces_dir.join("workspace_root");
        fs_err::create_dir_all(&workspace_root_dir)?;
        fs_err::write(
            workspace_root_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member1\", \"member2\"]\nresolver = \"2\"\n",
        )?;

        let _member1_dir = workspace_root_dir.join("member1");
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace_root_dir)
            .arg("new")
            .arg("--lib")
            .arg("member1");
        execute_command(&mut cmd, &environment, &workspace_root_dir)?;

        let _member2_dir = workspace_root_dir.join("member2");
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace_root_dir)
            .arg("new")
            .arg("--bin")
            .arg("member2");
        execute_command(&mut cmd, &environment, &workspace_root_dir)?;

        let options = crate::Options {
            command: crate::Command::Target(crate::targets::TargetParameters {
                sub_command: crate::targets::TargetSubCommand::Add(crate::targets::AddParameters {
                    manifest_path: workspace_root_dir.join("Cargo.toml"),
                }),
            }),
        };
        crate::run_app(options, environment.clone()).await?;

        let options = crate::Options {
            command: crate::Command::TargetSet(TargetSetParameters {
                sub_command: TargetSetSubCommand::Create(CreateTargetSetParameters {
                    name: "test-describe-set".to_string(),
                    target_set: TargetSet::Workspaces(WorkspaceFilterParameters {
                        no_standalone: false,
                    }),
                }),
            }),
        };
        crate::run_app(options, environment.clone()).await?;

        let cargo_for_each_bin_path = std::env::current_dir()?.join("target/debug/cargo-for-each"); // Use debug for tests

        let mut command = std::process::Command::new(&cargo_for_each_bin_path);
        command
            .env("XDG_CONFIG_HOME", &environment.config_dir) // Set XDG_CONFIG_HOME for the external command
            .arg("target-set")
            .arg("describe")
            .arg("--name")
            .arg("test-describe-set");

        let output = crate::utils::execute_command(&mut command, &environment, temp_path)?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(stdout.contains(&workspace_root_dir.to_string_lossy().to_string()));
        assert!(!stdout.contains(&_member1_dir.to_string_lossy().to_string()));
        assert!(!stdout.contains(&_member2_dir.to_string_lossy().to_string()));

        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_target_set_workspaces_with_sub_crates()
    -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory for the test environment
        let temp_dir = tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        // 1. Setup workspace with sub-crates
        let workspace_root_dir = temp_path.join("workspace_root");
        fs_err::create_dir_all(&workspace_root_dir)?;
        let workspace_cargo_toml = workspace_root_dir.join("Cargo.toml");
        fs_err::write(
            &workspace_cargo_toml,
            "[workspace]\nmembers = [\"member1\", \"member2\"]\nresolver = \"2\"\n",
        )?;

        let _member1_dir = workspace_root_dir.join("member1");
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace_root_dir)
            .arg("new")
            .arg("--lib")
            .arg("member1");
        execute_command(&mut cmd, &environment, &workspace_root_dir)?;

        let _member2_dir = workspace_root_dir.join("member2");
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&workspace_root_dir)
            .arg("new")
            .arg("--bin")
            .arg("member2");
        execute_command(&mut cmd, &environment, &workspace_root_dir)?;

        // Instead of manually creating Config and adding workspace, use `add_command`
        let options = crate::Options {
            command: crate::Command::Target(crate::targets::TargetParameters {
                sub_command: crate::targets::TargetSubCommand::Add(crate::targets::AddParameters {
                    manifest_path: workspace_cargo_toml.clone(),
                }),
            }),
        };
        crate::run_app(options, environment.clone()).await?;

        // 2. Load the config (now properly populated by `add_command`)
        let config = crate::Config::load(&environment)?;

        // 3. Call resolve_target_set
        let target_set = TargetSet::Workspaces(WorkspaceFilterParameters {
            no_standalone: false,
        });
        let resolved_target_set = resolve_target_set(&target_set, &config)?;

        // 4. Assertions: Expect only the workspace root
        assert_eq!(resolved_target_set.targets.len(), 1);

        let found_workspace_root = resolved_target_set
            .targets
            .iter()
            .any(|target| target.manifest_dir == workspace_root_dir);

        assert!(
            found_workspace_root,
            "Did not find target for workspace root"
        );

        // Regression test for Bug 4: all stored manifest_dirs must already be
        // in canonical form so that target_map lookups match dependency paths.
        for target in &resolved_target_set.targets {
            let canonical = fs_err::canonicalize(&target.manifest_dir)?;
            assert_eq!(
                target.manifest_dir, canonical,
                "manifest_dir should be stored in canonical form"
            );
        }

        Ok(())
    }

    /// Resolving a Crates target set stores canonical `manifest_dir` values and
    /// records intra-workspace dependencies correctly (Bug 4 regression test).
    #[tokio::test]
    async fn test_resolve_crates_target_set_stores_canonical_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        // Build a workspace with two member crates.
        let workspace_root_dir = temp_path.join("ws");
        fs_err::create_dir_all(&workspace_root_dir)?;
        fs_err::write(
            workspace_root_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate_a\", \"crate_b\"]\nresolver = \"2\"\n",
        )?;

        for name in &["crate_a", "crate_b"] {
            let mut cmd = std::process::Command::new("cargo");
            cmd.current_dir(&workspace_root_dir)
                .arg("new")
                .arg("--lib")
                .arg(name);
            execute_command(&mut cmd, &environment, &workspace_root_dir)?;
        }

        // Register workspace via the add command.
        let options = crate::Options {
            command: crate::Command::Target(crate::targets::TargetParameters {
                sub_command: crate::targets::TargetSubCommand::Add(crate::targets::AddParameters {
                    manifest_path: workspace_root_dir.join("Cargo.toml"),
                }),
            }),
        };
        crate::run_app(options, environment.clone()).await?;

        let config = crate::Config::load(&environment)?;
        let target_set = TargetSet::Crates(crate::targets::CrateFilterParameters {
            r#type: None,
            standalone: None,
        });
        let resolved = resolve_target_set(&target_set, &config)?;

        assert_eq!(resolved.targets.len(), 2, "expected two crate targets");

        // Every manifest_dir must equal its own canonicalized form.
        for target in &resolved.targets {
            let canonical = fs_err::canonicalize(&target.manifest_dir)?;
            assert_eq!(
                target.manifest_dir,
                canonical,
                "manifest_dir '{}' is not canonical",
                target.manifest_dir.display()
            );
            // All stored dependency paths must also be canonical.
            for dep in &target.dependencies {
                let canonical_dep = fs_err::canonicalize(dep)?;
                assert_eq!(
                    dep,
                    &canonical_dep,
                    "dependency path '{}' is not canonical",
                    dep.display()
                );
            }
        }

        Ok(())
    }
}
