//! This module defines the core data structures and traits related to targets (workspaces and crates).
//! It includes extensions for `cargo_metadata` and the `Target` struct itself.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cargo_metadata::PackageId;

use crate::error::Error;

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
    ) -> Result<&cargo_metadata::Package, Error>;

    /// allows retrieval of a package by the package id
    ///
    /// this is usually required to retrieve the package object
    /// for package ids mentioned in e.g. workspace members
    ///
    /// # Errors
    ///
    /// fails if there is no package like that
    fn get_package_by_id(&self, package_id: &PackageId) -> Result<&cargo_metadata::Package, Error>;
}

impl CargoMetadataExt for cargo_metadata::Metadata {
    fn get_package_by_manifest_path(
        &self,
        manifest_path: &Path,
    ) -> Result<&cargo_metadata::Package, Error> {
        let Some(package) = self
            .packages
            .iter()
            .find(|p| p.manifest_path == manifest_path)
        else {
            return Err(Error::FoundNoPackageInCargoMetadataWithCurrentManifestPath(
                manifest_path.to_owned(),
            ));
        };
        Ok(package)
    }

    fn get_package_by_id(&self, package_id: &PackageId) -> Result<&cargo_metadata::Package, Error> {
        let Some(package) = self.packages.iter().find(|p| p.id == *package_id) else {
            return Err(Error::FoundNoPackageInCargoMetadataWithPackageId(
                package_id.to_owned(),
            ));
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
