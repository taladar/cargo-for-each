use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::Error;
use crate::targets::{CargoMetadataExt as _, CrateType};
use crate::{Crate, Workspace};
use tracing::instrument;

/// Parameters for listing crates
#[derive(clap::Parser, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrateListParameters {
    /// only list crates of this type
    #[clap(long)]
    pub r#type: Option<CrateType>,
    /// only list crates that are standalone or not
    #[clap(long)]
    pub standalone: Option<bool>,
}

/// Parameters for listing workspaces
#[derive(clap::Parser, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceListParameters {
    /// only list multi-crate workspaces
    #[clap(long)]
    pub no_standalone: bool,
}

/// The type of object to list
#[derive(clap::Parser, Debug, Clone)]
pub enum ListType {
    /// list workspaces
    Workspaces(WorkspaceListParameters),
    /// list crates
    Crates(CrateListParameters),
}

/// Parameters for list subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct ListParameters {
    /// the type of object to list
    #[clap(subcommand)]
    pub list_type: ListType,
}

/// Parameters for add subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct AddParameters {
    /// the manifest file to add, if it refers to a workspace manifest all crates in the workspace are added too
    #[clap(long)]
    pub manifest_path: PathBuf,
}

/// Parameters for remove subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct RemoveParameters {
    /// the manifest file to remove
    #[clap(long)]
    pub manifest_path: PathBuf,
}

/// implementation of the list subcommand
///
/// # Errors
///
/// fails if the implementation of list fails
#[instrument]
pub async fn list_command(list_parameters: ListParameters) -> Result<(), Error> {
    #[expect(clippy::print_stderr, reason = "This is part of the UI, not logging")]
    let Ok(config) = crate::Config::load() else {
        eprintln!("No config file found, nothing to list");
        return Ok(());
    };
    #[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
    match list_parameters.list_type {
        ListType::Workspaces(params) => {
            for workspace in config.workspaces {
                if params.no_standalone && workspace.is_standalone {
                    continue;
                }
                println!(
                    "{} (standalone: {})",
                    workspace.manifest_dir.display(),
                    workspace.is_standalone
                );
            }
        }
        ListType::Crates(params) => {
            let workspace_standalone_map: HashMap<_, _> = config
                .workspaces
                .iter()
                .map(|w| (w.manifest_dir.clone(), w.is_standalone))
                .collect();

            for krate in config.crates {
                if let Some(crate_type) = &params.r#type
                    && !krate.types.contains(crate_type)
                {
                    continue;
                }
                if let Some(standalone) = params.standalone
                    && workspace_standalone_map
                        .get(&krate.workspace_manifest_dir)
                        .is_none_or(|&is_standalone| is_standalone != standalone)
                {
                    continue;
                }
                if krate.manifest_dir == krate.workspace_manifest_dir {
                    println!(
                        "{} (types: {:?})",
                        krate.manifest_dir.display(),
                        krate.types
                    );
                } else {
                    println!(
                        "{} (workspace: {}, types: {:?})",
                        krate.manifest_dir.display(),
                        krate.workspace_manifest_dir.display(),
                        krate.types
                    );
                }
            }
        }
    }
    Ok(())
}

/// implementation of the add subcommand
///
/// # Errors
///
/// fails if the implementation of add fails
#[instrument]
pub async fn add_command(add_parameters: AddParameters) -> Result<(), Error> {
    let mut config = crate::Config::load()?;
    let manifest_path =
        std::path::absolute(add_parameters.manifest_path.clone()).map_err(|err| {
            Error::CouldNotDetermineAbsoluteManifestPath(add_parameters.manifest_path, err)
        })?;
    let manifest_path = fs_err::canonicalize(manifest_path.clone())
        .map_err(|err| Error::CouldNotDetermineCanonicalManifestPath(manifest_path, err))?;

    // first call to metadata to find the workspace root
    let initial_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&manifest_path)
        .exec()
        .map_err(|err| Error::CargoMetadataError(manifest_path.clone(), err))?; // manifest_path here is already std::path::PathBuf
    let workspace_manifest_path_camino = initial_metadata.workspace_root.join("Cargo.toml");

    let Some(workspace_manifest_dir_camino) = workspace_manifest_path_camino.parent() else {
        return Err(Error::ManifestPathHasNoParentDir(
            workspace_manifest_path_camino.into_std_path_buf(),
        ));
    };
    let workspace_manifest_dir_camino = workspace_manifest_dir_camino.to_path_buf();

    // second call to metadata to get all packages in the workspace
    let workspace_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&workspace_manifest_path_camino)
        .exec()
        .map_err(|err| {
            Error::CargoMetadataError(
                workspace_manifest_path_camino.clone().into_std_path_buf(),
                err,
            )
        })?;

    let is_standalone = if let [package_id] = workspace_metadata.workspace_members.as_slice() {
        let package = workspace_metadata.get_package_by_id(package_id)?;
        package.manifest_path == workspace_manifest_path_camino
    } else {
        false
    };

    if is_standalone {
        tracing::debug!("Identified Cargo.toml as standalone crate");
        let package = workspace_metadata
            .get_package_by_manifest_path(&workspace_manifest_path_camino.into_std_path_buf())?; // Convert for comparison
        let crate_types = CrateType::from_package(package);
        config.add_workspace(Workspace {
            manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
            is_standalone: true,
        });
        config.add_crate(Crate {
            manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
            workspace_manifest_dir: workspace_manifest_dir_camino.into_std_path_buf(),
            types: crate_types,
        });
    } else {
        tracing::debug!("Identified Cargo.toml as workspace");
        config.add_workspace(Workspace {
            manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
            is_standalone: false,
        });
        for package_id in workspace_metadata.workspace_members.clone() {
            let package = workspace_metadata.get_package_by_id(&package_id)?;
            let package_manifest_path = package.manifest_path.to_owned().into_std_path_buf();
            let Some(package_manifest_dir) = package_manifest_path.parent() else {
                return Err(Error::ManifestPathHasNoParentDir(package_manifest_path));
            };
            let crate_types = CrateType::from_package(package);
            config.add_crate(Crate {
                manifest_dir: package_manifest_dir.to_path_buf(),
                workspace_manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
                types: crate_types,
            });
        }
    }

    config.save()?;

    Ok(())
}

/// implementation of the remove subcommand
///
/// # Errors
///
/// fails if the implementation of remove fails
#[instrument]
pub async fn remove_command(remove_parameters: RemoveParameters) -> Result<(), Error> {
    let mut config = crate::Config::load()?;
    let manifest_path =
        std::path::absolute(remove_parameters.manifest_path.clone()).map_err(|err| {
            Error::CouldNotDetermineAbsoluteManifestPath(remove_parameters.manifest_path, err)
        })?;
    let manifest_path = fs_err::canonicalize(manifest_path.clone())
        .map_err(|err| Error::CouldNotDetermineCanonicalManifestPath(manifest_path, err))?;

    // Filter out the workspace if it matches the manifest_path
    let initial_workspace_count = config.workspaces.len();
    config
        .workspaces
        .retain(|w| w.manifest_dir != manifest_path);
    if config.workspaces.len() < initial_workspace_count {
        tracing::debug!("Removed workspace at {}", manifest_path.display());
    } else {
        tracing::warn!("No workspace found at {}", manifest_path.display());
    }

    // Filter out crates that match the manifest_path or belong to the removed workspace
    let initial_crate_count = config.crates.len();
    config
        .crates
        .retain(|c| c.manifest_dir != manifest_path && c.workspace_manifest_dir != manifest_path);
    if config.crates.len() < initial_crate_count {
        tracing::debug!("Removed crates associated with {}", manifest_path.display());
    } else {
        tracing::warn!(
            "No crates found associated with {}",
            manifest_path.display()
        );
    }

    config.save()?;
    Ok(())
}

/// implementation of the refresh subcommand
///
/// # Errors
///
/// fails if the implementation of refresh fails
#[instrument]
pub async fn refresh_command() -> Result<(), Error> {
    let mut config = crate::Config::load()?;

    // 1. Remove workspaces that no longer exist.
    let (retained_workspaces, removed_workspaces): (Vec<_>, Vec<_>) = config
        .workspaces
        .drain(..)
        .partition(|w| w.manifest_dir.join("Cargo.toml").is_file());
    for r in &removed_workspaces {
        tracing::debug!(
            "Removing workspace at {} because Cargo.toml is gone.",
            r.manifest_dir.display()
        );
    }
    config.workspaces = retained_workspaces;

    // 2. Remove crates that no longer exist.
    let (retained_crates, removed_crates): (Vec<_>, Vec<_>) = config
        .crates
        .drain(..)
        .partition(|c| c.manifest_dir.join("Cargo.toml").is_file());
    for r in &removed_crates {
        tracing::debug!(
            "Removing crate at {} because Cargo.toml is gone.",
            r.manifest_dir.display()
        );
    }
    config.crates = retained_crates;

    // 3. For all existing workspaces, discover and add new member crates.
    //    We don't need to update existing crates found here, as the next step will do it.
    let workspaces_to_scan = config.workspaces.clone();
    for workspace in &workspaces_to_scan {
        let manifest_path = workspace.manifest_dir.join("Cargo.toml");
        let cargo_metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .exec()
            .map_err(|err| Error::CargoMetadataError(manifest_path, err))?;

        for package_id in &cargo_metadata.workspace_members {
            let package = cargo_metadata.get_package_by_id(package_id)?;
            let pkg_manifest_path = package.manifest_path.to_owned().into_std_path_buf();
            if let Some(manifest_dir) = pkg_manifest_path.parent() {
                let manifest_dir = manifest_dir.to_path_buf();

                // Only add if it doesn't exist. `add_crate` does this.
                if !config.crates.iter().any(|c| c.manifest_dir == manifest_dir) {
                    let crate_types = CrateType::from_package(package);
                    config.add_crate(Crate {
                        manifest_dir,
                        workspace_manifest_dir: workspace.manifest_dir.clone(),
                        types: crate_types,
                    });
                }
            }
        }
    }

    // 4. Update crate_types for all existing crates.
    for krate in &mut config.crates {
        let manifest_path = krate.manifest_dir.join("Cargo.toml");

        let cargo_metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .no_deps()
            .exec()
            .map_err(|err| Error::CargoMetadataError(manifest_path.clone(), err))?;

        // We need the package object to determine the crate type.
        // Using get_package_by_manifest_path is correct for single crates/workspace members.
        if let Ok(package) = cargo_metadata.get_package_by_manifest_path(&manifest_path) {
            let new_crate_types = CrateType::from_package(package);
            if krate.types != new_crate_types {
                tracing::debug!(
                    "Updating types for {} from {:?} to {:?}",
                    krate.manifest_dir.display(),
                    krate.types,
                    new_crate_types
                );
                krate.types = new_crate_types;
            }
        } else {
            tracing::warn!(
                "Could not find package for manifest path {} during refresh.",
                manifest_path.display()
            );
        }
    }

    config.save()?;
    Ok(())
}

/// The type of object to target
#[derive(clap::Parser, Debug, Clone)]
pub enum TargetType {
    /// List workspaces and crates managed by cargo-for-each.
    List(ListParameters),
    /// Add a workspace or crate to be managed by cargo-for-each.
    Add(AddParameters),
    /// Remove a workspace or crate managed by cargo-for-each.
    Remove(RemoveParameters),
    /// Refresh the list of workspaces and crates managed by cargo-for-each, removing deleted entries and adding new ones.
    Refresh,
}

/// Parameters for target subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct TargetParameters {
    /// The type of object to target
    #[clap(subcommand)]
    pub target_type: TargetType,
}

/// implementation of the target subcommand
///
/// # Errors
///
/// fails if the implementation of target fails
#[instrument]
pub async fn target_command(target_parameters: TargetParameters) -> Result<(), Error> {
    match target_parameters.target_type {
        TargetType::List(list_parameters) => {
            list_command(list_parameters).await?;
        }
        TargetType::Add(add_parameters) => {
            add_command(add_parameters).await?;
        }
        TargetType::Remove(remove_parameters) => {
            remove_command(remove_parameters).await?;
        }
        TargetType::Refresh => {
            refresh_command().await?;
        }
    }
    Ok(())
}
