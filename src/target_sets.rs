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
        TargetSet::Workspaces(params) => config
            .workspaces
            .iter()
            .filter(|w| !params.no_standalone || !w.is_standalone)
            .map(|w| w.manifest_dir.clone())
            .collect(),
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

    let target_manifest_paths_set: HashSet<PathBuf> =
        initial_manifest_dirs.iter().cloned().collect();

    // Collect all packages from all initial_manifest_dirs into a single map
    let mut all_packages: HashMap<PackageId, cargo_metadata::Package> = HashMap::new();
    let mut package_name_to_id: HashMap<String, PackageId> = HashMap::new();

    for manifest_dir in &initial_manifest_dirs {
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(manifest_dir.join("Cargo.toml"))
            .exec()
            .map_err(|e| Error::CargoMetadataError(manifest_dir.clone(), e))?;

        for package in metadata.packages {
            all_packages.insert(package.id.clone(), package.clone());
            package_name_to_id.insert(package.name.to_string(), package.id.clone());
        }
    }

    let mut targets: Vec<Target> = Vec::new();

    for manifest_dir in &initial_manifest_dirs {
        // Find the package corresponding to the current manifest_dir
        let current_package_id = package_name_to_id
            .iter()
            .find_map(|(_name, id)| {
                let package = all_packages.get(id)?;
                // Compare canonicalized paths to avoid issues with different path representations
                let package_manifest_dir = package
                    .manifest_path
                    .parent()
                    .ok_or_else(|| {
                        Error::ManifestPathHasNoParentDir(
                            package.manifest_path.clone().into_std_path_buf(),
                        )
                    })
                    .ok()?; // Use ok() to turn into Option for find_map
                let canonical_package_manifest_dir =
                    fs_err::canonicalize(package_manifest_dir).ok()?; // Use ok()

                let canonical_manifest_dir = fs_err::canonicalize(manifest_dir.clone()).ok()?; // Use ok()

                if canonical_package_manifest_dir == canonical_manifest_dir {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(manifest_dir.clone())
            })?;

        let current_package = all_packages.get(&current_package_id).ok_or_else(|| {
            Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(manifest_dir.clone())
        })?; // Should not happen if current_package_id was found

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

                if target_manifest_paths_set.contains(&dep_manifest_path) {
                    dependencies.push(dep_manifest_path);
                }
            }
        }
        targets.push(Target {
            manifest_dir: manifest_dir.clone(),
            dependencies,
        });
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
