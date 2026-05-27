//! This module defines the core data structures and traits related to targets (workspaces and crates).
//! It includes extensions for `cargo_metadata` and the `Target` struct itself.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cargo_metadata::PackageId;

use std::collections::HashMap;

use crate::{Crate, Workspace};
use tracing::instrument;

/// The target sub command
#[derive(clap::Parser, Debug, Clone)]
pub enum TargetSubCommand {
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
    /// The target subcommand
    #[clap(subcommand)]
    pub sub_command: TargetSubCommand,
}

/// implementation of the target subcommand
///
/// # Errors
///
/// This command can fail due to errors in its subcommands, such as issues with loading or saving configuration, resolving manifest paths, or executing cargo metadata operations.
#[instrument]
pub async fn target_command(
    target_parameters: TargetParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    match target_parameters.sub_command {
        TargetSubCommand::List(list_parameters) => {
            list_command(list_parameters, environment).await?;
        }
        TargetSubCommand::Add(add_parameters) => {
            add_command(add_parameters, environment).await?;
        }
        TargetSubCommand::Remove(remove_parameters) => {
            remove_command(remove_parameters, environment).await?;
        }
        TargetSubCommand::Refresh => {
            refresh_command(environment).await?;
        }
    }
    Ok(())
}

/// Parameters for filtering crates
#[derive(clap::Parser, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrateFilterParameters {
    /// only list crates whose compile-time output kinds include this crate
    /// type (e.g. `bin`, `lib`, `proc-macro`)
    #[clap(long)]
    pub crate_type: Option<CrateType>,
    /// only list crates whose auxiliary cargo targets include this kind
    /// (`test`, `bench`, `example`, `custom-build`). Almost every package
    /// has at least a `test` target, so this filter on its own rarely
    /// narrows much.
    #[clap(long)]
    pub target_kind: Option<TargetKind>,
    /// only list crates that are standalone or not
    #[clap(long)]
    pub standalone: Option<bool>,
}

/// Parameters for filtering workspaces
#[derive(clap::Parser, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceFilterParameters {
    /// only list multi-crate workspaces
    #[clap(long)]
    pub no_standalone: bool,
}

/// The type of object to filter
#[derive(clap::Parser, Debug, Clone)]
pub enum TargetFilter {
    /// list workspaces
    Workspaces(WorkspaceFilterParameters),
    /// list crates
    Crates(CrateFilterParameters),
}

/// Parameters for list subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct ListParameters {
    /// the type of object to list
    #[clap(subcommand)]
    pub target_filter: TargetFilter,
}

/// implementation of the list subcommand
///
/// # Stability
///
/// The line format printed to stdout — currently
/// `{path} (standalone: {bool})` for workspaces and
/// `{path} (workspace: {ws}, crate_types: {…}, target_kinds: {…})` for crates
/// — is intended for human consumption and is **not** a stable interface.
/// In particular both type sections use Rust `Debug` for `BTreeSet`s
/// (e.g. `{Bin, Lib}`). Scripts that parse this output will break across
/// versions; use a future structured output flag instead when one exists.
///
/// # Errors
///
/// This command can fail if the configuration file cannot be loaded or parsed.
#[instrument]
pub async fn list_command(
    list_parameters: ListParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    // `Config::load` already returns `Ok(Self::default())` for a missing
    // file, so the only ways to get `Err` here are real failures —
    // permission-denied reads, other I/O errors, or malformed TOML — which
    // the user should see, not have silently swallowed as "no config".
    let config = crate::Config::load(&environment)?;
    #[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
    match list_parameters.target_filter {
        TargetFilter::Workspaces(params) => {
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
        TargetFilter::Crates(params) => {
            let workspace_standalone_map: HashMap<_, _> = config
                .workspaces
                .iter()
                .map(|w| (w.manifest_dir.clone(), w.is_standalone))
                .collect();

            for krate in config.crates {
                if let Some(crate_type) = &params.crate_type
                    && !krate.crate_types.contains(crate_type)
                {
                    continue;
                }
                if let Some(target_kind) = &params.target_kind
                    && !krate.target_kinds.contains(target_kind)
                {
                    continue;
                }
                if let Some(standalone) = params.standalone {
                    match workspace_standalone_map.get(&krate.workspace_manifest_dir) {
                        // Known workspace: respect the filter.
                        Some(&is_standalone) if is_standalone != standalone => continue,
                        Some(_) => {}
                        // Orphan crate: workspace was removed from config but
                        // the crate entry still references it. The user is
                        // likely filtering precisely to find these, so we
                        // surface the orphan rather than silently hiding it.
                        None => {
                            tracing::warn!(
                                "Crate {} references unknown workspace {} (listing as orphan)",
                                krate.manifest_dir.display(),
                                krate.workspace_manifest_dir.display(),
                            );
                        }
                    }
                }
                if krate.manifest_dir == krate.workspace_manifest_dir {
                    println!(
                        "{} (crate_types: {:?}, target_kinds: {:?})",
                        krate.manifest_dir.display(),
                        krate.crate_types,
                        krate.target_kinds,
                    );
                } else {
                    println!(
                        "{} (workspace: {}, crate_types: {:?}, target_kinds: {:?})",
                        krate.manifest_dir.display(),
                        krate.workspace_manifest_dir.display(),
                        krate.crate_types,
                        krate.target_kinds,
                    );
                }
            }
        }
    }
    Ok(())
}

/// Parameters for add subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct AddParameters {
    /// the manifest file to add, if it refers to a workspace manifest all crates in the workspace are added too
    #[clap(long)]
    pub manifest_path: PathBuf,
}

/// implementation of the add subcommand
///
/// # Errors
///
/// This command can fail due to issues with loading or saving the configuration, resolving or canonicalizing manifest paths, errors during cargo metadata execution, inability to determine parent directories of manifest paths, or if expected packages are not found in cargo metadata output.
#[instrument]
pub async fn add_command(
    add_parameters: AddParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    // Hold the config lock for the entire load → cargo-metadata → mutate →
    // save cycle. Without it, two concurrent `target add` invocations both
    // load the same baseline and the later save drops the earlier entry.
    let _lock = crate::ConfigLock::acquire(&environment)?;
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

    // Canonicalize the workspace manifest path so subsequent equality checks
    // (here and on later refresh) compare against canonical paths — cargo may
    // emit symlinked or non-canonical paths even when we passed it a canonical
    // one.
    let workspace_manifest_path_raw = initial_metadata
        .workspace_root
        .join("Cargo.toml")
        .into_std_path_buf();
    let workspace_manifest_path =
        fs_err::canonicalize(&workspace_manifest_path_raw).map_err(|err| {
            crate::error::Error::CouldNotDetermineCanonicalManifestPath(
                workspace_manifest_path_raw.clone(),
                err,
            )
        })?;

    let Some(workspace_manifest_dir) = workspace_manifest_path.parent() else {
        return Err(crate::error::Error::ManifestPathHasNoParentDir(
            workspace_manifest_path.clone(),
        ));
    };
    let workspace_manifest_dir = workspace_manifest_dir.to_path_buf();

    // second call to metadata to get all packages in the workspace
    let workspace_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&workspace_manifest_path)
        .exec()
        .map_err(|err| {
            crate::error::Error::CargoMetadataError(workspace_manifest_path.clone(), err)
        })?;

    // Resolve each workspace member's canonical Cargo.toml path up front so
    // every downstream comparison and storage uses the same canonicalization.
    let mut member_canonical_paths: Vec<(cargo_metadata::PackageId, PathBuf)> =
        Vec::with_capacity(workspace_metadata.workspace_members.len());
    for package_id in &workspace_metadata.workspace_members {
        let package = workspace_metadata.get_package_by_id(package_id)?;
        let raw = package.manifest_path.clone().into_std_path_buf();
        let canonical = fs_err::canonicalize(&raw).map_err(|err| {
            crate::error::Error::CouldNotDetermineCanonicalManifestPath(raw.clone(), err)
        })?;
        member_canonical_paths.push((package_id.clone(), canonical));
    }

    let is_standalone = match member_canonical_paths.as_slice() {
        [(_, only_manifest)] => *only_manifest == workspace_manifest_path,
        _ => false,
    };

    if is_standalone {
        tracing::debug!("Identified Cargo.toml as standalone crate");
        let [(package_id, _)] = member_canonical_paths.as_slice() else {
            unreachable!("is_standalone implies exactly one workspace member");
        };
        let package = workspace_metadata.get_package_by_id(package_id)?;
        let crate_types = CrateType::from_package(package);
        let target_kinds = TargetKind::from_package(package);
        config.add_workspace(Workspace {
            manifest_dir: workspace_manifest_dir.clone(),
            is_standalone: true,
        });
        config.add_crate(Crate {
            manifest_dir: workspace_manifest_dir.clone(),
            workspace_manifest_dir,
            crate_types,
            target_kinds,
        });
    } else {
        tracing::debug!("Identified Cargo.toml as workspace");
        config.add_workspace(Workspace {
            manifest_dir: workspace_manifest_dir.clone(),
            is_standalone: false,
        });
        for (package_id, package_manifest_path) in &member_canonical_paths {
            let package = workspace_metadata.get_package_by_id(package_id)?;
            let Some(package_manifest_dir) = package_manifest_path.parent() else {
                return Err(crate::error::Error::ManifestPathHasNoParentDir(
                    package_manifest_path.clone(),
                ));
            };
            let crate_types = CrateType::from_package(package);
            let target_kinds = TargetKind::from_package(package);
            config.add_crate(Crate {
                manifest_dir: package_manifest_dir.to_path_buf(),
                workspace_manifest_dir: workspace_manifest_dir.clone(),
                crate_types,
                target_kinds,
            });
        }
    }

    config.save(&environment)?;

    Ok(())
}

/// Parameters for remove subcommand
#[derive(clap::Parser, Debug, Clone)]
pub struct RemoveParameters {
    /// the manifest file to remove
    #[clap(long)]
    pub manifest_path: PathBuf,
}

/// implementation of the remove subcommand
///
/// # Errors
///
/// This command can fail due to issues with loading or saving the configuration, resolving or canonicalizing manifest paths, or other file system errors during config saving.
#[instrument]
pub async fn remove_command(
    remove_parameters: RemoveParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    let _lock = crate::ConfigLock::acquire(&environment)?;
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

    // The user supplies a path to Cargo.toml; config entries store the
    // containing directory, so derive the directory before comparing.
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| crate::error::Error::ManifestPathHasNoParentDir(manifest_path.clone()))?
        .to_path_buf();

    // Filter out the workspace if it matches the manifest_dir
    let initial_workspace_count = config.workspaces.len();
    config.workspaces.retain(|w| w.manifest_dir != manifest_dir);
    if config.workspaces.len() < initial_workspace_count {
        tracing::debug!("Removed workspace at {}", manifest_dir.display());
    } else {
        tracing::warn!("No workspace found at {}", manifest_dir.display());
    }

    // Filter out crates that match the manifest_dir or belong to the removed workspace
    let initial_crate_count = config.crates.len();
    config
        .crates
        .retain(|c| c.manifest_dir != manifest_dir && c.workspace_manifest_dir != manifest_dir);
    if config.crates.len() < initial_crate_count {
        tracing::debug!("Removed crates associated with {}", manifest_dir.display());
    } else {
        tracing::warn!("No crates found associated with {}", manifest_dir.display());
    }

    config.save(&environment)?;
    Ok(())
}

/// Check whether a `Cargo.toml` exists under `manifest_dir`.
///
/// Returns `true` when the file is observably present (using
/// `symlink_metadata`, so a broken symlink still counts as present rather
/// than "gone"). Returns `false` only on a confirmed `NotFound`. Any other
/// I/O error (permission denied, broken mount, etc.) is logged and the
/// entry is kept, since silently dropping a config entry on a transient
/// failure is destructive.
fn cargo_toml_present(manifest_dir: &Path) -> bool {
    let cargo_toml = manifest_dir.join("Cargo.toml");
    match fs_err::symlink_metadata(&cargo_toml) {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            tracing::warn!(
                "Could not stat {} during refresh: {}. Keeping the entry.",
                cargo_toml.display(),
                err
            );
            true
        }
    }
}

/// implementation of the refresh subcommand
///
/// # Errors
///
/// This command can fail due to issues with loading or saving the configuration, errors during cargo metadata execution, if expected packages are not found in cargo metadata output, or other file system errors during config saving.
#[instrument]
pub async fn refresh_command(environment: crate::Environment) -> Result<(), crate::error::Error> {
    let _lock = crate::ConfigLock::acquire(&environment)?;
    let mut config = crate::Config::load(&environment)?;

    // 1. Remove workspaces that no longer exist.
    let (retained_workspaces, removed_workspaces): (Vec<_>, Vec<_>) = config
        .workspaces
        .drain(..)
        .partition(|w| cargo_toml_present(&w.manifest_dir));
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
        .partition(|c| cargo_toml_present(&c.manifest_dir));
    for r in &removed_crates {
        tracing::debug!(
            "Removing crate at {} because Cargo.toml is gone.",
            r.manifest_dir.display()
        );
    }
    config.crates = retained_crates;

    // 3. For all existing workspaces, discover and add new member crates.
    //    We don't need to update existing crates found here, as the next step will do it.
    //    A failure in one workspace's cargo-metadata must not abort refresh
    //    entirely (the deletions from steps 1-2 still need to be saved);
    //    log and skip that workspace instead.
    let workspaces_to_scan = config.workspaces.clone();
    for workspace in &workspaces_to_scan {
        let manifest_path = workspace.manifest_dir.join("Cargo.toml");
        let cargo_metadata = match cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .exec()
        {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    "cargo-metadata failed for {}: {err}. Skipping new-crate discovery for this workspace.",
                    manifest_path.display()
                );
                continue;
            }
        };

        for package_id in &cargo_metadata.workspace_members {
            let package = cargo_metadata.get_package_by_id(package_id)?;
            let pkg_manifest_path = package.manifest_path.to_owned().into_std_path_buf();
            let Some(manifest_dir) = pkg_manifest_path.parent() else {
                continue;
            };
            // Canonicalize so the existence check below compares against the
            // canonical paths stored at add-time, not raw cargo output.
            let manifest_dir = match fs_err::canonicalize(manifest_dir) {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(
                        "Could not canonicalize {} during refresh: {err}. Skipping.",
                        manifest_dir.display()
                    );
                    continue;
                }
            };

            // Only add if it doesn't exist. `add_crate` also de-dupes.
            if !config.crates.iter().any(|c| c.manifest_dir == manifest_dir) {
                let crate_types = CrateType::from_package(package);
                let target_kinds = TargetKind::from_package(package);
                config.add_crate(Crate {
                    manifest_dir,
                    workspace_manifest_dir: workspace.manifest_dir.clone(),
                    crate_types,
                    target_kinds,
                });
            }
        }
    }

    // 4. Update crate_types/target_kinds for all existing crates.
    //    Same warn-and-continue rationale as step 3.
    for krate in &mut config.crates {
        let manifest_path = krate.manifest_dir.join("Cargo.toml");

        let cargo_metadata = match cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .no_deps()
            .exec()
        {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    "cargo-metadata failed for {}: {err}. Skipping type update.",
                    manifest_path.display()
                );
                continue;
            }
        };

        // We need the package object to determine the crate type.
        // Using get_package_by_manifest_path is correct for single crates/workspace members.
        if let Ok(package) = cargo_metadata.get_package_by_manifest_path(&manifest_path) {
            let new_crate_types = CrateType::from_package(package);
            let new_target_kinds = TargetKind::from_package(package);
            if krate.crate_types != new_crate_types {
                tracing::debug!(
                    "Updating crate_types for {} from {:?} to {:?}",
                    krate.manifest_dir.display(),
                    krate.crate_types,
                    new_crate_types
                );
                krate.crate_types = new_crate_types;
            }
            if krate.target_kinds != new_target_kinds {
                tracing::debug!(
                    "Updating target_kinds for {} from {:?} to {:?}",
                    krate.manifest_dir.display(),
                    krate.target_kinds,
                    new_target_kinds
                );
                krate.target_kinds = new_target_kinds;
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
    /// Returns a `FoundNoPackageInCargoMetadataWithCurrentManifestPath` error if no package is found with a manifest path matching the provided one.
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
    /// Returns a `FoundNoPackageInCargoMetadataWithPackageId` error if no package is found with the provided package ID.
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

/// The compile-time output kind of a Rust crate.
///
/// This corresponds to entries that may legitimately appear in a `Cargo.toml`
/// `[lib].crate-type` (`lib`, `rlib`, `dylib`, `cdylib`, `staticlib`,
/// `proc-macro`) plus the `bin` produced by `[[bin]]` targets. The auxiliary
/// build artifacts cargo also generates per package — tests, benches,
/// examples, the build script — live in [`TargetKind`] instead.
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
    /// a C-compatible dynamic library (e.g. for FFI or WebAssembly)
    CDyLib,
    /// a Rust dynamic library
    DyLib,
    /// a Rust static library (rlib)
    RLib,
    /// a C-compatible static library
    StaticLib,
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
        if package.has_target(&cargo_metadata::TargetKind::CDyLib) {
            crate_types.insert(Self::CDyLib);
        }
        if package.has_target(&cargo_metadata::TargetKind::DyLib) {
            crate_types.insert(Self::DyLib);
        }
        if package.has_target(&cargo_metadata::TargetKind::RLib) {
            crate_types.insert(Self::RLib);
        }
        if package.has_target(&cargo_metadata::TargetKind::StaticLib) {
            crate_types.insert(Self::StaticLib);
        }
        crate_types
    }
}

/// An auxiliary cargo target kind attached to a package.
///
/// These are the cargo target kinds that are **not** a compile-time crate
/// output: integration tests, benchmarks, examples, and the build script.
/// Almost every package implicitly has at least a `Test` kind, so filters on
/// `TargetKind` should not be used as a substitute for a [`CrateType`] filter.
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
pub enum TargetKind {
    /// a benchmark target
    Bench,
    /// an integration test target
    Test,
    /// an example target
    Example,
    /// a custom build script (build.rs)
    CustomBuild,
}

impl TargetKind {
    /// determine the set of `TargetKind` for a given package
    #[must_use]
    pub fn from_package(package: &cargo_metadata::Package) -> BTreeSet<Self> {
        let mut target_kinds = BTreeSet::new();
        if package.has_target(&cargo_metadata::TargetKind::Bench) {
            target_kinds.insert(Self::Bench);
        }
        if package.has_target(&cargo_metadata::TargetKind::Test) {
            target_kinds.insert(Self::Test);
        }
        if package.has_target(&cargo_metadata::TargetKind::Example) {
            target_kinds.insert(Self::Example);
        }
        if package.has_target(&cargo_metadata::TargetKind::CustomBuild) {
            target_kinds.insert(Self::CustomBuild);
        }
        target_kinds
    }
}
