# Changelog

## 0.1.1 - 2026-08-25 10:55:42Z

### 🚀 Features

- *(config)* Allow adding Manifests to config and listing crates and workspaces
- *(config)* Do not add crates/workspaces if they already exist in the config
- *(refresh)* Add a refresh subcommand that updates crate types and new
  workspace members
- *(tracing)* Add log messages on debug level about add/refresh activity
- *(exec)* Add the exec subcommand, the main purpose of this cargo plugin
- *(exec)* Add crate type filter to list and exec crates subcommands
- *(config)* Store standalone status and workspace manifest in config
- *(standalone)* Filter by standalone status
- *(target_sets)* Add the ability to abstract target sets
- *(plan)* Add plan subcommand to create plans of multiple commands to run
- *(task)* Add commands to create/delete tasks from plan and target set
- *(dependencies)* Add dependencies inside the task resolved target set
- *(target)* Add a target subcommand and move the existing commands there
- *(run)* Implement task run
- *(run)* Consider dependencies in task run
- *(environment)* Get all environment data at the start in preparation for
  testing
- *(tests)* Basic test logic and first example test
- *(tests)* Capture test output
- *(tasks)* Add list, describe and rewind sub-commands for task
- *(targetsets)* Add describe command for target-set
- *(step-position)* Introduce StepPosition type for nested step addressing
- *(condition)* Introduce Condition type for plan control flow
- *(plans)* Introduce IfElseIf step type with conditional branching
- *(plans)* Add CLI sub-commands for IfElseIf step management
- *(condition)* Add FileExists condition variant
- *(condition)* Add WorkingDirectoryClean condition variant
- *(program)* Add snapshot_metadata statement and \${} string interpolation
- *(condition)* Git config string comparison permission, e.g. to check if the
  repository has a specific commit message format set in its git config
- *(program)* Add with_env_file block to load env vars from a file for nested
  statements
- *(program)* Print each run step command with full arguments before execution
- *(describe)* Annotate program listing with cursor, completion status, and
  branch info
- *(wait_for_continue)* Add wait barrier statement and task continue command
- *(task_create)* Add --workspace and --crate flags for explicit target
  selection
- *(crate_types)* Add cdylib, dylib, rlib, staticlib, bench, test, example,
  custom_build crate type filters
- *(execution)* Print statement type and condition details during execution
- *(resolve)* Soft dev-dep ordering via SCC-aware cycle breaking
- *(check)* Add read-only audit subcommand
- *(parser)* Accept \\ as backslash escape in string literals

### 🐛 Bug Fixes

- *(workspaces)* Add test and fix full workflow for workspaces
- *(target_sets)* Do not use dependencies when looking for package definitions
- *(tasks)* Simplistic escape mechanism for command arguments
- *(targetset)* Fix handling of workspace target set
- *(utils)* Use environment.paths for command lookup instead of process PATH
- *(plans)* Return PlanNotFound when plan does not exist in plan-step commands
- *(target-sets)* Store canonical manifest paths to fix dependency resolution
- *(tasks)* Fix infinite loop and wrong error type in run-all-targets
- *(tasks)* Write exit_status file on command launch failure
- *(tasks)* Use wrapper script to capture real exit code from asciinema
- *(program)* Properly capture the output of the git status --porcelain call to
  evaluate clean working directory
- *(asciinema)* When a step failed or was rewound asciinema failed with an error
  because the output file already existed
- *(resolve)* Exclude dev-dependencies from intra-workspace crate execution
  order
- *(formatting)* Fix formatting
- *(wait_barrier)* Include cargo-for-each and task name in suggested continue
  command
- *(github)* Pin action SHAs and scope contents:write to release jobs
- *(run)* Use POSIX shell quoting for command and args in wrapper script
- *(file_exists)* Restrict path to within enclosing workspace
- *(perms)* Write user state/config files with mode 0o600 and dirs 0o700
- *(windows)* Restore build and tighten executable detection
- *(clippy)* Correct assert_matches path in disallowed-macros
- *(targets)* Canonicalize paths and harden remove/refresh
- *(snapshots)* Canonicalize manifest key, drop cross-workspace fallback
- *(task-remove)* Clean up state dir, return TaskNotFound
- *(task-continue)* Validate cursor targets a wait_for_continue
- *(run-step)* Propagate exit_status read/parse errors
- *(if-block)* Propagate chosen_branch read/parse errors
- *(run-all)* Refuse parallel jobs with interactive steps
- *(resolve)* Propagate canonicalize errors when computing inter-workspace deps
- *(condition)* Surface signal-killed run-condition as an error
- *(wait_barrier)* Use to_path_string for human-readable cursor mention
- *(wait_barrier)* Add `cargo-for-each` prefix to remaining suggestion site
- *(cursor)* Require digits after w/c/s/if prefix in FromStr
- *(evaluate)* Use operator-aware separator in runtime detail
- *(tasks)* Propagate barrier suspension through scope runners
- *(bin)* Support invocation as `cargo for-each` subcommand
- *(config)* Atomic writes + flock around load/modify/save
- *(targets)* Propagate Config::load errors from list_command
- *(tasks)* Validate --name to prevent path traversal
- *(tasks)* Propagate non-NotFound chosen_branch read errors
- *(parser)* Reject duplicate top-level for-workspace / for-crate blocks
- *(targets)* Refresh now handles workspace transitions and orphans
- *(targets)* Remove now errors when nothing matched
- *(cursor)* Reject malformed cursor path strings
- *(tasks)* Distinguish "cursor off the program" from "not a barrier"
- *(targets)* Reject add when manifest path isn't named Cargo.toml

### 💼 Other

- *(test)* Refactor a lot of the non-sensical naming as part of first full test

### 🚜 Refactor

- Replace TOML plans/target-sets with .cfe program language
- *(targets)* Split CrateType into CrateType and TargetKind
- *(targets)* Remove unreachable! from add_command
- *(ast)* Enforce "if branches non-empty" at the type level
- *(tasks)* Tighten run-outcome encoding and propagate parse errors
- *(ast)* Enforce >= 2 operands on And/Or at the type level
- *(tasks)* Decompose describe/find-next/run engines and add coverage
- *(evaluate)* Decompose condition evaluators into leaf helpers
- *(tasks)* Extract shared scheduler phase from run_all_targets_command
- *(check)* Decompose task/metadata checks and add unit tests
- *(targets)* Extract manifest-dir resolution from remove_command

### 📚 Documentation

- *(cfe)* Add language reference for .cfe program files
- *(readme)* Document all CLI commands, concepts, and typical workflow
- *(with_env_file)* Clarify that absolute paths are accepted
- *(wait_barrier)* Make empty-body no-op explicit
- *(env)* Correct XDG env-var names in Environment fields
- *(workspace)* Correct HasMembers doc — single member counts
- *(error)* Clarify CommandFailed exit-code semantics
- *(error)* Explain why IoError omits #[from]
- *(ast)* Correct CrateStatement::SnapshotMetadata scoping
- *(resolve)* Correct snapshot ordering claim
- *(resolve)* Document Standalone fallback for unknown workspaces
- *(tasks)* Reword is_run_failed to cover empty-status case
- *(tasks)* Document tri-state wait-barrier model
- *(tasks)* Document actual subpath layouts for task dirs
- *(crate)* Drop obsolete cardinality claim on Crate.types
- *(parser)* Clarify padding's progress invariant
- *(cursor)* Document per-block reset of s{N} indices
- *(parser)* Note string_literal consumes surrounding padding
- *(targets)* Mark list_command output as unstable
- *(tasks)* Document empty-branch ifs as intentional rewind steps
- Document type vs target_kind split (follow-up to 39d2c07)
- *(readme)* Correct registered-targets config filename
- *(readme)* Correct task rewind single-step / single-target semantics
- *(readme)* Document all three target remove match cases
- *(cfe-language)* Note file_exists symlink behavior
- *(cfe-language)* Describe runtime readiness, not iteration order
- *(parser)* List actual condition productions in doc comments
- *(utils)* Describe is_executable's actual contract
- *(error)* Reword FoundNoPackageInCargoMetadataWithCurrentManifestPath
- *(readme)* Document the three log sinks and what they capture
- *(tasks)* Explain why the wait-barrier race is unfixed
- *(tasks)* Document the intentional out-of-scope dep skip

### 🧪 Testing

- *(tasks)* Cover command handlers via task fixtures; tidy dispatch

### ⚙️ Miscellaneous Tasks

- *(init)* Initial commit with cargo-generate output
- *(code)* Code reorg to split it into multiple files
- *(dependencies)* Update dependencies
- *(dependencies)* Update dependencies
- *(dependencies)* Update dependencies
- Apply tombi format and fix clippy::map_unwrap_or
- *(targets)* Delete unused `Target` struct
- *(ast)* Delete dead `From<*SelectCondition>` impls
- *(ast)* Re-export `WaitForContinueNode` and `WithEnvFileBlock`
- *(config)* Make `config_dir_path`/`config_file` infallible
- *(crap)* Add cargo-crap config with documented allow-list
- *(template)* Sync release tooling and rustdoc lints from bin template
- *(dependencies)* Update dependencies
- *(dependencies)* Update dependencies

## 0.1.0

Initial Release
