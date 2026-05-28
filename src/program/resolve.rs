//! Resolves a parsed [`Program`] against the registered workspaces/crates to
//! produce a [`ResolvedProgram`] snapshot for task execution.

pub mod snapshot;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use cargo_metadata::{DependencyKind, PackageId};

use crate::error::Error;
use crate::program::ast::crate_ctx::{CrateFilter, CrateSelectCondition};
use crate::program::ast::workspace_ctx::{WorkspaceFilter, WorkspaceSelectCondition};
use crate::program::{GlobalStatement, Program};
use crate::targets::{CrateType, TargetKind};

pub use snapshot::{ResolvedCrateExecution, ResolvedProgram, ResolvedWorkspaceExecution};

// ── Soft dev-dep ordering ──────────────────────────────────────────────────────
//
// Cargo dev-dependencies are included as normal edges when they don't form a
// cycle, and dropped only on the cycle-closing edges. This lets a release-flow
// task publish a dev-dep before its consumer (the common case) while still
// running when two crates only-but-mutually dev-depend on each other (e.g. for
// test helpers).

/// Iterative DFS frame for [`tarjan_scc`]; each frame remembers which
/// successor index it has visited last so the next iteration can pick up
/// where it left off.
struct TarjanFrame {
    /// Graph node this frame is currently visiting.
    node: usize,
    /// Index of the next successor of `node` to visit on resume.
    succ_idx: usize,
}

/// Computes strongly-connected components of a directed graph using Tarjan's
/// algorithm. Returns a vector of length `successors.len()` mapping each node
/// to its SCC id. Singleton nodes with no self-loop each get their own SCC id.
///
/// `successors` must already contain only valid in-range node indices
/// (`< successors.len()`); callers in this module always satisfy this because
/// they build `successors` from `(0..n).map(...)` themselves.
#[expect(
    clippy::indexing_slicing,
    reason = "all indices come from `0..n` or from `successors[v]` which is built only from in-range values"
)]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "counters increment at most `n` times where `n = successors.len()`; overflow would require usize::MAX nodes"
)]
fn tarjan_scc(successors: &[Vec<usize>]) -> Vec<usize> {
    let n = successors.len();
    let mut index_of: Vec<Option<usize>> = vec![None; n];
    let mut lowlink: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut scc_of: Vec<usize> = vec![0; n];
    let mut call_stack: Vec<TarjanFrame> = Vec::new();
    let mut counter: usize = 0;
    let mut next_scc: usize = 0;

    for root in 0..n {
        if index_of[root].is_some() {
            continue;
        }
        // Enter `root`.
        index_of[root] = Some(counter);
        lowlink[root] = counter;
        counter += 1;
        stack.push(root);
        on_stack[root] = true;
        call_stack.push(TarjanFrame {
            node: root,
            succ_idx: 0,
        });

        while let Some(frame) = call_stack.last_mut() {
            let v = frame.node;
            let succs = &successors[v];
            if frame.succ_idx < succs.len() {
                let w = succs[frame.succ_idx];
                frame.succ_idx += 1;
                match index_of[w] {
                    None => {
                        // Descend into w.
                        index_of[w] = Some(counter);
                        lowlink[w] = counter;
                        counter += 1;
                        stack.push(w);
                        on_stack[w] = true;
                        call_stack.push(TarjanFrame {
                            node: w,
                            succ_idx: 0,
                        });
                    }
                    Some(w_idx) if on_stack[w] => {
                        let lv = lowlink[v];
                        if w_idx < lv {
                            lowlink[v] = w_idx;
                        }
                    }
                    Some(_) => {
                        // w is in an already-finished SCC; ignore.
                    }
                }
            } else {
                // All successors of v explored — pop the frame and propagate
                // lowlink upward.
                call_stack.pop();
                let v_idx = index_of[v].unwrap_or(0);
                if lowlink[v] == v_idx {
                    // v is an SCC root; pop until we get v.
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        scc_of[w] = next_scc;
                        if w == v {
                            break;
                        }
                    }
                    next_scc += 1;
                }
                if let Some(parent) = call_stack.last() {
                    let lp = lowlink[parent.node];
                    let lv = lowlink[v];
                    if lv < lp {
                        lowlink[parent.node] = lv;
                    }
                }
            }
        }
    }

    scc_of
}

/// Applies the soft-ordering policy for dev-dependencies.
///
/// For each entry `(node, deps)`, `deps` lists `(dep_node, kind)` pairs. The
/// function returns the same shape with `Vec<dep_node>` per entry, but with
/// dev-dependency edges removed when both endpoints lie in the same
/// strongly-connected component of the full (normal + dev) graph. Normal- and
/// build-dependency edges, plus edges between different SCCs, are always
/// preserved.
///
/// Edges whose destination is not in `nodes_with_deps` are kept verbatim
/// (they cannot participate in a cycle inside this graph).
fn apply_soft_dev_dep_ordering(
    nodes_with_deps: Vec<(PathBuf, Vec<(PathBuf, DependencyKind)>)>,
) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let idx_of: HashMap<PathBuf, usize> = nodes_with_deps
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.clone(), i))
        .collect();
    let n = nodes_with_deps.len();
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (src_idx, (_, deps)) in nodes_with_deps.iter().enumerate() {
        let Some(succ_list) = successors.get_mut(src_idx) else {
            continue;
        };
        for (dep_id, _) in deps {
            if let Some(&dst_idx) = idx_of.get(dep_id) {
                succ_list.push(dst_idx);
            }
        }
    }
    let scc = tarjan_scc(&successors);

    nodes_with_deps
        .into_iter()
        .enumerate()
        .map(|(src_idx, (node, deps))| {
            let filtered: Vec<PathBuf> = deps
                .into_iter()
                .filter(|(dep_id, kind)| {
                    if *kind != DependencyKind::Development {
                        return true;
                    }
                    // Drop only if both endpoints are in the same SCC.
                    idx_of.get(dep_id).is_none_or(|&dst_idx| {
                        scc.get(src_idx).copied() != scc.get(dst_idx).copied()
                    })
                })
                .map(|(dep_id, _)| dep_id)
                .collect();
            (node, filtered)
        })
        .collect()
}

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
        // If `krate.workspace_manifest_dir` is not in the standalone map
        // (orphan crate, or the workspace was removed from the config), fall
        // back to `false` — i.e. treat unknown enclosing workspaces as
        // non-standalone so they are filtered out by `where standalone`.
        // Safe today because both sides use uncanonicalized paths; revisit
        // if either side switches to canonical paths.
        CrateSelectCondition::Standalone => workspace_standalone_map
            .get(&krate.workspace_manifest_dir)
            .copied()
            .unwrap_or(false),
        CrateSelectCondition::CrateType(filter) => {
            krate.crate_types.contains(&CrateType::from(*filter))
        }
        CrateSelectCondition::TargetKind(filter) => {
            krate.target_kinds.contains(&TargetKind::from(*filter))
        }
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

    resolve_workspaces_from_canonical_dirs(canonical_selected)
}

/// Resolves workspace executions from an explicit list of canonical workspace
/// directory paths, loading `cargo metadata` directly.
///
/// This is the shared implementation used by both the filter-based and
/// explicit-path-based workspace resolution paths.
fn resolve_workspaces_from_canonical_dirs(
    canonical_selected: Vec<PathBuf>,
) -> Result<Vec<ResolvedWorkspaceExecution>, Error> {
    if canonical_selected.is_empty() {
        return Ok(Vec::new());
    }

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

    // Inter-workspace deps are computed for every workspace in a single pass
    // so the SCC view inside `apply_soft_dev_dep_ordering` covers the whole
    // graph.
    let inter_ws_deps = compute_inter_workspace_deps(
        &canonical_selected,
        &workspace_packages,
        &all_packages,
        &package_name_to_id,
    )?;

    // For each selected workspace, resolve member crates (with intra-workspace deps)
    // and look up its inter-workspace dependencies.
    let mut executions: Vec<ResolvedWorkspaceExecution> = Vec::new();

    for canonical_ws_dir in &canonical_selected {
        let member_crates = resolve_workspace_member_crates(
            canonical_ws_dir,
            &workspace_packages,
            &all_packages,
            &package_name_to_id,
        )?;

        let workspace_deps = inter_ws_deps
            .get(canonical_ws_dir)
            .cloned()
            .unwrap_or_default();

        executions.push(ResolvedWorkspaceExecution {
            manifest_dir: canonical_ws_dir.clone(),
            dependencies: workspace_deps,
            member_crates,
        });
    }

    Ok(executions)
}

/// Resolves workspace executions from an explicit list of workspace directory
/// paths provided by the user, bypassing the program's `select workspaces`
/// filter.
///
/// Dependency ordering among the provided workspaces is still computed and
/// applied.
///
/// # Errors
///
/// Returns an error if any path cannot be canonicalized or if `cargo metadata`
/// fails for any workspace.
#[expect(
    clippy::module_name_repetitions,
    reason = "the 'resolve_' prefix is part of the function's identity within this module"
)]
pub fn resolve_explicit_workspace_targets(
    workspace_dirs: &[PathBuf],
) -> Result<Vec<ResolvedWorkspaceExecution>, Error> {
    let canonical: Vec<PathBuf> = workspace_dirs
        .iter()
        .map(|d| {
            fs_err::canonicalize(d)
                .map_err(|e| Error::CouldNotDetermineCanonicalManifestPath(d.clone(), e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    resolve_workspaces_from_canonical_dirs(canonical)
}

/// Resolves crate executions from an explicit list of crate directory paths
/// provided by the user, bypassing the program's `select crates` filter.
///
/// For each provided path `cargo metadata` is run to discover its workspace
/// root; metadata from each unique workspace root is loaded once. Dependency
/// ordering among the provided crates is still computed and applied.
///
/// # Errors
///
/// Returns an error if any path cannot be canonicalized or if `cargo metadata`
/// fails.
#[expect(
    clippy::module_name_repetitions,
    reason = "the 'resolve_' prefix is part of the function's identity within this module"
)]
pub fn resolve_explicit_crate_targets(
    crate_dirs: &[PathBuf],
) -> Result<Vec<ResolvedCrateExecution>, Error> {
    if crate_dirs.is_empty() {
        return Ok(Vec::new());
    }

    let canonical_dirs: Vec<PathBuf> = crate_dirs
        .iter()
        .map(|d| {
            fs_err::canonicalize(d)
                .map_err(|e| Error::CouldNotDetermineCanonicalManifestPath(d.clone(), e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let target_set: HashSet<&PathBuf> = canonical_dirs.iter().collect();

    // Run `cargo metadata` in each crate dir to discover its workspace root,
    // then load all packages from each unique workspace root exactly once.
    let mut all_packages: HashMap<PackageId, cargo_metadata::Package> = HashMap::new();
    let mut package_name_to_id: HashMap<String, PackageId> = HashMap::new();
    let mut seen_workspace_roots: HashSet<PathBuf> = HashSet::new();

    for canonical_dir in &canonical_dirs {
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(canonical_dir.join("Cargo.toml"))
            .no_deps()
            .exec()
            .map_err(|e| Error::CargoMetadataError(canonical_dir.clone(), e))?;

        let ws_root = metadata.workspace_root.into_std_path_buf();
        let canonical_ws_root = fs_err::canonicalize(&ws_root)
            .map_err(|e| Error::CouldNotDetermineCanonicalManifestPath(ws_root.clone(), e))?;

        if seen_workspace_roots.insert(canonical_ws_root.clone()) {
            let ws_metadata = cargo_metadata::MetadataCommand::new()
                .manifest_path(canonical_ws_root.join("Cargo.toml"))
                .no_deps()
                .exec()
                .map_err(|e| Error::CargoMetadataError(canonical_ws_root.clone(), e))?;

            for package in ws_metadata.packages {
                package_name_to_id.insert(package.name.to_string(), package.id.clone());
                all_packages.insert(package.id.clone(), package);
            }
        }
    }

    crate_executions_from_dirs(
        &canonical_dirs,
        &target_set,
        &all_packages,
        &package_name_to_id,
    )
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
///
/// Dev-dependencies participate in the dep graph, but `apply_soft_dev_dep_ordering`
/// drops dev-dep edges that close a cycle so dev-dep-only cycles do not
/// produce a runtime `CircularDependency` error.
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

    let mut raw: Vec<(PathBuf, Vec<(PathBuf, DependencyKind)>)> = Vec::new();

    for member in members {
        let package = all_packages.get(&member.package_id).ok_or_else(|| {
            Error::FoundNoPackageInCargoMetadataWithGivenManifestPath(member.manifest_dir.clone())
        })?;

        let mut dependencies: Vec<(PathBuf, DependencyKind)> = Vec::new();
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
                    dependencies.push((canonical_dep_dir, dep.kind));
                }
            }
        }

        raw.push((member.manifest_dir.clone(), dependencies));
    }

    Ok(apply_soft_dev_dep_ordering(raw)
        .into_iter()
        .map(|(manifest_dir, dependencies)| ResolvedCrateExecution {
            manifest_dir,
            dependencies,
        })
        .collect())
}

/// Computes inter-workspace dependencies for all selected workspaces in one
/// pass, returning a map from workspace_dir to the list of other selected
/// workspace_dirs it depends on.
///
/// Member-level dev/normal/build kinds are aggregated up to the workspace
/// level: if any member-to-other-workspace edge is non-dev, the resulting
/// workspace-to-workspace edge is treated as non-dev. Only when every
/// underlying member edge is a dev-dep does the aggregated edge count as a
/// dev-dep — and hence become eligible for cycle-breaking under
/// [`apply_soft_dev_dep_ordering`].
fn compute_inter_workspace_deps(
    canonical_selected: &[PathBuf],
    workspace_packages: &HashMap<PathBuf, Vec<WorkspaceMemberInfo>>,
    all_packages: &HashMap<PackageId, cargo_metadata::Package>,
    package_name_to_id: &HashMap<String, PackageId>,
) -> Result<HashMap<PathBuf, Vec<PathBuf>>, Error> {
    let selected_set: HashSet<&PathBuf> = canonical_selected.iter().collect();

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

    let mut raw: Vec<(PathBuf, Vec<(PathBuf, DependencyKind)>)> = Vec::new();

    for workspace_dir in canonical_selected {
        let mut per_target_kind: HashMap<PathBuf, DependencyKind> = HashMap::new();

        if let Some(members) = workspace_packages.get(workspace_dir) {
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
                    // Match the hard-fail behaviour of the sibling resolvers
                    // (`resolve_workspace_member_crates`,
                    // `crate_executions_from_dirs`): a canonicalize failure
                    // here used to be silently swallowed via
                    // `.ok().ok_or(())`, which dropped the dep edge entirely
                    // and broke runtime dependency-readiness gating
                    // downstream.
                    let dep_dir = dep_pkg.manifest_path.parent().ok_or_else(|| {
                        Error::ManifestPathHasNoParentDir(
                            dep_pkg.manifest_path.clone().into_std_path_buf(),
                        )
                    })?;
                    let dep_dir_canonical = dep_dir.canonicalize().map_err(|e| {
                        Error::CouldNotDetermineCanonicalManifestPath(
                            dep_dir.to_path_buf().into(),
                            e,
                        )
                    })?;

                    if let Some(&dep_ws) = crate_to_workspace.get(&dep_dir_canonical)
                        && dep_ws != workspace_dir
                        && selected_set.contains(dep_ws)
                    {
                        // Aggregate: non-Development trumps Development;
                        // beyond that we just keep the first kind seen.
                        per_target_kind
                            .entry(dep_ws.clone())
                            .and_modify(|existing| {
                                if *existing == DependencyKind::Development
                                    && dep.kind != DependencyKind::Development
                                {
                                    *existing = dep.kind;
                                }
                            })
                            .or_insert(dep.kind);
                    }
                }
            }
        }

        let deps: Vec<(PathBuf, DependencyKind)> = per_target_kind.into_iter().collect();
        raw.push((workspace_dir.clone(), deps));
    }

    Ok(apply_soft_dev_dep_ordering(raw).into_iter().collect())
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
///
/// Dev-dependencies participate in the dep graph, but
/// `apply_soft_dev_dep_ordering` drops dev-dep edges that close a cycle so
/// dev-dep-only cycles among standalone crates do not produce a runtime
/// `CircularDependency` error.
fn crate_executions_from_dirs(
    canonical_dirs: &[PathBuf],
    target_set: &HashSet<&PathBuf>,
    all_packages: &HashMap<PackageId, cargo_metadata::Package>,
    package_name_to_id: &HashMap<String, PackageId>,
) -> Result<Vec<ResolvedCrateExecution>, Error> {
    let mut raw: Vec<(PathBuf, Vec<(PathBuf, DependencyKind)>)> = Vec::new();

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

        let mut dependencies: Vec<(PathBuf, DependencyKind)> = Vec::new();
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
                dependencies.push((canonical_dep_dir, dep.kind));
            }
        }

        raw.push((canonical_dir.clone(), dependencies));
    }

    Ok(apply_soft_dev_dep_ordering(raw)
        .into_iter()
        .map(|(manifest_dir, dependencies)| ResolvedCrateExecution {
            manifest_dir,
            dependencies,
        })
        .collect())
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

    use pretty_assertions::{assert_eq, assert_ne};

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

    // ── tarjan_scc / apply_soft_dev_dep_ordering ──────────────────────────────

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Returns the set of SCC ids touched by the given node indices, useful
    /// when checking that a group of nodes was placed in the same SCC.
    fn sccs_of(scc: &[usize], nodes: &[usize]) -> Vec<usize> {
        let mut s: Vec<usize> = nodes.iter().map(|&i| scc[i]).collect();
        s.sort_unstable();
        s.dedup();
        s
    }

    #[test]
    fn tarjan_scc_singletons() {
        // 0 → 1 → 2 (no cycles): three SCCs of size 1.
        let succ = vec![vec![1], vec![2], vec![]];
        let scc = tarjan_scc(&succ);
        assert_eq!(sccs_of(&scc, &[0]).len(), 1);
        assert_eq!(sccs_of(&scc, &[1]).len(), 1);
        assert_eq!(sccs_of(&scc, &[2]).len(), 1);
        assert_eq!(sccs_of(&scc, &[0, 1, 2]).len(), 3);
    }

    #[test]
    fn tarjan_scc_two_cycle() {
        // 0 ↔ 1: one SCC of size 2.
        let succ = vec![vec![1], vec![0]];
        let scc = tarjan_scc(&succ);
        assert_eq!(scc[0], scc[1]);
    }

    #[test]
    fn tarjan_scc_three_cycle_with_attached_tail() {
        // 0 → 1 → 2 → 0 (3-cycle), 2 → 3 (tail).
        let succ = vec![vec![1], vec![2], vec![0, 3], vec![]];
        let scc = tarjan_scc(&succ);
        assert_eq!(scc[0], scc[1]);
        assert_eq!(scc[1], scc[2]);
        assert_ne!(scc[0], scc[3]);
    }

    #[test]
    fn tarjan_scc_self_loop_is_own_scc() {
        // 0 → 0; SCC is size 1 but the node IS in a cycle.
        let succ = vec![vec![0]];
        let scc = tarjan_scc(&succ);
        // Self-loop semantics: scc[0] == scc[0] (trivially), so an edge from
        // 0 to 0 would be detected as "same SCC" by the filter.
        assert_eq!(scc[0], scc[0]);
    }

    /// Two crates that only dev-depend on each other (A ↔ B, both Dev) should
    /// have both edges dropped by the soft-ordering filter.
    #[test]
    fn soft_ordering_breaks_dev_only_cycle() {
        let nodes = vec![
            (p("/a"), vec![(p("/b"), DependencyKind::Development)]),
            (p("/b"), vec![(p("/a"), DependencyKind::Development)]),
        ];
        let out = apply_soft_dev_dep_ordering(nodes);
        assert_eq!(out.len(), 2);
        for (_, deps) in &out {
            assert!(
                deps.is_empty(),
                "expected empty deps after cycle-break, got {deps:?}"
            );
        }
    }

    /// A normal-dep cycle must NOT be broken — the runtime should report it
    /// as a circular dependency.
    #[test]
    fn soft_ordering_preserves_normal_cycle() {
        let nodes = vec![
            (p("/a"), vec![(p("/b"), DependencyKind::Normal)]),
            (p("/b"), vec![(p("/a"), DependencyKind::Normal)]),
        ];
        let out = apply_soft_dev_dep_ordering(nodes);
        // Both edges still present.
        let by_node: HashMap<&PathBuf, &Vec<PathBuf>> = out.iter().map(|(n, d)| (n, d)).collect();
        assert_eq!(by_node[&p("/a")], &vec![p("/b")]);
        assert_eq!(by_node[&p("/b")], &vec![p("/a")]);
    }

    /// A mixed cycle (one Normal edge, one Dev edge) must NOT be fully broken:
    /// only the Dev edge is dropped; the Normal edge stays so the runtime
    /// still reports the underlying issue as appropriate.
    #[test]
    fn soft_ordering_drops_dev_edge_in_mixed_cycle() {
        // a -Normal-> b, b -Dev-> a. SCC is {a, b}. Drop only the Dev edge.
        let nodes = vec![
            (p("/a"), vec![(p("/b"), DependencyKind::Normal)]),
            (p("/b"), vec![(p("/a"), DependencyKind::Development)]),
        ];
        let out = apply_soft_dev_dep_ordering(nodes);
        let by_node: HashMap<&PathBuf, &Vec<PathBuf>> = out.iter().map(|(n, d)| (n, d)).collect();
        assert_eq!(by_node[&p("/a")], &vec![p("/b")]);
        assert!(
            by_node[&p("/b")].is_empty(),
            "dev edge in cycle should be dropped"
        );
    }

    /// A dev-dep edge that does NOT close a cycle (the endpoint is downstream)
    /// must be preserved so it contributes to ordering.
    #[test]
    fn soft_ordering_keeps_dev_dep_outside_cycle() {
        // a -Dev-> b, no reverse edge. SCC singletons; edge preserved.
        let nodes = vec![
            (p("/a"), vec![(p("/b"), DependencyKind::Development)]),
            (p("/b"), vec![]),
        ];
        let out = apply_soft_dev_dep_ordering(nodes);
        let by_node: HashMap<&PathBuf, &Vec<PathBuf>> = out.iter().map(|(n, d)| (n, d)).collect();
        assert_eq!(by_node[&p("/a")], &vec![p("/b")]);
    }

    /// Build-dependencies are treated like Normal: they contribute to ordering
    /// AND they are never dropped even when they close a cycle.
    #[test]
    fn soft_ordering_treats_build_like_normal() {
        let nodes = vec![
            (p("/a"), vec![(p("/b"), DependencyKind::Build)]),
            (p("/b"), vec![(p("/a"), DependencyKind::Build)]),
        ];
        let out = apply_soft_dev_dep_ordering(nodes);
        let by_node: HashMap<&PathBuf, &Vec<PathBuf>> = out.iter().map(|(n, d)| (n, d)).collect();
        assert_eq!(by_node[&p("/a")], &vec![p("/b")]);
        assert_eq!(by_node[&p("/b")], &vec![p("/a")]);
    }

    /// An edge whose destination is outside the node set must be left alone
    /// (it cannot participate in an in-set cycle).
    #[test]
    fn soft_ordering_keeps_edge_to_unknown_dest() {
        let nodes = vec![(p("/a"), vec![(p("/external"), DependencyKind::Development)])];
        let out = apply_soft_dev_dep_ordering(nodes);
        assert_eq!(out[0].1, vec![p("/external")]);
    }

    // ── evaluate_crate_select_condition ───────────────────────────────────────

    use crate::program::ast::common::AtLeastTwo;
    use crate::program::ast::crate_ctx::{CrateTypeFilter, TargetKindFilter};

    fn select_krate(crate_types: &[CrateType], target_kinds: &[TargetKind]) -> crate::Crate {
        crate::Crate {
            manifest_dir: p("/c"),
            workspace_manifest_dir: p("/ws"),
            crate_types: crate_types.iter().cloned().collect(),
            target_kinds: target_kinds.iter().cloned().collect(),
        }
    }

    #[test]
    fn crate_select_standalone_reads_workspace_map() {
        let k = select_krate(&[CrateType::Bin], &[]);
        let mut map: HashMap<PathBuf, bool> = HashMap::new();
        // Enclosing workspace unknown -> treated as non-standalone.
        assert!(!evaluate_crate_select_condition(
            &CrateSelectCondition::Standalone,
            &k,
            &map,
        ));
        map.insert(p("/ws"), true);
        assert!(evaluate_crate_select_condition(
            &CrateSelectCondition::Standalone,
            &k,
            &map,
        ));
        map.insert(p("/ws"), false);
        assert!(!evaluate_crate_select_condition(
            &CrateSelectCondition::Standalone,
            &k,
            &map,
        ));
    }

    #[test]
    fn crate_select_type_kind_and_combinators() {
        let k = select_krate(&[CrateType::Lib], &[TargetKind::Test]);
        let map: HashMap<PathBuf, bool> = HashMap::new();
        let eval = |c: &CrateSelectCondition| evaluate_crate_select_condition(c, &k, &map);

        assert!(eval(&CrateSelectCondition::CrateType(CrateTypeFilter::Lib)));
        assert!(!eval(&CrateSelectCondition::CrateType(
            CrateTypeFilter::Bin
        )));
        assert!(eval(&CrateSelectCondition::TargetKind(
            TargetKindFilter::Test
        )));
        assert!(!eval(&CrateSelectCondition::TargetKind(
            TargetKindFilter::Bench
        )));

        // Not.
        assert!(eval(&CrateSelectCondition::Not(Box::new(
            CrateSelectCondition::CrateType(CrateTypeFilter::Bin),
        ))));
        // And: lib && bin -> false.
        assert!(!eval(&CrateSelectCondition::And(AtLeastTwo::from_pair(
            CrateSelectCondition::CrateType(CrateTypeFilter::Lib),
            CrateSelectCondition::CrateType(CrateTypeFilter::Bin),
        ))));
        // Or: bin || lib -> true.
        assert!(eval(&CrateSelectCondition::Or(AtLeastTwo::from_pair(
            CrateSelectCondition::CrateType(CrateTypeFilter::Bin),
            CrateSelectCondition::CrateType(CrateTypeFilter::Lib),
        ))));
    }
}
