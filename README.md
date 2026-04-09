# cargo-for-each

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/taladar/cargo-for-each/github-release.yaml)](https://github.com/taladar/cargo-for-each/actions/workflows/github-release.yaml)

cargo-for-each:
[![Crates.io Version cargo-for-each](https://img.shields.io/crates/v/cargo-for-each)](https://crates.io/crates/cargo-for-each)
[![lib.rs Version cargo-for-each](https://img.shields.io/crates/v/cargo-for-each?label=lib.rs)](https://lib.rs/crates/cargo-for-each)
![docs.rs cargo-for-each - none for binary crate](https://img.shields.io/badge/docs-none_for_binary_crate-lightgrey)
[![Dependency status cargo-for-each](https://deps.rs/crate/cargo-for-each/latest/status.svg)](https://deps.rs/crate/cargo-for-each/)

`cargo-for-each` is a task runner for Rust developers who maintain multiple
workspaces or crates. You register your projects once, write a `.cfe` program
that describes the steps to execute, then let `cargo-for-each` run those steps
across every registered target in the correct dependency order.

## Core Concepts

### Registered Targets

`cargo-for-each` keeps a configuration file (stored in the XDG config
directory, typically `~/.config/cargo-for-each/config.toml`) that lists every
workspace and crate you want to manage. You add entries with `target add` and
remove them with `target remove`. The configuration records:

- **Workspaces** — each identified by its directory containing `Cargo.toml`;
  may be standalone (single-crate) or multi-crate.
- **Crates** — each identified by its own `Cargo.toml` directory and the
  workspace it belongs to.

### `.cfe` Programs

A task is driven by a `.cfe` (cargo-for-each) program file. The program
selects a subset of the registered targets, defines conditions (e.g. only
library crates), and lists the statements to execute on each target:
`run` commands, `manual_step` prompts, `wait_for_continue` barriers, and
`snapshot_metadata` captures.

See [`doc/cfe-language.md`](doc/cfe-language.md) for the full language
reference.

### Tasks

A task combines a `.cfe` program with persisted execution state. Once created,
you can run a task step-by-step, one target at a time, or in parallel across
all targets. If a step fails you can fix the problem and re-run; if you need to
back up you can rewind. State is stored in the XDG state directory (typically
`~/.local/state/cargo-for-each/tasks/<name>/`).

## Typical Workflow

```text
# 1. Register your projects
cargo-for-each target add --manifest-path ~/projects/my-lib/Cargo.toml
cargo-for-each target add --manifest-path ~/projects/my-app/Cargo.toml

# 2. Write a program (release.cfe)
#    see doc/cfe-language.md for syntax

# 3. Create a task from the program
cargo-for-each task create --name release --program release.cfe

# 4. Check what will run
cargo-for-each task describe --name release

# 5. Run everything
cargo-for-each task run all-targets --name release -j 4

# 6. If something went wrong, rewind and fix
cargo-for-each task rewind single-step --name release
```

## Commands

### `target` — Manage Registered Projects

#### `target list workspaces`

List all registered workspaces.

| Flag | Description |
|------|-------------|
| `--no-standalone` | Only list multi-crate workspaces (exclude standalone crates). |

#### `target list crates`

List all registered crates.

| Flag | Description |
|------|-------------|
| `--type <TYPE>` | Only list crates of the given type (`bin`, `lib`, `proc-macro`, `cdylib`, `dylib`, `rlib`, `staticlib`, `bench`, `test`, `example`, `custom-build`). |
| `--standalone <BOOL>` | Filter by whether the crate belongs to a standalone workspace. |

#### `target add`

Add a workspace or crate. If the given `Cargo.toml` is a workspace root, all
member crates are added automatically.

| Flag | Description |
|------|-------------|
| `--manifest-path <PATH>` | Path to the `Cargo.toml` file to register. |

#### `target remove`

Remove a workspace and all its crates from the configuration.

| Flag | Description |
|------|-------------|
| `--manifest-path <PATH>` | Path to the `Cargo.toml` file to remove. |

#### `target refresh`

Re-scan all registered workspaces: remove entries whose directories no longer
exist, and add any new member crates that have appeared since the last
`target add` or `refresh`.

---

### `task` — Manage and Run Tasks

#### `task list`

Print the names of all existing tasks.

#### `task create`

Create a new task from a `.cfe` program.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name for the task. |
| `--program <PATH>` | Path to the `.cfe` program file. |
| `--workspace <PATH>` | (Repeatable) Explicit workspace directory to target. Overrides `select workspaces` in the program. Dependency ordering is still computed. |
| `--crate <PATH>` | (Repeatable) Explicit crate directory to target. Overrides `select crates` in the program. Dependency ordering is still computed. |

When `--workspace` or `--crate` flags are provided they take precedence over
the corresponding `select` statements in the program. You can mix: supply
explicit crates while letting the program choose workspaces, or vice versa.

#### `task remove`

Delete a task and all its execution state.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to remove. |

#### `task describe`

Print the full program with the current execution status of each step.
Each statement is prefixed with its cursor path and a status icon:

| Icon | Meaning |
|------|---------|
| ⬜ | Not yet started. |
| ▶ | Currently the next step to run. |
| ✅ | Completed successfully. |
| ❌ | Failed (non-zero exit code). |
| ⏳ | Waiting at a `wait_for_continue` barrier. |

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to describe. |

#### `task run single-step`

Execute the single next uncompleted statement across all targets, then stop.
Useful for stepping through a task manually.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to run. |

#### `task run single-target`

Run all remaining statements for the first target that has pending work, then
stop. Useful when you want to fully process one workspace or crate before
moving on.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to run. |

#### `task run all-targets`

Run all targets to completion in dependency order.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to run. |
| `-j <N>`, `--jobs <N>` | Number of targets to process in parallel (default: 1). |
| `-k`, `--keep-going` | Continue running other targets when one fails, similar to `make -k`. |

Targets that reach a `wait_for_continue` barrier are suspended automatically.
Other ready targets continue running. Use `task continue` to release a barrier
and let a suspended target resume on the next invocation.

#### `task rewind single-step`

Undo the last completed statement across all targets. The state for that
statement is deleted so it will be re-executed on the next run.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to rewind. |

#### `task rewind single-target`

Undo all completed statements for the last target that finished, resetting it
to the beginning.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to rewind. |

#### `task rewind all-targets`

Reset the entire task: delete all execution state so the task starts from
scratch.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task to rewind. |

#### `task continue`

Release a `wait_for_continue` barrier so the blocked target can proceed past it
on the next `task run` invocation. The cursor path to pass is printed when the
barrier is first reached.

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Name of the task containing the barrier. |
| `--cursor <CURSOR>` | Cursor path of the barrier (e.g. `w0/s2/`). |

You can release a barrier before execution reaches it (pre-release), in which
case the barrier will be skipped when encountered.

---

### `generate-manpage`

Generate man pages for all commands into a directory.

| Flag | Description |
|------|-------------|
| `--output-dir <PATH>` | Directory to write the generated man pages. |

### `generate-shell-completion`

Generate shell completion scripts.

| Flag | Description |
|------|-------------|
| `--output-file <PATH>` | File to write the completion script. |
| `--shell <SHELL>` | Shell to generate completions for (`bash`, `zsh`, `fish`, `elvish`, `powershell`). |

## Installation

```text
cargo install cargo-for-each
```

## Further Reading

- [`.cfe` Language Reference](doc/cfe-language.md) — full syntax reference for
  program files including all statements, conditions, operators, and
  interpolation features.
