//! Resolves a parsed [`Program`] against the registered workspaces/crates to
//! produce a [`ResolvedProgram`] snapshot for task execution.

pub mod snapshot;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use cargo_metadata::PackageId;

use crate::error::Error;
use crate::program::ast::crate_ctx::{CrateFilter, CrateSelectCondition, CrateTypeFilter};
use crate::program::ast::workspace_ctx::{WorkspaceFilter, WorkspaceSelectCondition};
use crate::program::{GlobalStatement, Program};
use crate::targets::CrateType;

pub use snapshot::{ResolvedCrateExecution, ResolvedProgram, ResolvedWorkspaceExecution};

/// Resolves a parsed program against the current configuration.
///
/// Processes all `select workspaces` and `select crates` statements, filters
/// the registered targets accordingly, and returns a [`ResolvedProgram`] that
/// lists which workspaces and crates will be iterated over when the task runs.
///
/// # Errors
///
/// Returns an error if `cargo metadata` fails for any workspace, if a manifest
/// path cannot be canonicalized, or if a package listed in metadata cannot be
/// found.
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the resolve module"
)]
pub fn resolve_program(
    program: &Program,
    config: &crate::Config,
) -> Result<ResolvedProgram, Error> {
    // ── Collect filters from the program ─────────────────────────────────────
    let workspace_filters: Vec<&WorkspaceFilter> = program
        .statements
        .iter()
        .filter_map(|s| {
            if let GlobalStatement::SelectWorkspaces(f) = s {
                Some(f)
            } else {
                None
            }
        })
        .collect();

    let crate_filters: Vec<&CrateFilter> = program
        .statements
        .iter()
        .filter_map(|s| {
            if let GlobalStatement::SelectCrates(f) = s {
                Some(f)
            } else {
                None
            }
        })
        .collect();

    // ── Resolve workspaces ────────────────────────────────────────────────────
    let workspace_executions = if workspace_filters.is_empty() {
        Vec::new()
    } else {
        resolve_workspaces(&workspace_filters, config)?
    };

    // ── Resolve standalone crates ─────────────────────────────────────────────
    let crate_executions = if crate_filters.is_empty() {
        Vec::new()
    } else {
        resolve_standalone_crates(&crate_filters, config)?
    };

    Ok(ResolvedProgram {
        workspace_executions,
        crate_executions,
    })
}

/// Returns `true` if the workspace satisfies the filter.
fn workspace_matches_filter(workspace: &crate::Workspace, filter: &WorkspaceFilter) -> bool {
    match &filter.condition {
        None => true,
        Some(cond) => evaluate_workspace_select_condition(cond, workspace),
    }
}

/// Evaluates a [`WorkspaceSelectCondition`] against a single workspace.
fn evaluate_workspace_select_condition(
    cond: &WorkspaceSelectCondition,
    workspace: &crate::Workspace,
) -> bool {
    match cond {
        WorkspaceSelectCondition::Standalone => workspace.is_standalone,
        WorkspaceSelectCondition::HasMembers => !workspace.is_standalone,
        WorkspaceSelectCondition::Not(inner) => {
            !evaluate_workspace_select_condition(inner, workspace)
        }
        WorkspaceSelectCondition::And(conditions) => conditions
            .iter()
            .all(|c| evaluate_workspace_select_condition(c, workspace)),
        WorkspaceSelectCondition::Or(conditions) => conditions
            .iter()
            .any(|c| evaluate_workspace_select_condition(c, workspace)),
    }
}

/// Returns `true` if the crate satisfies the filter.
fn crate_matches_filter(
    krate: &crate::Crate,
    filter: &CrateFilter,
    workspace_standalone_map: &HashMap<PathBuf, bool>,
) -> bool {
    match &filter.condition {
        None => true,
        Some(cond) => evaluate_crate_select_condition(cond, krate, workspace_standalone_map),
    }
}

/// Evaluates a [`CrateSelectCondition`] against a single crate.
fn evaluate_crate_select_condition(
    cond: &CrateSelectCondition,
    krate: &crate::Crate,
    workspace_standalone_map: &HashMap<PathBuf, bool>,
) -> bool {
    match cond {
        CrateSelectCondition::Standalone => workspace_standalone_map
            .get(&krate.workspace_manifest_dir)
            .copied()
            .unwrap_or(false),
        CrateSelectCondition::CrateType(filter) => match filter {
            CrateTypeFilter::Bin => krate.types.contains(&CrateType::Bin),
            CrateTypeFilter::Lib => krate.types.contains(&CrateType::Lib),
            CrateTypeFilter::ProcMacro => krate.types.contains(&CrateType::ProcMacro),
        },
        CrateSelectCondition::Not(inner) => {
            !evaluate_crate_select_condition(inner, krate, workspace_standalone_map)
        }
        CrateSelectCondition::And(conditions) => conditions
            .iter()
            .all(|c| evaluate_crate_select_condition(c, krate, workspace_standalone_map)),
        CrateSelectCondition::Or(conditions) => conditions
            .iter()
            .any(|c| evaluate_crate_select_condition(c, krate, workspace_standalone_map)),
    }
}

/// Selects and resolves the workspaces that match any of the given filters,
/// together with their member crates and dependency information.
fn resolve_workspaces(
    filters: &[&WorkspaceFilter],
    config: &crate::Config,
) -> Result<Vec<ResolvedWorkspaceExecution>, Error> {
    // Deduplicate: a workspace is selected if it matches at least one filter.
    let selected_manifest_dirs: Vec<PathBuf> = config
        .workspaces
        .iter()
        .filter(|w| filters.iter().any(|f| workspace_matches_filter(w, f)))
        .map(|w| w.manifest_dir.clone())
        .collect::<Vec<_>>();

    // Canonicalize selected workspace paths.
    let canonical_selected: Vec<PathBuf> = selected_manifest_dirs
        .iter()
        .map(|d| {
            fs_err::canonicalize(d)
                .map_err(|e| Error::CouldNotDetermineCanonicalManifestPath(d.clone(), e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let selected_set: HashSet<&PathBuf> = canonical_selected.iter().collect();

    // For each selected workspace, load cargo metadata to get member crates.
    // We also collect all package info to compute inter-workspace deps.
    let mut workspace_packages: HashMap<PathBuf, Vec<WorkspaceMemberInfo>> = HashMap::new();
    let mut all_packages: HashMap<PackageId, cargo_metadata::Package> = HashMap::new();
    let mut package_name_to_id: HashMap<String, PackageId> = HashMap::new();

    for canonical_ws_dir in &canonical_selected {
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(canonical_ws_dir.join("Cargo.toml"))
            .no_deps()
            .exec()
            .map_err(|e| Error::CargoMetadataError(canonical_ws_dir.clone(), e))?;

        let mut members: Vec<WorkspaceMemberInfo> = Vec::new();
        for package in metadata.packages {
            let pkg_dir = package.manifest_path.parent().ok_or_else(|| {
                Error::ManifestPathHasNoParentDir(package.manifest_path.clone().into_std_path_buf())
            })?;
            let canonical_pkg_dir = fs_err::canonicalize(pkg_dir).map_err(|e| {
                Error::CouldNotDetermineCanonicalManifestPath(pkg_dir.to_path_buf().into(), e)
            })?;
            members.push(WorkspaceMemberInfo {
                package_id: package.id.clone(),
                manifest_dir: canonical_pkg_dir.clone(),
            });
            package_name_to_id.insert(package.name.to_string(), package.id.clone());
            all_packages.insert(package.id.clone(), package);
        }
        workspace_packages.insert(canonical_ws_dir.clone(), members);
    }

    // For each selected workspace, resolve member crates (with intra-workspace deps)
    // and determine inter-workspace dependencies.
    let mut executions: Vec<ResolvedWorkspaceExecution> = Vec::new();

    for canonical_ws_dir in &canonical_selected {
        let member_crates = resolve_workspace_member_crates(
            canonical_ws_dir,
            &workspace_packages,
            &all_packages,
            &package_name_to_id,
        )?;

        // Inter-workspace deps: does any member of this workspace depend on a
        // crate that belongs to a *different* selected workspace?
        let workspace_deps = compute_inter_workspace_deps(
            canonical_ws_dir,
            &workspace_packages,
            &all_packages,
            &package_name_to_id,
            &selected_set,
            &canonical_selected,
        );

        executions.push(ResolvedWorkspaceExecution {
            manifest_dir: canonical_ws_dir.clone(),
            dependencies: workspace_deps,
            member_crates,
        });
    }

    Ok(executions)
}

/// Info about a single workspace member package.
struct WorkspaceMemberInfo {
    /// The cargo package ID.
    package_id: PackageId,
    /// Canonical manifest directory of the member.
    manifest_dir: PathBuf,
}

/// Resolves the member crates of a single workspace with their intra-workspace
/// dependencies.
fn resolve_workspace_member_crates(
    workspace_dir: &Path,
    workspace_packages: &HashMap<PathBuf, Vec<WorkspaceMemberInfo>>,
    all_packages: &HashMap<PackageId, cargo_metadata::Package>,
    package_name_to_id: &HashMap<String, PackageId>,
) -> Result<Vec<ResolvedCrateExecution>, Error> {
    let Some(members) = workspace_packages.get(workspace_dir) else {
        return Ok(Vec::new());
    };

    let member_dirs: HashSet<&PathBuf> = members.iter().map(|m| &m.manifest_dir).collect();

    let mut crates: Vec<ResolvedCrateExecution> = Vec::new();

    for member in members {
        let package = all_packages.get(&member.package_id).ok_or_else(|| {
            Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(member.manifest_dir.clone())
        })?;

        let mut dependencies: Vec<PathBuf> = Vec::new();
        for dep in &package.dependencies {
            if let Some(dep_id) = package_name_to_id.get(&dep.name)
                && let Some(dep_pkg) = all_packages.get(dep_id)
            {
                let dep_dir = dep_pkg.manifest_path.parent().ok_or_else(|| {
                    Error::ManifestPathHasNoParentDir(
                        dep_pkg.manifest_path.clone().into_std_path_buf(),
                    )
                })?;
                let canonical_dep_dir = dep_dir.canonicalize().map_err(|e| {
                    Error::CouldNotDetermineCanonicalManifestPath(dep_dir.to_path_buf().into(), e)
                })?;
                // Only record intra-workspace deps (i.e., the dep is also a member).
                if member_dirs.contains(&canonical_dep_dir) {
                    dependencies.push(canonical_dep_dir);
                }
            }
        }

        crates.push(ResolvedCrateExecution {
            manifest_dir: member.manifest_dir.clone(),
            dependencies,
        });
    }

    Ok(crates)
}

/// Computes which other selected workspaces the given workspace depends on
/// (i.e., any member of this workspace depends on a crate in another selected
/// workspace).
fn compute_inter_workspace_deps(
    workspace_dir: &Path,
    workspace_packages: &HashMap<PathBuf, Vec<WorkspaceMemberInfo>>,
    all_packages: &HashMap<PackageId, cargo_metadata::Package>,
    package_name_to_id: &HashMap<String, PackageId>,
    selected_set: &HashSet<&PathBuf>,
    canonical_selected: &[PathBuf],
) -> Vec<PathBuf> {
    let Some(members) = workspace_packages.get(workspace_dir) else {
        return Vec::new();
    };

    // Build a map from member manifest_dir → workspace manifest_dir for all
    // members of all selected workspaces.
    let mut crate_to_workspace: HashMap<&PathBuf, &PathBuf> = HashMap::new();
    for ws_dir in canonical_selected {
        if let Some(ws_members) = workspace_packages.get(ws_dir) {
            for member in ws_members {
                crate_to_workspace.insert(&member.manifest_dir, ws_dir);
            }
        }
    }

    let mut dep_workspaces: HashSet<PathBuf> = HashSet::new();

    for member in members {
        let Some(package) = all_packages.get(&member.package_id) else {
            continue;
        };

        for dep in &package.dependencies {
            let Some(dep_id) = package_name_to_id.get(&dep.name) else {
                continue;
            };
            let Some(dep_pkg) = all_packages.get(dep_id) else {
                continue;
            };
            let Ok(dep_dir) = dep_pkg
                .manifest_path
                .parent()
                .ok_or(())
                .and_then(|p| p.canonicalize().ok().ok_or(()))
            else {
                continue;
            };

            if let Some(&dep_ws) = crate_to_workspace.get(&dep_dir) {
                // The dep lives in another selected workspace.
                if dep_ws != workspace_dir && selected_set.contains(dep_ws) {
                    dep_workspaces.insert(dep_ws.clone());
                }
            }
        }
    }

    dep_workspaces.into_iter().collect()
}

/// Selects and resolves standalone crates that match any of the given filters.
///
/// "Standalone crates" are crates whose `workspace_manifest_dir` equals their
/// own `manifest_dir` (the crate IS the workspace root).
fn resolve_standalone_crates(
    filters: &[&CrateFilter],
    config: &crate::Config,
) -> Result<Vec<ResolvedCrateExecution>, Error> {
    // Build a map from workspace manifest_dir → is_standalone for filter evaluation.
    let workspace_standalone_map: HashMap<PathBuf, bool> = config
        .workspaces
        .iter()
        .map(|w| (w.manifest_dir.clone(), w.is_standalone))
        .collect();

    // Only consider crates in standalone workspaces for `select crates`.
    let initial_dirs: Vec<PathBuf> = config
        .crates
        .iter()
        .filter(|c| {
            workspace_standalone_map
                .get(&c.workspace_manifest_dir)
                .copied()
                .unwrap_or(false)
        })
        .filter(|c| {
            filters
                .iter()
                .any(|f| crate_matches_filter(c, f, &workspace_standalone_map))
        })
        .map(|c| c.manifest_dir.clone())
        .collect();

    // Canonicalize and build a target set for dep resolution.
    let canonical_dirs: Vec<PathBuf> = initial_dirs
        .iter()
        .map(|d| {
            fs_err::canonicalize(d)
                .map_err(|e| Error::CouldNotDetermineCanonicalManifestPath(d.clone(), e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let target_set: HashSet<&PathBuf> = canonical_dirs.iter().collect();

    if canonical_dirs.is_empty() {
        return Ok(Vec::new());
    }

    // Load cargo metadata for every workspace that contains a selected crate.
    let mut all_packages: HashMap<PackageId, cargo_metadata::Package> = HashMap::new();
    let mut package_name_to_id: HashMap<String, PackageId> = HashMap::new();

    let unique_workspace_roots: HashSet<PathBuf> = config
        .workspaces
        .iter()
        .filter(|w| w.is_standalone)
        .map(|w| w.manifest_dir.clone())
        .collect();

    for ws_root in &unique_workspace_roots {
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(ws_root.join("Cargo.toml"))
            .no_deps()
            .exec()
            .map_err(|e| Error::CargoMetadataError(ws_root.clone(), e))?;

        for package in metadata.packages {
            package_name_to_id.insert(package.name.to_string(), package.id.clone());
            all_packages.insert(package.id.clone(), package);
        }
    }

    // For each selected crate, find its intra-target-set dependencies.
    crate_executions_from_dirs(
        &canonical_dirs,
        &target_set,
        &all_packages,
        &package_name_to_id,
    )
}

/// Builds [`ResolvedCrateExecution`] entries for the given canonical manifest
/// directories, resolving intra-set dependencies via `cargo metadata` data.
fn crate_executions_from_dirs(
    canonical_dirs: &[PathBuf],
    target_set: &HashSet<&PathBuf>,
    all_packages: &HashMap<PackageId, cargo_metadata::Package>,
    package_name_to_id: &HashMap<String, PackageId>,
) -> Result<Vec<ResolvedCrateExecution>, Error> {
    let mut results: Vec<ResolvedCrateExecution> = Vec::new();

    for canonical_dir in canonical_dirs {
        // Find which package corresponds to this manifest directory.
        let package_id = package_name_to_id
            .iter()
            .find_map(|(_name, id)| {
                let package = all_packages.get(id)?;
                let pkg_dir = package.manifest_path.parent()?;
                let canonical_pkg_dir = fs_err::canonicalize(pkg_dir).ok()?;
                if canonical_pkg_dir == *canonical_dir {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(canonical_dir.clone())
            })?;

        let package = all_packages.get(&package_id).ok_or_else(|| {
            Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(canonical_dir.clone())
        })?;

        let mut dependencies: Vec<PathBuf> = Vec::new();
        for dep in &package.dependencies {
            let Some(dep_id) = package_name_to_id.get(&dep.name) else {
                continue;
            };
            let Some(dep_pkg) = all_packages.get(dep_id) else {
                continue;
            };
            let dep_dir = dep_pkg.manifest_path.parent().ok_or_else(|| {
                Error::ManifestPathHasNoParentDir(dep_pkg.manifest_path.clone().into_std_path_buf())
            })?;
            let canonical_dep_dir = dep_dir.canonicalize().map_err(|e| {
                Error::CouldNotDetermineCanonicalManifestPath(dep_dir.to_path_buf().into(), e)
            })?;
            if target_set.contains(&canonical_dep_dir) {
                dependencies.push(canonical_dep_dir);
            }
        }

        results.push(ResolvedCrateExecution {
            manifest_dir: canonical_dir.clone(),
            dependencies,
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helper functions use panic! on unexpected failures"
    )]
    #![expect(
        clippy::indexing_slicing,
        reason = "test code indexes known positions in resolved structures"
    )]

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::program::parser::parse;
    use crate::utils::execute_command;
    use tempfile::tempdir;

    /// Parses a program, resolving it against the given config.
    fn resolve_ok(src: &str, config: &crate::Config) -> ResolvedProgram {
        let program = parse(src, "<test>").unwrap_or_else(|errs| {
            panic!(
                "parse error:\n{}",
                errs.iter()
                    .map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
        resolve_program(&program, config).unwrap_or_else(|e| {
            panic!("resolve error: {e}");
        })
    }

    fn empty_config() -> crate::Config {
        crate::Config {
            workspaces: vec![],
            crates: vec![],
        }
    }

    #[test]
    fn empty_program_resolves_to_empty() {
        let resolved = resolve_ok("", &empty_config());
        assert!(resolved.workspace_executions.is_empty());
        assert!(resolved.crate_executions.is_empty());
    }

    #[test]
    fn select_workspaces_no_registered_workspaces() {
        let resolved = resolve_ok("select workspaces;", &empty_config());
        assert!(resolved.workspace_executions.is_empty());
    }

    #[tokio::test]
    async fn select_workspaces_all() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let environment = crate::Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        // Create a minimal standalone workspace.
        let ws_dir = temp_path.join("myws");
        fs_err::create_dir_all(&ws_dir)?;
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&ws_dir)
            .args(["init", "--name", "myws", "--lib"]);
        execute_command(&mut cmd, &environment, &ws_dir)?;

        // Register it.
        let options = crate::Options {
            command: crate::Command::Target(crate::targets::TargetParameters {
                sub_command: crate::targets::TargetSubCommand::Add(crate::targets::AddParameters {
                    manifest_path: ws_dir.join("Cargo.toml"),
                }),
            }),
        };
        crate::run_app(options, environment.clone()).await?;

        let config = crate::Config::load(&environment)?;
        let resolved = resolve_ok("select workspaces;", &config);

        assert_eq!(resolved.workspace_executions.len(), 1);
        assert_eq!(
            resolved.workspace_executions[0].manifest_dir,
            fs_err::canonicalize(&ws_dir)?
        );
        Ok(())
    }

    #[tokio::test]
    async fn select_workspaces_where_standalone() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let environment = crate::Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        // Create one standalone workspace.
        let standalone_dir = temp_path.join("standalone");
        fs_err::create_dir_all(&standalone_dir)?;
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&standalone_dir)
            .args(["init", "--name", "standalone", "--lib"]);
        execute_command(&mut cmd, &environment, &standalone_dir)?;

        // Create one multi-crate workspace.
        let ws_dir = temp_path.join("multi");
        fs_err::create_dir_all(&ws_dir)?;
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate_a\"]\nresolver = \"2\"\n",
        )?;
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&ws_dir).args(["new", "--lib", "crate_a"]);
        execute_command(&mut cmd, &environment, &ws_dir)?;

        // Register both.
        for manifest in [standalone_dir.join("Cargo.toml"), ws_dir.join("Cargo.toml")] {
            let options = crate::Options {
                command: crate::Command::Target(crate::targets::TargetParameters {
                    sub_command: crate::targets::TargetSubCommand::Add(
                        crate::targets::AddParameters {
                            manifest_path: manifest,
                        },
                    ),
                }),
            };
            crate::run_app(options, environment.clone()).await?;
        }

        let config = crate::Config::load(&environment)?;

        // select workspaces where standalone — should only return the standalone one.
        let resolved = resolve_ok("select workspaces where standalone;", &config);
        assert_eq!(resolved.workspace_executions.len(), 1);
        assert_eq!(
            resolved.workspace_executions[0].manifest_dir,
            fs_err::canonicalize(&standalone_dir)?
        );

        // select workspaces where !standalone — should only return the multi-crate one.
        let resolved2 = resolve_ok("select workspaces where !standalone;", &config);
        assert_eq!(resolved2.workspace_executions.len(), 1);
        assert_eq!(
            resolved2.workspace_executions[0].manifest_dir,
            fs_err::canonicalize(&ws_dir)?
        );

        Ok(())
    }

    #[tokio::test]
    async fn workspace_member_crates_resolved() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let environment = crate::Environment::mock(&temp_dir)?;
        let temp_path = temp_dir.path();

        let ws_dir = temp_path.join("ws");
        fs_err::create_dir_all(&ws_dir)?;
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate_a\", \"crate_b\"]\nresolver = \"2\"\n",
        )?;
        for name in &["crate_a", "crate_b"] {
            let mut cmd = std::process::Command::new("cargo");
            cmd.current_dir(&ws_dir).args(["new", "--lib", name]);
            execute_command(&mut cmd, &environment, &ws_dir)?;
        }

        let options = crate::Options {
            command: crate::Command::Target(crate::targets::TargetParameters {
                sub_command: crate::targets::TargetSubCommand::Add(crate::targets::AddParameters {
                    manifest_path: ws_dir.join("Cargo.toml"),
                }),
            }),
        };
        crate::run_app(options, environment.clone()).await?;

        let config = crate::Config::load(&environment)?;
        let resolved = resolve_ok("select workspaces;", &config);

        assert_eq!(resolved.workspace_executions.len(), 1);
        // The workspace should have 2 member crates.
        assert_eq!(resolved.workspace_executions[0].member_crates.len(), 2);

        // All member crate manifest dirs must be canonical.
        for member in &resolved.workspace_executions[0].member_crates {
            let canonical = fs_err::canonicalize(&member.manifest_dir)?;
            assert_eq!(member.manifest_dir, canonical);
        }
        Ok(())
    }
}
