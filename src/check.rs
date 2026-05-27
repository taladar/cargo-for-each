//! Read-only audit of the `cargo-for-each` install.
//!
//! `cargo-for-each check` walks the configuration and disk state and reports
//! drift without mutating anything.  It is the diagnostic counterpart to
//! [`crate::targets::refresh_command`], which detects the same kinds of
//! issues but bundles detection with mutation.
//!
//! The command prints findings grouped by category with severity and
//! remediation hints, then returns [`crate::error::Error::CheckFoundIssues`]
//! if any error-severity finding was emitted so the binary exits non-zero.
//! Warnings alone do not fail the command.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::instrument;

use crate::program::cursor::{CursorSegment, ProgramCursor};
use crate::program::resolve::snapshot::ResolvedProgram;
use crate::targets::{CargoMetadataExt as _, CrateType, TargetKind, cargo_toml_present};

// ── CLI parameters ────────────────────────────────────────────────────────────

/// Parameters for the `check` subcommand.
#[expect(
    clippy::module_name_repetitions,
    reason = "public struct in the check module; renaming would be lossy at the call site"
)]
#[derive(clap::Parser, Debug, Clone)]
pub struct CheckParameters {
    /// Run only target-related checks; skip task checks.
    #[clap(long, conflicts_with = "tasks_only")]
    pub targets_only: bool,
    /// Run only task-related checks; skip target checks.
    #[clap(long, conflicts_with = "targets_only")]
    pub tasks_only: bool,
}

// ── Finding types ────────────────────────────────────────────────────────────

/// Severity of a single finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A problem the user almost certainly wants to act on; presence of any
    /// error-severity finding causes `check` to return a non-zero error.
    Error,
    /// A problem worth surfacing but not fatal; warnings alone do not fail
    /// the command.
    Warning,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error => f.write_str("error"),
            Self::Warning => f.write_str("warning"),
        }
    }
}

/// Top-level grouping for findings; printed as a header in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Findings about registered workspaces and crates.
    Targets,
    /// Findings about registered tasks and their on-disk state.
    Tasks,
    /// Findings about the configuration itself (parse errors, lock state).
    Config,
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Targets => f.write_str("Targets"),
            Self::Tasks => f.write_str("Tasks"),
            Self::Config => f.write_str("Config"),
        }
    }
}

/// A single audit finding emitted by `check`.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Severity bucket; controls exit code.
    pub severity: Severity,
    /// Top-level category; controls header grouping.
    pub category: Category,
    /// Human-readable identifier for the affected entity (e.g.
    /// `"workspace /path"`, `"crate /path"`, `"task name"`, `"config"`).
    /// Findings are grouped by this string within a category.
    pub entity: String,
    /// The problem statement.
    pub message: String,
    /// Optional remediation suggestion appended after the message.
    pub hint: Option<String>,
}

// ── Top-level command ────────────────────────────────────────────────────────

/// Run the audit and print findings.
///
/// # Errors
///
/// Returns [`crate::error::Error::CheckFoundIssues`] when any
/// error-severity finding was emitted.  Returns other error variants if
/// the audit itself cannot run (e.g. the config file cannot be opened
/// for reasons other than "does not exist").
#[expect(
    clippy::module_name_repetitions,
    reason = "public command function mirrors target_command / task_command naming"
)]
#[instrument]
pub async fn check_command(
    params: CheckParameters,
    environment: crate::Environment,
) -> Result<(), crate::error::Error> {
    let mut findings: Vec<Finding> = Vec::new();

    // Config parse / availability comes first; if the config can't be loaded
    // at all we report only that and stop, since every later check depends
    // on it.
    let config = match crate::Config::load(&environment) {
        Ok(c) => c,
        Err(err) => {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Config,
                entity: "config".to_owned(),
                message: format!("could not load config: {err}"),
                hint: Some("inspect `cargo-for-each.toml` manually".to_owned()),
            });
            return finalize(findings);
        }
    };

    findings.extend(check_config_lock(&environment));

    if !params.tasks_only {
        findings.extend(check_target_filesystem(&config));
        findings.extend(check_target_cargo_metadata(&config));
    }

    if !params.targets_only {
        findings.extend(check_tasks(&environment, &config));
    }

    finalize(findings)
}

/// Sort, print, and turn findings into the appropriate exit result.
#[expect(clippy::print_stdout, reason = "user-facing output, not logging")]
fn finalize(mut findings: Vec<Finding>) -> Result<(), crate::error::Error> {
    findings.sort_by(|a, b| {
        (a.category, &a.entity, a.severity).cmp(&(b.category, &b.entity, b.severity))
    });

    let mut current_category: Option<Category> = None;
    let mut current_entity: Option<&str> = None;
    let mut errors: usize = 0;
    let mut warnings: usize = 0;

    for finding in &findings {
        match finding.severity {
            Severity::Error => errors = errors.saturating_add(1),
            Severity::Warning => warnings = warnings.saturating_add(1),
        }
        if current_category != Some(finding.category) {
            println!("{}", finding.category);
            current_category = Some(finding.category);
            current_entity = None;
        }
        if current_entity != Some(finding.entity.as_str()) {
            println!("  {}", finding.entity);
            current_entity = Some(finding.entity.as_str());
        }
        if let Some(hint) = &finding.hint {
            println!(
                "    [{severity}] {message} Hint: {hint}.",
                severity = finding.severity,
                message = finding.message,
            );
        } else {
            println!(
                "    [{severity}] {message}",
                severity = finding.severity,
                message = finding.message,
            );
        }
    }

    println!("Summary: {errors} error(s), {warnings} warning(s).");

    if errors > 0 {
        Err(crate::error::Error::CheckFoundIssues { errors, warnings })
    } else {
        Ok(())
    }
}

// ── Config-category checks ───────────────────────────────────────────────────

/// Probe the config lock file to see if it is currently held by a peer process.
///
/// On Unix `flock` is process-bound and released on process exit, so a held
/// lock means there is an actually-running peer — not a stale file.  The
/// finding is therefore informational ("findings may be transient"), not
/// "lock is stale, delete it".
fn check_config_lock(environment: &crate::Environment) -> Vec<Finding> {
    let lock_path = crate::config_dir_path(environment).join("cargo-for-each.lock");
    if !lock_path.exists() {
        return Vec::new();
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        return Vec::new();
    };
    match file.try_lock() {
        Ok(()) => {
            // Acquired immediately: nothing else was holding it.  Drop the
            // file to release before returning.
            drop(file);
            Vec::new()
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            vec![Finding {
                severity: Severity::Warning,
                category: Category::Config,
                entity: "config".to_owned(),
                message: "config lock currently held by another `cargo-for-each` process; \
                          findings may be transient"
                    .to_owned(),
                hint: None,
            }]
        }
        Err(std::fs::TryLockError::Error(_)) => Vec::new(),
    }
}

// ── Target-filesystem checks ─────────────────────────────────────────────────

/// Filesystem-only target checks: missing Cargo.toml files, orphan crates,
/// duplicate registrations.  Does not invoke `cargo metadata`.
fn check_target_filesystem(config: &crate::Config) -> Vec<Finding> {
    let mut findings = Vec::new();

    // (1) Workspace Cargo.toml missing.
    for ws in &config.workspaces {
        if !cargo_toml_present(&ws.manifest_dir) {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Targets,
                entity: format!("workspace {}", ws.manifest_dir.display()),
                message: "Cargo.toml is missing on disk".to_owned(),
                hint: Some("run `cargo-for-each target refresh`".to_owned()),
            });
        }
    }

    // (2) Crate Cargo.toml missing.
    for krate in &config.crates {
        if !cargo_toml_present(&krate.manifest_dir) {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Targets,
                entity: format!("crate {}", krate.manifest_dir.display()),
                message: "Cargo.toml is missing on disk".to_owned(),
                hint: Some("run `cargo-for-each target refresh`".to_owned()),
            });
        }
    }

    // (3) Orphan crate: workspace_manifest_dir not in config.workspaces.
    let workspace_dirs: HashSet<&PathBuf> =
        config.workspaces.iter().map(|w| &w.manifest_dir).collect();
    for krate in &config.crates {
        if !workspace_dirs.contains(&krate.workspace_manifest_dir) {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Targets,
                entity: format!("crate {}", krate.manifest_dir.display()),
                message: format!(
                    "workspace {} is not registered (orphan crate)",
                    krate.workspace_manifest_dir.display()
                ),
                hint: Some("run `cargo-for-each target refresh`".to_owned()),
            });
        }
    }

    // (4) Duplicate registrations within workspaces or crates.
    let mut workspace_counts: BTreeMap<&PathBuf, usize> = BTreeMap::new();
    for ws in &config.workspaces {
        let entry = workspace_counts.entry(&ws.manifest_dir).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    for (dir, count) in workspace_counts {
        if count > 1 {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Targets,
                entity: format!("workspace {}", dir.display()),
                message: format!("registered {count} times"),
                hint: Some("edit `cargo-for-each.toml` to remove duplicates".to_owned()),
            });
        }
    }
    let mut crate_counts: BTreeMap<&PathBuf, usize> = BTreeMap::new();
    for krate in &config.crates {
        let entry = crate_counts.entry(&krate.manifest_dir).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    for (dir, count) in crate_counts {
        if count > 1 {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Targets,
                entity: format!("crate {}", dir.display()),
                message: format!("registered {count} times"),
                hint: Some("edit `cargo-for-each.toml` to remove duplicates".to_owned()),
            });
        }
    }

    findings
}

// ── Target cargo-metadata checks ─────────────────────────────────────────────

/// Run `cargo metadata` once per registered workspace and report drift:
/// `is_standalone` flag, member-set mismatches, stale `crate_types` /
/// `target_kinds`, and metadata invocation failures.
#[expect(
    clippy::too_many_lines,
    reason = "single cohesive walk over workspaces and their members; splitting would force exporting helpers"
)]
fn check_target_cargo_metadata(config: &crate::Config) -> Vec<Finding> {
    let mut findings = Vec::new();

    for ws in &config.workspaces {
        // If the Cargo.toml itself is missing we already reported it in
        // check_target_filesystem; skip the metadata call.
        if !cargo_toml_present(&ws.manifest_dir) {
            continue;
        }
        let manifest_path = ws.manifest_dir.join("Cargo.toml");
        let metadata = match cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .no_deps()
            .exec()
        {
            Ok(m) => m,
            Err(err) => {
                findings.push(Finding {
                    severity: Severity::Error,
                    category: Category::Targets,
                    entity: format!("workspace {}", ws.manifest_dir.display()),
                    message: format!("`cargo metadata` failed: {err}"),
                    hint: Some("inspect the workspace manifest manually".to_owned()),
                });
                continue;
            }
        };

        // Canonicalise the member manifest paths so we compare apples to apples
        // with the stored canonical workspace_manifest_dir / manifest_dir.
        let mut member_canonical: Vec<(cargo_metadata::PackageId, PathBuf)> = Vec::new();
        let mut member_dirs: HashSet<PathBuf> = HashSet::new();
        let mut canonicalization_failed = false;
        for package_id in &metadata.workspace_members {
            let Ok(package) = metadata.get_package_by_id(package_id) else {
                continue;
            };
            let raw = package.manifest_path.clone().into_std_path_buf();
            let canonical = match fs_err::canonicalize(&raw) {
                Ok(p) => p,
                Err(err) => {
                    findings.push(Finding {
                        severity: Severity::Error,
                        category: Category::Targets,
                        entity: format!("workspace {}", ws.manifest_dir.display()),
                        message: format!("could not canonicalize member {}: {err}", raw.display()),
                        hint: Some("inspect the workspace manually".to_owned()),
                    });
                    canonicalization_failed = true;
                    break;
                }
            };
            if let Some(parent) = canonical.parent() {
                member_dirs.insert(parent.to_path_buf());
            }
            member_canonical.push((package_id.clone(), canonical));
        }
        if canonicalization_failed {
            continue;
        }

        let workspace_manifest_path = ws.manifest_dir.join("Cargo.toml");
        let canonical_workspace_manifest = match fs_err::canonicalize(&workspace_manifest_path) {
            Ok(p) => p,
            Err(_) => workspace_manifest_path.clone(),
        };

        // (5) is_standalone drift.  Mirrors the rule in
        // `src/targets.rs:285-288` / `:546`.
        let new_is_standalone = match member_canonical.as_slice() {
            [(_, only_manifest)] => *only_manifest == canonical_workspace_manifest,
            _ => false,
        };
        if new_is_standalone != ws.is_standalone {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Targets,
                entity: format!("workspace {}", ws.manifest_dir.display()),
                message: format!(
                    "is_standalone flag is {stored} but current cargo metadata says {actual}",
                    stored = ws.is_standalone,
                    actual = new_is_standalone,
                ),
                hint: Some("run `cargo-for-each target refresh`".to_owned()),
            });
        }

        // Registered crates that claim this workspace.
        let registered_member_dirs: HashSet<&PathBuf> = config
            .crates
            .iter()
            .filter(|c| c.workspace_manifest_dir == ws.manifest_dir)
            .map(|c| &c.manifest_dir)
            .collect();

        // (6) Registered crate not in current member list.
        for registered in &registered_member_dirs {
            if !member_dirs.contains(*registered) {
                findings.push(Finding {
                    severity: Severity::Error,
                    category: Category::Targets,
                    entity: format!("crate {}", registered.display()),
                    message: format!(
                        "no longer a member of workspace {}",
                        ws.manifest_dir.display()
                    ),
                    hint: Some("run `cargo-for-each target refresh`".to_owned()),
                });
            }
        }

        // (7) New unregistered member appeared on disk.
        for member_dir in &member_dirs {
            if !registered_member_dirs.contains(member_dir) {
                findings.push(Finding {
                    severity: Severity::Error,
                    category: Category::Targets,
                    entity: format!("workspace {}", ws.manifest_dir.display()),
                    message: format!("member {} is not registered", member_dir.display()),
                    hint: Some("run `cargo-for-each target refresh`".to_owned()),
                });
            }
        }

        // (8) Stale crate_types / target_kinds for registered members.
        for (package_id, manifest_path) in &member_canonical {
            let Some(parent) = manifest_path.parent() else {
                continue;
            };
            let Some(registered_crate) = config
                .crates
                .iter()
                .find(|c| c.manifest_dir == parent && c.workspace_manifest_dir == ws.manifest_dir)
            else {
                continue; // covered by (7).
            };
            let Ok(package) = metadata.get_package_by_id(package_id) else {
                continue;
            };
            let current_crate_types = CrateType::from_package(package);
            let current_target_kinds = TargetKind::from_package(package);
            if current_crate_types != registered_crate.crate_types {
                findings.push(Finding {
                    severity: Severity::Warning,
                    category: Category::Targets,
                    entity: format!("crate {}", registered_crate.manifest_dir.display()),
                    message: format!(
                        "crate_types differ from current Cargo.toml ({stored:?} vs {actual:?})",
                        stored = registered_crate.crate_types,
                        actual = current_crate_types,
                    ),
                    hint: Some("run `cargo-for-each target refresh`".to_owned()),
                });
            }
            if current_target_kinds != registered_crate.target_kinds {
                findings.push(Finding {
                    severity: Severity::Warning,
                    category: Category::Targets,
                    entity: format!("crate {}", registered_crate.manifest_dir.display()),
                    message: format!(
                        "target_kinds differ from current Cargo.toml ({stored:?} vs {actual:?})",
                        stored = registered_crate.target_kinds,
                        actual = current_target_kinds,
                    ),
                    hint: Some("run `cargo-for-each target refresh`".to_owned()),
                });
            }
        }
    }

    findings
}

// ── Task checks ──────────────────────────────────────────────────────────────

/// Walk all known tasks (those with a definition dir under
/// `<config_dir>/cargo-for-each/tasks/`) and surface every issue: missing
/// definition files, parse errors, references to unregistered targets,
/// invalid cursors in the state tree, and orphan snapshot dirs.  Also
/// detects state-dir orphans (state without a matching definition).
fn check_tasks(environment: &crate::Environment, config: &crate::Config) -> Vec<Finding> {
    let mut findings = Vec::new();

    let task_def_root = match crate::tasks::dir_path(environment) {
        Ok(p) => p,
        Err(err) => {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Tasks,
                entity: "tasks".to_owned(),
                message: format!("could not determine tasks directory: {err}"),
                hint: None,
            });
            return findings;
        }
    };
    let task_state_root = environment.state_dir.join("cargo-for-each").join("tasks");

    let known_task_names = list_task_names(&task_def_root);

    // (10) State dirs with no matching definition.
    if task_state_root.is_dir()
        && let Ok(entries) = fs_err::read_dir(&task_state_root)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.is_dir() && !known_task_names.iter().any(|n| n == name) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    category: Category::Tasks,
                    entity: format!("task {name}"),
                    message: format!(
                        "state directory exists at {} but no task definition",
                        path.display()
                    ),
                    hint: Some(format!("`rm -rf {}`", path.display())),
                });
            }
        }
    }

    // Per-task checks (11-16).
    for name in &known_task_names {
        let task_dir = task_def_root.join(name);
        let program_path = task_dir.join("program.cfe");
        let resolved_path = task_dir.join("resolved-program.toml");

        let program_present = program_path.is_file();
        if !program_present {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Tasks,
                entity: format!("task {name}"),
                message: "program.cfe is missing".to_owned(),
                hint: Some(format!("`cargo-for-each task remove --name {name}`")),
            });
        }
        let resolved_present = resolved_path.is_file();
        if !resolved_present {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Tasks,
                entity: format!("task {name}"),
                message: "resolved-program.toml is missing".to_owned(),
                hint: Some(format!("`cargo-for-each task remove --name {name}`")),
            });
        }

        // (13) program.cfe parses.
        let parsed_program = if program_present {
            match fs_err::read_to_string(&program_path) {
                Ok(source) => match crate::program::parser::parse(&source, "program.cfe") {
                    Ok(prog) => Some(prog),
                    Err(errors) => {
                        let msg = errors
                            .iter()
                            .take(3)
                            .map(|e| e.as_str().to_owned())
                            .collect::<Vec<_>>()
                            .join("; ");
                        findings.push(Finding {
                            severity: Severity::Error,
                            category: Category::Tasks,
                            entity: format!("task {name}"),
                            message: format!("program.cfe no longer parses: {msg}"),
                            hint: Some(format!("edit {}", program_path.display())),
                        });
                        None
                    }
                },
                Err(err) => {
                    findings.push(Finding {
                        severity: Severity::Error,
                        category: Category::Tasks,
                        entity: format!("task {name}"),
                        message: format!("could not read program.cfe: {err}"),
                        hint: None,
                    });
                    None
                }
            }
        } else {
            None
        };

        // (14) resolved-program parses and references registered targets.
        let resolved = if resolved_present {
            match fs_err::read_to_string(&resolved_path) {
                Ok(src) => match toml::from_str::<ResolvedProgram>(&src) {
                    Ok(r) => Some(r),
                    Err(err) => {
                        findings.push(Finding {
                            severity: Severity::Error,
                            category: Category::Tasks,
                            entity: format!("task {name}"),
                            message: format!("resolved-program.toml could not be parsed: {err}"),
                            hint: Some(format!(
                                "`cargo-for-each task remove --name {name}` and recreate"
                            )),
                        });
                        None
                    }
                },
                Err(err) => {
                    findings.push(Finding {
                        severity: Severity::Error,
                        category: Category::Tasks,
                        entity: format!("task {name}"),
                        message: format!("could not read resolved-program.toml: {err}"),
                        hint: None,
                    });
                    None
                }
            }
        } else {
            None
        };

        if let Some(resolved) = &resolved {
            check_resolved_targets_registered(name, resolved, config, &mut findings);
        }

        // (15) and (16) need the state dir.
        let state_dir = task_state_root.join(name);
        if !state_dir.is_dir() {
            continue;
        }
        if let Some(resolved) = &resolved {
            check_state_cursors(name, &state_dir, resolved, &mut findings);
        }
        if let Some(program) = &parsed_program {
            check_orphan_snapshot_dirs(name, &state_dir, program, &mut findings);
        }
    }

    findings
}

/// List the sub-directory names under `<config_dir>/cargo-for-each/tasks/`.
fn list_task_names(task_def_root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if !task_def_root.is_dir() {
        return names;
    }
    let Ok(entries) = fs_err::read_dir(task_def_root) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|s| s.to_str())
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
}

/// Check that every manifest_dir referenced by the resolved program is
/// currently registered in the config (either as a workspace or as a crate).
fn check_resolved_targets_registered(
    task_name: &str,
    resolved: &ResolvedProgram,
    config: &crate::Config,
    findings: &mut Vec<Finding>,
) {
    let mut registered: HashSet<&PathBuf> = HashSet::new();
    for w in &config.workspaces {
        registered.insert(&w.manifest_dir);
    }
    for c in &config.crates {
        registered.insert(&c.manifest_dir);
    }

    for ws in &resolved.workspace_executions {
        if !registered.contains(&ws.manifest_dir) {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Tasks,
                entity: format!("task {task_name}"),
                message: format!(
                    "resolved program references workspace {} which is no longer registered",
                    ws.manifest_dir.display()
                ),
                hint: Some(
                    "`cargo-for-each target add` the workspace or remove the task".to_owned(),
                ),
            });
        }
        for member in &ws.member_crates {
            if !registered.contains(&member.manifest_dir) {
                findings.push(Finding {
                    severity: Severity::Error,
                    category: Category::Tasks,
                    entity: format!("task {task_name}"),
                    message: format!(
                        "resolved program references crate {} which is no longer registered",
                        member.manifest_dir.display()
                    ),
                    hint: Some(
                        "`cargo-for-each target add` the crate or remove the task".to_owned(),
                    ),
                });
            }
        }
    }
    for c in &resolved.crate_executions {
        if !registered.contains(&c.manifest_dir) {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Tasks,
                entity: format!("task {task_name}"),
                message: format!(
                    "resolved program references crate {} which is no longer registered",
                    c.manifest_dir.display()
                ),
                hint: Some("`cargo-for-each target add` the crate or remove the task".to_owned()),
            });
        }
    }
}

/// Walk the task's state tree; for every directory that contains a marker
/// file, parse the relative path as a [`ProgramCursor`] and validate that
/// any workspace/crate iteration indices fit inside the resolved program.
fn check_state_cursors(
    task_name: &str,
    state_dir: &Path,
    resolved: &ResolvedProgram,
    findings: &mut Vec<Finding>,
) {
    let marker_names = [
        "exit_status",
        "snapshot_metadata_completed",
        "barrier_released",
    ];

    let Ok(walker) = walk_dirs(state_dir) else {
        return;
    };
    for sub in walker {
        if !marker_names.iter().any(|m| sub.join(m).exists()) && !sub.join("chosen_branch").exists()
        {
            continue;
        }
        let Ok(rel) = sub.strip_prefix(state_dir) else {
            continue;
        };
        let rel_str = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let cursor = match ProgramCursor::from_path_string(&rel_str) {
            Ok(c) => c,
            Err(err) => {
                findings.push(Finding {
                    severity: Severity::Error,
                    category: Category::Tasks,
                    entity: format!("task {task_name}"),
                    message: format!(
                        "state directory {} contains an unparsable cursor: {err}",
                        sub.display()
                    ),
                    hint: Some(format!("`rm -rf {}`", sub.display())),
                });
                continue;
            }
        };
        if let Some(reason) = cursor_out_of_range(&cursor, resolved) {
            findings.push(Finding {
                severity: Severity::Error,
                category: Category::Tasks,
                entity: format!("task {task_name}"),
                message: format!("state directory {} references {reason}", sub.display()),
                hint: Some(format!("`rm -rf {}`", sub.display())),
            });
        }
    }
}

/// Return a human-readable reason if the cursor's top-level segments index
/// past the resolved program's workspace or crate executions.
fn cursor_out_of_range(cursor: &ProgramCursor, resolved: &ResolvedProgram) -> Option<String> {
    let mut iter = cursor.segments().iter();
    match iter.next()? {
        CursorSegment::WorkspaceIteration(n) => {
            if *n >= resolved.workspace_executions.len() {
                return Some(format!(
                    "workspace iteration {n} which is out of range (program has {})",
                    resolved.workspace_executions.len()
                ));
            }
            // If next segment is a crate iteration, validate against the
            // workspace's member crates.
            if let Some(CursorSegment::CrateIteration(c)) = iter.next()
                && let Some(ws) = resolved.workspace_executions.get(*n)
            {
                let len = ws.member_crates.len();
                if *c >= len {
                    return Some(format!(
                        "crate iteration {c} in workspace {n} which is out of range (workspace has {len} members)",
                    ));
                }
            }
            None
        }
        CursorSegment::CrateIteration(n) => {
            if *n >= resolved.crate_executions.len() {
                return Some(format!(
                    "crate iteration {n} which is out of range (program has {})",
                    resolved.crate_executions.len()
                ));
            }
            None
        }
        _ => None,
    }
}

/// Compare snapshot directory names on disk against the snapshot statements
/// declared in the program AST; orphan directories become warnings.
fn check_orphan_snapshot_dirs(
    task_name: &str,
    state_dir: &Path,
    program: &crate::program::Program,
    findings: &mut Vec<Finding>,
) {
    let snapshots_root = state_dir.join("snapshots");
    if !snapshots_root.is_dir() {
        return;
    }
    let declared: HashSet<String> = collect_snapshot_names(program);
    let Ok(entries) = fs_err::read_dir(&snapshots_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        if !declared.contains(name) {
            findings.push(Finding {
                severity: Severity::Warning,
                category: Category::Tasks,
                entity: format!("task {task_name}"),
                message: format!(
                    "snapshot directory {} does not match any `snapshot_metadata` step in the program",
                    path.display()
                ),
                hint: Some(format!("`rm -rf {}`", path.display())),
            });
        }
    }
}

/// Visit the AST and collect the `name` field of every `snapshot_metadata`
/// statement reachable from any top-level block.
fn collect_snapshot_names(program: &crate::program::Program) -> HashSet<String> {
    use crate::program::GlobalStatement;
    use crate::program::ast::common::{IfBlock, SnapshotMetadataNode, WithEnvFileBlock};
    use crate::program::ast::crate_ctx::CrateStatement;
    use crate::program::ast::workspace_ctx::{ForCrateInWorkspaceBlock, WorkspaceStatement};

    fn visit_crate_stmts(stmts: &[CrateStatement], out: &mut HashSet<String>) {
        for s in stmts {
            match s {
                CrateStatement::SnapshotMetadata(SnapshotMetadataNode { name }) => {
                    out.insert(name.clone());
                }
                CrateStatement::If(IfBlock {
                    branches,
                    else_statements,
                    ..
                }) => {
                    for b in branches {
                        visit_crate_stmts(&b.statements, out);
                    }
                    visit_crate_stmts(else_statements, out);
                }
                CrateStatement::WithEnvFile(WithEnvFileBlock { statements, .. }) => {
                    visit_crate_stmts(statements, out);
                }
                _ => {}
            }
        }
    }

    fn visit_workspace_stmts(stmts: &[WorkspaceStatement], out: &mut HashSet<String>) {
        for s in stmts {
            match s {
                WorkspaceStatement::SnapshotMetadata(SnapshotMetadataNode { name }) => {
                    out.insert(name.clone());
                }
                WorkspaceStatement::ForCrateInWorkspace(ForCrateInWorkspaceBlock {
                    statements,
                }) => {
                    visit_crate_stmts(statements, out);
                }
                WorkspaceStatement::If(IfBlock {
                    branches,
                    else_statements,
                    ..
                }) => {
                    for b in branches {
                        visit_workspace_stmts(&b.statements, out);
                    }
                    visit_workspace_stmts(else_statements, out);
                }
                WorkspaceStatement::WithEnvFile(WithEnvFileBlock { statements, .. }) => {
                    visit_workspace_stmts(statements, out);
                }
                _ => {}
            }
        }
    }

    let mut out = HashSet::new();
    for stmt in &program.statements {
        match stmt {
            GlobalStatement::ForWorkspace(block) => {
                visit_workspace_stmts(&block.statements, &mut out);
            }
            GlobalStatement::ForCrate(block) => visit_crate_stmts(&block.statements, &mut out),
            GlobalStatement::SelectWorkspaces(_) | GlobalStatement::SelectCrates(_) => {}
        }
    }
    out
}

/// Recursively enumerate sub-directories of `root` (including `root` itself).
fn walk_dirs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = vec![root.to_path_buf()];
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs_err::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.push(path.clone());
                stack.push(path);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helpers panic on unexpected shapes; clearer than assert with message"
    )]

    use super::*;
    use crate::error::Error;
    use crate::targets::{AddParameters, TargetParameters, TargetSubCommand};
    use crate::{Command, Config, Crate, Environment, Options, Workspace, config_file, run_app};
    use pretty_assertions::assert_eq;

    /// Set up an empty mock environment with a real on-disk crate under
    /// `temp_dir/workspaces/<name>/`.  Returns the canonical crate manifest_dir.
    fn add_real_workspace(
        temp_dir: &tempfile::TempDir,
        environment: &Environment,
        name: &str,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let parent = temp_dir.path().join("workspaces");
        fs_err::create_dir_all(&parent)?;
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&parent).args(["new", "--lib", name]);
        crate::utils::execute_command(&mut cmd, environment, &parent)?;
        Ok(fs_err::canonicalize(parent.join(name))?)
    }

    async fn add_to_config(
        manifest_path: PathBuf,
        environment: Environment,
    ) -> Result<(), Box<dyn std::error::Error>> {
        run_app(
            Options {
                command: Command::Target(TargetParameters {
                    sub_command: TargetSubCommand::Add(AddParameters { manifest_path }),
                }),
            },
            environment,
        )
        .await?;
        Ok(())
    }

    async fn run_check(environment: Environment, params: CheckParameters) -> Result<(), Error> {
        run_app(
            Options {
                command: Command::Check(params),
            },
            environment,
        )
        .await
    }

    fn default_params() -> CheckParameters {
        CheckParameters {
            targets_only: false,
            tasks_only: false,
        }
    }

    fn count_severities(findings: &[Finding]) -> (usize, usize) {
        let errors = findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count();
        let warnings = findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count();
        (errors, warnings)
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_clean_returns_ok() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let ws_dir = add_real_workspace(&temp_dir, &environment, "clean")?;
        add_to_config(ws_dir.join("Cargo.toml"), environment.clone()).await?;
        let result = run_check(environment, default_params()).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_missing_workspace_cargo_toml() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let ws_dir = add_real_workspace(&temp_dir, &environment, "gone")?;
        add_to_config(ws_dir.join("Cargo.toml"), environment.clone()).await?;

        // Capture the config file's mtime to assert no mutation later.
        let mtime_before = fs_err::metadata(config_file(&environment))?.modified()?;

        fs_err::remove_file(ws_dir.join("Cargo.toml"))?;

        let config = Config::load(&environment)?;
        let findings = check_target_filesystem(&config);
        assert!(
            findings.iter().any(|f| f.severity == Severity::Error
                && f.entity.contains("workspace")
                && f.message.contains("Cargo.toml is missing")),
            "expected workspace-missing-Cargo.toml finding, got: {findings:#?}"
        );

        let result = run_check(environment.clone(), default_params()).await;
        match result {
            Err(Error::CheckFoundIssues { errors, .. }) => {
                assert!(errors >= 1, "expected at least one error, got {errors}");
            }
            other => panic!("expected CheckFoundIssues, got {other:?}"),
        }

        let mtime_after = fs_err::metadata(config_file(&environment))?.modified()?;
        assert_eq!(
            mtime_before, mtime_after,
            "check must not rewrite the config file"
        );

        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_orphan_crate() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        // Hand-build a config with an orphan crate (its workspace_manifest_dir
        // is not registered).  Saves us a metadata round-trip.
        let workspace_dir = temp_dir.path().join("real");
        fs_err::create_dir_all(&workspace_dir)?;
        let orphan_workspace = temp_dir.path().join("ghost");
        let orphan_crate = orphan_workspace.join("inner");
        let mut config = Config::default();
        config.workspaces.push(Workspace {
            manifest_dir: workspace_dir.clone(),
            is_standalone: true,
        });
        config.crates.push(Crate {
            manifest_dir: orphan_crate.clone(),
            workspace_manifest_dir: orphan_workspace.clone(),
            crate_types: std::collections::BTreeSet::new(),
            target_kinds: std::collections::BTreeSet::new(),
        });
        config.save(&environment)?;

        let loaded = Config::load(&environment)?;
        let findings = check_target_filesystem(&loaded);
        assert!(
            findings.iter().any(|f| f.message.contains("orphan crate")),
            "expected orphan-crate finding, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_duplicate_workspace() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let dup_dir = temp_dir.path().join("dup");
        fs_err::create_dir_all(&dup_dir)?;
        let mut config = Config::default();
        config.workspaces.push(Workspace {
            manifest_dir: dup_dir.clone(),
            is_standalone: true,
        });
        config.workspaces.push(Workspace {
            manifest_dir: dup_dir.clone(),
            is_standalone: true,
        });
        config.save(&environment)?;

        let loaded = Config::load(&environment)?;
        let findings = check_target_filesystem(&loaded);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("registered 2 times")),
            "expected duplicate-workspace finding, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_detects_is_standalone_drift() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let ws_dir = add_real_workspace(&temp_dir, &environment, "drift")?;
        add_to_config(ws_dir.join("Cargo.toml"), environment.clone()).await?;

        // Convert the standalone crate into a multi-crate workspace on disk
        // without telling cargo-for-each.
        fs_err::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"sub\"]\nresolver = \"2\"\n",
        )?;
        let mut cmd = std::process::Command::new("cargo");
        cmd.current_dir(&ws_dir).args(["new", "--lib", "sub"]);
        crate::utils::execute_command(&mut cmd, &environment, &ws_dir)?;

        let config = Config::load(&environment)?;
        let findings = check_target_cargo_metadata(&config);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("is_standalone flag")),
            "expected is_standalone-drift finding, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_state_dir_orphan() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        // No tasks registered, but plant a leftover state dir.
        let orphan_state = environment
            .state_dir
            .join("cargo-for-each")
            .join("tasks")
            .join("ghost-task");
        fs_err::create_dir_all(&orphan_state)?;

        let config = Config::default();
        let findings = check_tasks(&environment, &config);
        assert!(
            findings.iter().any(|f| f.severity == Severity::Warning
                && f.entity == "task ghost-task"
                && f.message.contains("state directory exists")),
            "expected state-dir-orphan warning, got: {findings:#?}"
        );

        // Warning-only ⇒ check_command returns Ok.
        let result = run_check(
            environment,
            CheckParameters {
                targets_only: false,
                tasks_only: true,
            },
        )
        .await;
        assert!(
            result.is_ok(),
            "expected Ok with only warnings, got {result:?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_missing_program_cfe() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        // Create a phantom task directory with neither program.cfe nor resolved-program.toml.
        let task_dir = crate::tasks::named_dir_path("incomplete", &environment)?;
        fs_err::create_dir_all(&task_dir)?;

        let config = Config::default();
        let findings = check_tasks(&environment, &config);
        let (errors, _) = count_severities(&findings);
        assert!(
            errors >= 2,
            "expected at least two errors (program.cfe and resolved-program.toml), got: {findings:#?}"
        );
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("program.cfe is missing")),
            "expected program.cfe-missing finding, got: {findings:#?}"
        );
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("resolved-program.toml is missing")),
            "expected resolved-program-missing finding, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_unparsable_program() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let task_dir = crate::tasks::named_dir_path("broken", &environment)?;
        fs_err::create_dir_all(&task_dir)?;
        fs_err::write(task_dir.join("program.cfe"), "not a valid program @@@")?;
        // Plausible empty resolved program so check focuses on parse failure.
        fs_err::write(
            task_dir.join("resolved-program.toml"),
            "workspace_executions = []\ncrate_executions = []\n",
        )?;

        let config = Config::default();
        let findings = check_tasks(&environment, &config);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("program.cfe no longer parses")),
            "expected program parse failure finding, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_resolved_program_references_unregistered_target()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let task_dir = crate::tasks::named_dir_path("stale-targets", &environment)?;
        fs_err::create_dir_all(&task_dir)?;
        fs_err::write(
            task_dir.join("program.cfe"),
            "select workspaces;\nfor workspace {\n    run \"true\";\n}\n",
        )?;
        // Hand-crafted resolved-program pointing at a workspace that won't be registered.
        fs_err::write(
            task_dir.join("resolved-program.toml"),
            "[[workspace_executions]]\nmanifest_dir = \"/nonexistent/ghost\"\ndependencies = []\nmember_crates = []\n\n[[crate_executions]]\nmanifest_dir = \"/nonexistent/ghost-crate\"\ndependencies = []\n",
        )?;

        let config = Config::default();
        let findings = check_tasks(&environment, &config);
        assert!(
            findings.iter().any(
                |f| f.severity == Severity::Error && f.message.contains("no longer registered")
            ),
            "expected unregistered-target finding, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_reports_invalid_cursor() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let task_dir = crate::tasks::named_dir_path("bad-cursor", &environment)?;
        fs_err::create_dir_all(&task_dir)?;
        fs_err::write(
            task_dir.join("program.cfe"),
            "select workspaces;\nfor workspace {\n    run \"true\";\n}\n",
        )?;
        fs_err::write(
            task_dir.join("resolved-program.toml"),
            "workspace_executions = []\ncrate_executions = []\n",
        )?;
        // Plant a state directory that claims workspace iteration 99.
        let state_dir = crate::tasks::state_dir_for_task("bad-cursor", &environment)?;
        let bogus = state_dir.join("w99").join("s0");
        fs_err::create_dir_all(&bogus)?;
        fs_err::write(bogus.join("exit_status"), "0")?;

        let config = Config::default();
        let findings = check_tasks(&environment, &config);
        assert!(
            findings.iter().any(|f| f.severity == Severity::Error
                && f.message.contains("workspace iteration 99")
                && f.message.contains("out of range")),
            "expected out-of-range cursor finding, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_lock_held_emits_warning() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let _lock = crate::ConfigLock::acquire(&environment)?;

        let findings = check_config_lock(&environment);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Warning
                    && f.message.contains("lock currently held")),
            "expected lock-held warning, got: {findings:#?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_targets_only_skips_tasks() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        // Plant a task-side issue (orphan state dir).
        let orphan_state = environment
            .state_dir
            .join("cargo-for-each")
            .join("tasks")
            .join("ghost-task");
        fs_err::create_dir_all(&orphan_state)?;

        let result = run_check(
            environment,
            CheckParameters {
                targets_only: true,
                tasks_only: false,
            },
        )
        .await;
        assert!(
            result.is_ok(),
            "task-side issue should be skipped with --targets-only; got {result:?}"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn check_does_not_mutate_state() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        let ws_dir = add_real_workspace(&temp_dir, &environment, "immutable")?;
        add_to_config(ws_dir.join("Cargo.toml"), environment.clone()).await?;

        // Create a deliberate finding (missing Cargo.toml).
        fs_err::remove_file(ws_dir.join("Cargo.toml"))?;

        let mtime_before = fs_err::metadata(config_file(&environment))?.modified()?;
        let config_before = Config::load(&environment)?;

        drop(run_check(environment.clone(), default_params()).await);

        let mtime_after = fs_err::metadata(config_file(&environment))?.modified()?;
        let config_after = Config::load(&environment)?;
        assert_eq!(mtime_before, mtime_after, "config mtime changed");
        assert_eq!(
            config_before.workspaces.len(),
            config_after.workspaces.len(),
            "workspace count changed"
        );
        assert_eq!(
            config_before.crates.len(),
            config_after.crates.len(),
            "crate count changed"
        );
        Ok(())
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn list_crates_no_longer_emits_orphans() -> Result<(), Box<dyn std::error::Error>> {
        use crate::targets::{CrateFilterParameters, ListParameters, TargetFilter};

        let temp_dir = tempfile::tempdir()?;
        let environment = Environment::mock(&temp_dir)?;
        // Manually plant an orphan crate.
        let mut config = Config::default();
        config.crates.push(Crate {
            manifest_dir: temp_dir.path().join("orphan-mfd"),
            workspace_manifest_dir: temp_dir.path().join("unknown-ws"),
            crate_types: std::collections::BTreeSet::new(),
            target_kinds: std::collections::BTreeSet::new(),
        });
        config.save(&environment)?;

        // Just verify the command completes Ok (no panic, no error from
        // filter logic).  Both --standalone true and false now exclude
        // orphans, so neither prints anything for this orphan.
        for standalone in [None, Some(true), Some(false)] {
            let options = Options {
                command: Command::Target(TargetParameters {
                    sub_command: TargetSubCommand::List(ListParameters {
                        target_filter: TargetFilter::Crates(CrateFilterParameters {
                            crate_type: None,
                            target_kind: None,
                            standalone,
                        }),
                    }),
                }),
            };
            let result = run_app(options, environment.clone()).await;
            assert!(
                result.is_ok(),
                "list crates --standalone {standalone:?} failed: {result:?}"
            );
        }
        Ok(())
    }
}
