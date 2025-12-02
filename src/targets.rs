//! This module defines the core data structures and traits related to targets (workspaces and crates).
//! It includes extensions for `cargo_metadata` and the `Target` struct itself.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cargo_metadata::PackageId;

use std::collections::HashMap;

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
pub async fn list_command(
    list_parameters: ListParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    #[expect(clippy::print_stderr, reason = "This is part of the UI, not logging")]
    let Ok(config) = crate::Config::load(&environment) else {
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
pub async fn add_command(
    add_parameters: AddParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    let mut config = crate::Config::load(&environment)?;
    let manifest_path =
        std::path::absolute(add_parameters.manifest_path.clone()).map_err(|err| {
            crate::error::Error::CouldNotDetermineAbsoluteManifestPath(
                add_parameters.manifest_path,
                err,
            )
        })?;
    let manifest_path = fs_err::canonicalize(manifest_path.clone()).map_err(|err| {
        crate::error::Error::CouldNotDetermineCanonicalManifestPath(manifest_path, err)
    })?;

    // first call to metadata to find the workspace root
    let initial_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&manifest_path)
        .exec()
        .map_err(|err| crate::error::Error::CargoMetadataError(manifest_path.clone(), err))?; // manifest_path here is already std::path::PathBuf
    let workspace_manifest_path_camino = initial_metadata.workspace_root.join("Cargo.toml");

    let Some(workspace_manifest_dir_camino) = workspace_manifest_path_camino.parent() else {
        return Err(crate::error::Error::ManifestPathHasNoParentDir(
            workspace_manifest_path_camino.into_std_path_buf(),
        ));
    };
    let workspace_manifest_dir_camino = workspace_manifest_dir_camino.to_path_buf();

    // second call to metadata to get all packages in the workspace
    let workspace_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&workspace_manifest_path_camino)
        .exec()
        .map_err(|err| {
            crate::error::Error::CargoMetadataError(
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
                return Err(crate::error::Error::ManifestPathHasNoParentDir(
                    package_manifest_path,
                ));
            };
            let crate_types = CrateType::from_package(package);
            config.add_crate(Crate {
                manifest_dir: package_manifest_dir.to_path_buf(),
                workspace_manifest_dir: workspace_manifest_dir_camino.clone().into_std_path_buf(),
                types: crate_types,
            });
        }
    }

    config.save(&environment)?;

    Ok(())
}

/// implementation of the remove subcommand
///
/// # Errors
///
/// fails if the implementation of remove fails
#[instrument]
pub async fn remove_command(
    remove_parameters: RemoveParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    let mut config = crate::Config::load(&environment)?;
    let manifest_path =
        std::path::absolute(remove_parameters.manifest_path.clone()).map_err(|err| {
            crate::error::Error::CouldNotDetermineAbsoluteManifestPath(
                remove_parameters.manifest_path,
                err,
            )
        })?;
    let manifest_path = fs_err::canonicalize(manifest_path.clone()).map_err(|err| {
        crate::error::Error::CouldNotDetermineCanonicalManifestPath(manifest_path, err)
    })?;

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

    config.save(&environment)?;
    Ok(())
}

/// implementation of the refresh subcommand
///
/// # Errors
///
/// fails if the implementation of refresh fails
#[instrument]
pub async fn refresh_command(environment: crate::Environment) -> Result<(), crate::error::Error> {
    let mut config = crate::Config::load(&environment)?;

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
            .map_err(|err| crate::error::Error::CargoMetadataError(manifest_path, err))?;

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
            .map_err(|err| crate::error::Error::CargoMetadataError(manifest_path.clone(), err))?;

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

    config.save(&environment)?;
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
pub async fn target_command(
    target_parameters: TargetParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    match target_parameters.target_type {
        TargetType::List(list_parameters) => {
            list_command(list_parameters, environment).await?;
        }
        TargetType::Add(add_parameters) => {
            add_command(add_parameters, environment).await?;
        }
        TargetType::Remove(remove_parameters) => {
            remove_command(remove_parameters, environment).await?;
        }
        TargetType::Refresh => {
            refresh_command(environment).await?;
        }
    }
    Ok(())
}

/// an extension trait on Cargo Metadata that allows easy retrieval
/// of a few pieces of information we need regularly
pub trait CargoMetadataExt {
    /// allows retrieval of a package by the manifest_path of its Cargo.toml
    ///
    /// this is usually required to get our own package in a workspace Metadata
    /// object that includes multiple packages
    ///
    /// # Errors
    ///
    /// fails if there is no package like that
    fn get_package_by_manifest_path(
        &self,
        manifest_path: &Path,
    ) -> Result<&cargo_metadata::Package, crate::error::Error>;

    /// allows retrieval of a package by the package id
    ///
    /// this is usually required to retrieve the package object
    /// for package ids mentioned in e.g. workspace members
    ///
    /// # Errors
    ///
    /// fails if there is no package like that
    fn get_package_by_id(
        &self,
        package_id: &PackageId,
    ) -> Result<&cargo_metadata::Package, crate::error::Error>;
}

impl CargoMetadataExt for cargo_metadata::Metadata {
    fn get_package_by_manifest_path(
        &self,
        manifest_path: &Path,
    ) -> Result<&cargo_metadata::Package, crate::error::Error> {
        let Some(package) = self
            .packages
            .iter()
            .find(|p| p.manifest_path == manifest_path)
        else {
            return Err(
                crate::error::Error::FoundNoPackageInCargoMetadataWithCurrentManifestPath(
                    manifest_path.to_owned(),
                ),
            );
        };
        Ok(package)
    }

    fn get_package_by_id(
        &self,
        package_id: &PackageId,
    ) -> Result<&cargo_metadata::Package, crate::error::Error> {
        let Some(package) = self.packages.iter().find(|p| p.id == *package_id) else {
            return Err(
                crate::error::Error::FoundNoPackageInCargoMetadataWithPackageId(
                    package_id.to_owned(),
                ),
            );
        };
        Ok(package)
    }
}

/// an extension trait on Cargo Metadata Packages that allows easy retrieval of a
/// few pieces of information we need regularly
pub trait CargoPackageExt {
    /// allows checking if this package has at least one target of the specified kind
    #[must_use]
    fn has_target(&self, target_kind: &cargo_metadata::TargetKind) -> bool;
}

impl CargoPackageExt for cargo_metadata::Package {
    fn has_target(&self, target_kind: &cargo_metadata::TargetKind) -> bool {
        self.targets.iter().any(|t| t.kind.contains(target_kind))
    }
}

/// represents the type of Rust crate
#[derive(
    Debug,
    Clone,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
pub enum CrateType {
    /// a binary crate
    Bin,
    /// a library crate
    Lib,
    /// a proc-macro crate
    ProcMacro,
}

impl CrateType {
    /// determine the set of `CrateType` for a given package
    #[must_use]
    pub fn from_package(package: &cargo_metadata::Package) -> BTreeSet<Self> {
        let mut crate_types = BTreeSet::new();
        if package.has_target(&cargo_metadata::TargetKind::Bin) {
            crate_types.insert(Self::Bin);
        }
        if package.has_target(&cargo_metadata::TargetKind::Lib) {
            crate_types.insert(Self::Lib);
        }
        if package.has_target(&cargo_metadata::TargetKind::ProcMacro) {
            crate_types.insert(Self::ProcMacro);
        }
        crate_types
    }
}

/// represents a target within a resolved target set
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Target {
    /// the manifest directory of the target
    pub manifest_dir: PathBuf,
    /// the manifest directories of the targets that this target depends on
    pub dependencies: Vec<PathBuf>,
}
