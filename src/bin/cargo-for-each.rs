#![doc = include_str!("../../README.md")]

use std::collections::BTreeSet;

use tracing::instrument;
use tracing_subscriber::{
    EnvFilter, Layer as _, Registry, filter::LevelFilter, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

/// Error enum for the application
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// error reading environment variable
    #[error("error when retrieving environment variable: {0}")]
    EnvVarError(
        #[source]
        #[from]
        std::env::VarError,
    ),
    /// error in clap
    #[error("error in CLI option parsing: {0}")]
    ClapError(
        #[source]
        #[from]
        clap::Error,
    ),
    /// error parsing log filter
    #[error("error parsing log filter: {0}")]
    LogFilterParseError(
        #[source]
        #[from]
        tracing_subscriber::filter::ParseError,
    ),
    /// error joining task
    #[error("error joining task: {0}")]
    JoinError(
        #[source]
        #[from]
        tokio::task::JoinError,
    ),
    /// error constructing tracing-journald layer
    #[cfg(target_os = "linux")]
    #[error("error constructing tracing-journald layer: {0}")]
    TracingJournaldError(#[source] std::io::Error),
    /// error generating man pages
    #[error("error generating man pages: {0}")]
    GenerateManpageError(#[source] std::io::Error),
    /// error generating shell completion
    #[error("error generating shell completion: {0}")]
    GenerateShellCompletionError(#[source] std::io::Error),
    /// error determining user config dir
    #[error("error determining user config dir")]
    CouldNotDetermineUserConfigDir,
    /// error reading config file
    #[error("error reading config file: {0}")]
    CouldNotReadConfigFile(#[source] std::io::Error),
    /// error parsing config file
    #[error("error parsing config file: {0}")]
    CouldNotParseConfigFile(#[source] toml::de::Error),
    /// error serializing config file
    #[error("error serializing config file: {0}")]
    CouldNotSerializeConfigFile(#[source] toml::ser::Error),
    /// could not create parent directories for config file
    #[error("could not create parent directories for config file: {0}")]
    CouldNotCreateConfigFileParentDirs(#[source] std::io::Error),
    /// error writing config file
    #[error("error writing config file: {0}")]
    CouldNotWriteConfigFile(#[source] std::io::Error),
    /// error running cargo-metadata
    #[error("error running cargo-metadata for {0}: {1}")]
    CargoMetadataError(std::path::PathBuf, #[source] cargo_metadata::Error),
    /// error turning a relative manifest path into an absolute one
    #[error("error turning the relative manifest path {0} into an absolute one: {1}")]
    CouldNotDetermineAbsoluteManifestPath(std::path::PathBuf, #[source] std::io::Error),
    /// error turning a absolute manifest path into a canonical one
    #[error("error turning the absolute manifest path {0} into a canonical one: {1}")]
    CouldNotDetermineCanonicalManifestPath(std::path::PathBuf, #[source] std::io::Error),
    /// the given manifest path has no parent directory
    #[error("the given manifest path {0} has no parent directory")]
    ManifestPathHasNoParentDir(std::path::PathBuf),
    /// we called cargo metadata on a directory with a Cargo.toml
    /// but the output did not contain a package with the manifest_path
    /// pointing to that Cargo.toml
    #[error(
        "found no package with manifest_path matching local Cargo.toml in cargo metadata output: {0}"
    )]
    FoundNoPackageInCargoMetadataWithCurrentManifestPath(std::path::PathBuf),
    /// metadata did not include a package with the given package id
    #[error("cargo metadata did not include a package with the package id {0}")]
    FoundNoPackageInCargoMetadataWithPackageId(cargo_metadata::PackageId),
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
        manifest_path: &std::path::Path,
    ) -> Result<&cargo_metadata::Package, Error>;

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
        package_id: &cargo_metadata::PackageId,
    ) -> Result<&cargo_metadata::Package, Error>;
}

impl CargoMetadataExt for cargo_metadata::Metadata {
    fn get_package_by_manifest_path(
        &self,
        manifest_path: &std::path::Path,
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

    fn get_package_by_id(
        &self,
        package_id: &cargo_metadata::PackageId,
    ) -> Result<&cargo_metadata::Package, Error> {
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

/// represents a Rust workspace
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    /// the directory that contains the workspace Cargo.toml file
    manifest_dir: std::path::PathBuf,
}

/// represents the type of Rust crate
#[derive(
    Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
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

/// represents a Rust crate
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Crate {
    /// the directory that contains the crate Cargo.toml file
    manifest_dir: std::path::PathBuf,
    /// the types of this crate (only bin and lib can be combined so this should have at most two members)
    crate_types: std::collections::BTreeSet<CrateType>,
}

/// represents the cargo-for-each configuration file
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// represents all the workspaces we know about
    workspaces: Vec<Workspace>,
    /// presents all the crates we know about
    crates: Vec<Crate>,
}

impl Config {
    /// adds a workspace to the config if it is not already present
    pub fn add_workspace(&mut self, workspace: Workspace) {
        if !self
            .workspaces
            .iter()
            .any(|w| w.manifest_dir == workspace.manifest_dir)
        {
            self.workspaces.push(workspace);
        }
    }

    /// adds a crate to the config, ignoring the new one if one with the same manifest directory already exists
    pub fn add_crate(&mut self, krate: Crate) {
        // If a crate with the same manifest_dir already exists, do nothing (ignore the new one).
        if self
            .crates
            .iter()
            .any(|c| c.manifest_dir == krate.manifest_dir)
        {
            return;
        }
        // If it doesn't exist, add the new crate.
        self.crates.push(krate);
    }

    /// Load the config file
    fn load() -> Result<Self, Error> {
        let config_file_path = config_file_path()?;
        if fs_err::exists(&config_file_path).map_err(Error::CouldNotReadConfigFile)? {
            let file_content =
                fs_err::read_to_string(&config_file_path).map_err(Error::CouldNotReadConfigFile)?;
            toml::from_str(&file_content).map_err(Error::CouldNotParseConfigFile)
        } else {
            Ok(Self::default())
        }
    }

    /// Save the config file
    fn save(&self) -> Result<(), Error> {
        let config_file_path = config_file_path()?;
        if let Some(config_dir_path) = config_file_path.parent() {
            fs_err::create_dir_all(config_dir_path)
                .map_err(Error::CouldNotCreateConfigFileParentDirs)?;
        }
        fs_err::write(
            &config_file_path,
            toml::to_string(self).map_err(Error::CouldNotSerializeConfigFile)?,
        )
        .map_err(Error::CouldNotWriteConfigFile)
    }
}

/// returns the config file path
fn config_file_path() -> Result<std::path::PathBuf, Error> {
    Ok(dirs::config_dir()
        .ok_or(Error::CouldNotDetermineUserConfigDir)?
        .join("cargo-for-each/cargo-for-each.toml"))
}

/// The type of object to list
#[derive(clap::Parser, Debug, Clone)]
pub enum ListType {
    /// list workspaces
    Workspaces,
    /// list crates
    Crates,
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
    pub manifest_path: std::path::PathBuf,
}

/// which subcommand to call
#[derive(clap::Parser, Debug)]
pub enum Command {
    /// Call list subcommand
    List(ListParameters),
    /// Call add subcommand
    Add(AddParameters),
    /// Generate man page
    GenerateManpage {
        /// target dir for man page generation
        #[clap(long)]
        output_dir: std::path::PathBuf,
    },
    /// Generate shell completion
    GenerateShellCompletion {
        /// output file for shell completion generation
        #[clap(long)]
        output_file: std::path::PathBuf,
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
struct Options {
    /// which subcommand to use
    #[clap(subcommand)]
    command: Command,
}

/// implementation of the list subcommand
///
/// # Errors
///
/// fails if the implementation of list fails
#[instrument]
async fn list_command(list_parameters: ListParameters) -> Result<(), crate::Error> {
    #[expect(clippy::print_stderr, reason = "This is part of the UI, not logging")]
    let Ok(config) = Config::load() else {
        eprintln!("No config file found, nothing to list");
        return Ok(());
    };
    #[expect(clippy::print_stdout, reason = "This is part of the UI, not logging")]
    match list_parameters.list_type {
        ListType::Workspaces => {
            for workspace in config.workspaces {
                println!("{}", workspace.manifest_dir.display());
            }
        }
        ListType::Crates => {
            for krate in config.crates {
                println!("{} ({:?})", krate.manifest_dir.display(), krate.crate_types);
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
async fn add_command(add_parameters: AddParameters) -> Result<(), crate::Error> {
    let mut config = Config::load()?;
    let manifest_path =
        std::path::absolute(add_parameters.manifest_path.clone()).map_err(|err| {
            Error::CouldNotDetermineAbsoluteManifestPath(add_parameters.manifest_path, err)
        })?;
    let manifest_path = fs_err::canonicalize(manifest_path.clone())
        .map_err(|err| Error::CouldNotDetermineCanonicalManifestPath(manifest_path, err))?;
    let Some(manifest_dir) = manifest_path.parent() else {
        return Err(Error::ManifestPathHasNoParentDir(manifest_path));
    };
    let manifest_dir = manifest_dir.to_path_buf();
    let cargo_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(manifest_path.clone())
        .exec()
        .map_err(|err| Error::CargoMetadataError(manifest_path.clone(), err))?;
    let workspace_root = cargo_metadata.workspace_root.to_owned().into_std_path_buf();
    tracing::debug!("Manifest dir: {}", manifest_dir.display());
    tracing::debug!("Workspace root: {}", workspace_root.display());
    if workspace_root == manifest_dir {
        // either standalone crate or workspace
        if let [package_id] = cargo_metadata.workspace_members.as_slice()
            && let package = cargo_metadata.get_package_by_id(package_id)?
            && package.manifest_path == manifest_path
        {
            tracing::debug!("Identified Cargo.toml as standalone crate");
            // the single package in the workspace lives in the directory
            // we called cargo metadata on so we are dealing with a
            // standalone crate
            //
            // if it was a workspace root Cargo.toml the manifest path
            // should not appear in any packages
            //
            // if it was the Cargo.toml of a lone member in a workspace the
            // workspace root should be in a parent directory
            let crate_types = CrateType::from_package(package);
            config.add_crate(Crate {
                manifest_dir,
                crate_types,
            });
        } else {
            tracing::debug!("Identified Cargo.toml as workspace");
            config.add_workspace(Workspace { manifest_dir });
            for package_id in cargo_metadata.workspace_members.clone() {
                let package = cargo_metadata.get_package_by_id(&package_id)?;
                let manifest_path = package.manifest_path.to_owned().into_std_path_buf();
                let Some(manifest_dir) = manifest_path.parent() else {
                    return Err(Error::ManifestPathHasNoParentDir(manifest_path));
                };
                let manifest_dir = manifest_dir.to_path_buf();
                let crate_types = CrateType::from_package(package);
                config.add_crate(Crate {
                    manifest_dir,
                    crate_types,
                });
            }
        }
    } else {
        tracing::debug!("Identified Cargo.toml as crate inside a workspace");
        // crate inside workspace
        if let [package_id] = cargo_metadata.workspace_members.as_slice()
            && let package = cargo_metadata.get_package_by_id(package_id)?
            && package.manifest_path == manifest_path
        {
            let crate_types = CrateType::from_package(package);
            config.add_crate(Crate {
                manifest_dir,
                crate_types,
            });
        }
    }
    config.save()?;

    Ok(())
}

/// The main behaviour of the binary should go here
///
/// # Errors
///
/// fails if the main behavior of the application fails
#[instrument]
async fn do_stuff() -> Result<(), crate::Error> {
    let options = <Options as clap::Parser>::parse();
    tracing::debug!("{:#?}", options);

    // main code either goes here or into the individual subcommands

    match options.command {
        Command::List(list_parameters) => {
            list_command(list_parameters).await?;
        }
        Command::Add(add_parameters) => {
            add_command(add_parameters).await?;
        }
        Command::GenerateManpage { output_dir } => {
            // generate man pages
            clap_mangen::generate_to(<Options as clap::CommandFactory>::command(), output_dir)
                .map_err(crate::Error::GenerateManpageError)?;
        }
        Command::GenerateShellCompletion { output_file, shell } => {
            let mut f = std::fs::File::create(output_file)
                .map_err(crate::Error::GenerateShellCompletionError)?;
            let mut c = <Options as clap::CommandFactory>::command();
            clap_complete::generate(shell, &mut c, "cargo-for-each", &mut f);
        }
    }

    Ok(())
}

/// The main function mainly just handles setting up tracing
/// and handling any Err Results.
#[tokio::main]
async fn main() -> Result<(), Error> {
    let terminal_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .parse(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string()))?;
    let file_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .parse(std::env::var("CARGO_FOR_EACH_LOG").unwrap_or_else(|_| "trace".to_string()))?;
    #[cfg(target_os = "linux")]
    let journald_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .parse(
            std::env::var("CARGO_FOR_EACH_JOURNALD_LOG").unwrap_or_else(|_| "info".to_string()),
        )?;
    let registry = Registry::default();
    let registry =
        registry.with(tracing_subscriber::fmt::Layer::default().with_filter(terminal_env_filter));
    let log_dir = std::env::var("CARGO_FOR_EACH_LOG_DIR");
    let file_layer = if let Ok(log_dir) = log_dir {
        let log_file = if let Ok(log_file) = std::env::var("CARGO_FOR_EACH_LOG_FILE") {
            log_file
        } else {
            "cargo_for_each.log".to_string()
        };
        let file_appender = tracing_appender::rolling::never(log_dir, log_file);
        Some(
            tracing_subscriber::fmt::Layer::default()
                .with_writer(file_appender)
                .with_filter(file_env_filter),
        )
    } else {
        None
    };
    let registry = registry.with(file_layer);
    #[cfg(target_os = "linux")]
    let registry = registry.with(
        tracing_journald::layer()
            .map_err(crate::Error::TracingJournaldError)?
            .with_filter(journald_env_filter),
    );
    registry.init();
    log_panics::init();
    #[expect(
        clippy::print_stderr,
        reason = "This is the final print in our error chain and we already log this with tracing above but depending on log level the tracing output is not seen by the user"
    )]
    match do_stuff().await {
        Ok(()) => (),
        Err(e) => {
            tracing::error!("{e}");
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    tracing::debug!("Exiting");
    Ok(())
}

#[cfg(test)]
mod test {
    //use super::*;
    //use pretty_assertions::{assert_eq, assert_ne};
}
