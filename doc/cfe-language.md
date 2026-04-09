# The `.cfe` Program Language

A `.cfe` file describes the steps to execute across a set of Rust workspaces
and/or standalone crates. When a task is created (`cargo-for-each task create
--program <file>`), the program is parsed and the target set is resolved once
against the registered workspaces and crates. Execution then proceeds in
dependency order.

---

## Table of contents

1. [Lexical conventions](#1-lexical-conventions)
2. [Program structure](#2-program-structure)
3. [Global statements](#3-global-statements)
   - [select workspaces](#31-select-workspaces)
   - [select crates](#32-select-crates)
   - [for workspace](#33-for-workspace)
   - [for crate](#34-for-crate)
4. [Workspace statements](#4-workspace-statements)
   - [run](#41-run)
   - [manual_step](#42-manual_step)
   - [snapshot_metadata](#43-snapshot_metadata)
   - [wait_for_continue](#44-wait_for_continue)
   - [with_env_file](#45-with_env_file)
   - [if / else if / else](#46-if--else-if--else)
   - [for crate in workspace](#47-for-crate-in-workspace)
5. [Crate statements](#5-crate-statements)
   - [run](#51-run)
   - [manual_step](#52-manual_step)
   - [snapshot_metadata](#53-snapshot_metadata)
   - [wait_for_continue](#54-wait_for_continue)
   - [with_env_file](#55-with_env_file)
   - [if / else if / else](#56-if--else-if--else)
6. [Conditions](#6-conditions)
   - [Common conditions](#61-common-conditions)
   - [Workspace-only conditions](#62-workspace-only-conditions)
   - [Crate-only conditions](#63-crate-only-conditions)
   - [Select-filter conditions](#64-select-filter-conditions)
   - [Boolean operators](#65-boolean-operators)
7. [String interpolation](#7-string-interpolation)
8. [Execution model](#8-execution-model)
9. [Complete examples](#9-complete-examples)

---

## 1. Lexical conventions

### Whitespace and comments

Whitespace (spaces, tabs, newlines) is ignored between tokens. Line comments
start with `//` and extend to the end of the line.

```text
// This is a comment.
select workspaces;  // inline comment
```

### String literals

All string parameters are **double-quoted**. The only supported escape
sequence inside a string is `\"` (a literal double-quote character).

```text
"hello world"
"path/to/file.env"
"She said \"hello\""
```

---

## 2. Program structure

A program is a sequence of **global statements** at the top level. There is no
required ordering, but typically the selection statements come before the
iteration blocks.

```text
// Select which targets to run against.
select workspaces;
select crates where standalone;

// Define what to do for each workspace.
for workspace {
    ...
}

// Define what to do for each standalone crate.
for crate {
    ...
}
```

---

## 3. Global statements

### 3.1 `select workspaces`

Chooses which registered workspaces to include in the task.

```text
select workspaces;
select workspaces where <workspace-select-condition>;
```

- Without `where`: all registered workspaces are selected.
- With `where`: only workspaces matching the condition are selected.
- Multiple `select workspaces` statements are allowed; a workspace is included
  if it matches **any** of them (union semantics).
- Only [select-filter conditions](#64-select-filter-conditions) are available
  here (no `ask_user` or `run`).

#### Examples

```text
select workspaces;
select workspaces where !standalone;
select workspaces where standalone || has_members;
```

### 3.2 `select crates`

Chooses which registered standalone crates to include in the task.

```text
select crates;
select crates where <crate-select-condition>;
```

- Without `where`: all registered standalone crates are selected.
- With `where`: only crates matching the condition are selected.
- Multiple `select crates` statements are allowed (union semantics).
- Only [select-filter conditions](#64-select-filter-conditions) are available
  here (no `ask_user` or `run`).

#### Examples

```text
select crates;
select crates where standalone;
select crates where type == lib;
select crates where type == bin || type == proc_macro;
```

### 3.3 `for workspace`

Defines the statements to execute once for each selected workspace.

```text
for workspace {
    <workspace-statement> ...
}
```

Workspaces are executed in inter-workspace dependency order (a workspace that
depends on another is executed after it). The body is a sequence of
[workspace statements](#4-workspace-statements).

### 3.4 `for crate`

Defines the statements to execute once for each selected standalone crate.

```text
for crate {
    <crate-statement> ...
}
```

Crates are executed in dependency order. The body is a sequence of
[crate statements](#5-crate-statements).

---

## 4. Workspace statements

These statements appear inside a `for workspace { ... }` block (or inside
nested blocks within it). Each statement runs with the workspace root as its
working directory.

### 4.1 `run`

Executes an external command in the workspace root directory.

```text
run "command" "arg1" "arg2" ... ;
```

- `"command"`: the executable name or absolute path.
- `"arg1" "arg2" ...`: zero or more arguments (each a separate string literal).
- Arguments may contain [string interpolations](#7-string-interpolation).
- If the command exits with a non-zero status the step is marked as **failed**
  (shown as ❌ in `task describe`) and execution stops for that workspace.
- A completed (exit 0) step is not re-run on subsequent invocations.

#### Examples

```text
run "cargo" "publish" "--no-verify";
run "git" "tag" "${meta.version}";
```

### 4.2 `manual_step`

Pauses and displays instructions for a step that the user must perform
manually.

```text
manual_step "title" "instructions";
```

- `"title"`: a short label shown in the task listing.
- `"instructions"`: the full text displayed when the step is reached.
- Both strings may contain [string interpolations](#7-string-interpolation).
- The user must confirm completion before execution proceeds to the next step.

#### Example

```text
manual_step
    "Tag and push release"
    "Create a signed tag v${meta.version} and push it to the remote.";
```

### 4.3 `snapshot_metadata`

Captures `cargo metadata` for the current workspace and stores it under a
name for later [interpolation](#7-string-interpolation).

```text
snapshot_metadata "name";
```

- `"name"`: the identifier used to reference this snapshot in `${name.field}`
  interpolations later in the program.
- The snapshot is taken at the moment this step executes, so it reflects the
  current state of `Cargo.toml`.
- A snapshot must be taken before any `${name.field}` reference that uses it.

#### Example

```text
snapshot_metadata "meta";
run "git" "tag" "v${meta.version}";
```

### 4.4 `wait_for_continue`

Pauses execution of this workspace until the user releases the barrier using
`cargo-for-each task continue`. This allows other independent workspaces or
crates to proceed while waiting.

```text
wait_for_continue "description";
```

- `"description"`: a message shown when the barrier is reached, explaining
  what the user should wait for.
- States shown in `task describe`:
  - ⬜ **Pending** — not yet reached.
  - ⏳ **Waiting** — reached and waiting for the user to release it.
  - ✅ **Released** — the user has released it; execution can continue.

#### Release command

```text
cargo-for-each task continue --name <task-name> --cursor <cursor>
```

The cursor (e.g. `w3/s1/`) is shown in `task describe` and in the message
printed when the barrier is first reached.

#### Example

```text
run "cargo" "publish";
wait_for_continue "Wait for crates.io to index the new version before tagging.";
run "git" "tag" "v${meta.version}";
```

### 4.5 `with_env_file`

Loads environment variables from a file and makes them available to all
statements in the enclosed block.

```text
with_env_file "relative/path/to/file.env" {
    <workspace-statement> ...
}
```

- The path is relative to the workspace root directory.
- The file is parsed as a `.env`-style file: `KEY=VALUE` lines, `#` comments,
  blank lines ignored, values may be single- or double-quoted.
- `export KEY=VALUE` lines are also accepted (the `export` prefix is stripped).
- Nested `with_env_file` blocks are allowed; inner variables extend (and
  override for duplicate keys) outer ones.
- The variables are passed to `run` commands inside the block as extra
  environment variables.

#### Example

```text
with_env_file ".env.publish" {
    run "cargo" "publish";
    run "cargo" "publish" "--manifest-path" "subcrate/Cargo.toml";
}
```

### 4.6 `if` / `else if` / `else`

Evaluates a condition and executes the matching branch.

```text
if <condition> {
    <workspace-statement> ...
}
else if <condition> {
    <workspace-statement> ...
}
else {
    <workspace-statement> ...
}
```

- The `else if` and `else` clauses are optional.
- Conditions are evaluated **once** when the `if` step is first reached; the
  chosen branch is recorded in the task state so subsequent runs of the same
  task do not re-evaluate conditions.
- Available conditions: [common conditions](#61-common-conditions) and
  [workspace-only conditions](#62-workspace-only-conditions).

#### Example

```text
if standalone {
    run "cargo" "publish";
} else {
    run "cargo" "publish" "--workspace";
}
```

### 4.7 `for crate in workspace`

Iterates over the member crates of the current workspace in intra-workspace
dependency order, executing the enclosed crate statements for each member.

```text
for crate in workspace {
    <crate-statement> ...
}
```

- Each member crate is processed with the crate's manifest directory as the
  working directory.
- Member crates that depend on other members in the same workspace are executed
  after their dependencies.
- Dev-dependencies do not affect execution order.

#### Example

```text
for crate in workspace {
    run "cargo" "publish" "--no-verify";
    wait_for_continue "Wait for crates.io to index before publishing the next crate.";
}
```

---

## 5. Crate statements

These statements appear inside a `for crate { ... }` block (or inside
`for crate in workspace { ... }` and nested blocks within those). Each
statement runs with the crate's manifest directory as its working directory.

### 5.1 `run`

Executes an external command in the crate's manifest directory.

```text
run "command" "arg1" "arg2" ... ;
```

Same semantics as [workspace `run`](#41-run).

#### Example

```text
run "cargo" "test";
run "cargo" "publish" "--no-verify";
```

### 5.2 `manual_step`

Pauses for a manual user action.

```text
manual_step "title" "instructions";
```

Same semantics as [workspace `manual_step`](#42-manual_step).

### 5.3 `snapshot_metadata`

Captures `cargo metadata` for the current crate's workspace.

```text
snapshot_metadata "name";
```

Same semantics as [workspace `snapshot_metadata`](#43-snapshot_metadata).

### 5.4 `wait_for_continue`

Pauses execution of this crate until released by the user.

```text
wait_for_continue "description";
```

Same semantics as [workspace `wait_for_continue`](#44-wait_for_continue).

### 5.5 `with_env_file`

Loads environment variables from a file relative to the crate directory.

```text
with_env_file "relative/path/to/file.env" {
    <crate-statement> ...
}
```

Same semantics as [workspace `with_env_file`](#45-with_env_file).

### 5.6 `if` / `else if` / `else`

Evaluates a condition and executes the matching branch.

```text
if <condition> {
    <crate-statement> ...
}
else if <condition> {
    <crate-statement> ...
}
else {
    <crate-statement> ...
}
```

Available conditions: [common conditions](#61-common-conditions) and
[crate-only conditions](#63-crate-only-conditions).

#### Example

```text
if type == lib {
    run "cargo" "publish";
} else if type == bin {
    run "cargo" "build" "--release";
}
```

---

## 6. Conditions

Conditions appear in `if` / `else if` guards and in `select` filter clauses.
Not all conditions are available in every position — see the subsections below.

### 6.1 Common conditions

Available everywhere a condition is accepted (workspace `if`, crate `if`).
Not available in `select` filter clauses.

| Syntax | Evaluates to `true` when… |
|--------|--------------------------|
| `ask_user "question"` | The user answers `y` or `yes` at the prompt. |
| `run "cmd" "arg"…` | The command exits with status 0. |
| `file_exists "path"` | A file at the given path (relative to the target directory) exists. |
| `working_directory_clean` | `git status --porcelain` produces no output in the target directory. |
| `git_config "key" == "value"` | The Git configuration key equals the given value in the target's repository. |

#### Examples

```text
if ask_user "Has the CHANGELOG been updated?" {
    run "git" "commit" "-am" "chore: release";
}

if run "test" "-f" "RELEASE_NOTES.md" {
    run "cat" "RELEASE_NOTES.md";
}

if file_exists ".env.publish" {
    with_env_file ".env.publish" {
        run "cargo" "publish";
    }
}

if working_directory_clean {
    run "git" "push";
}

if git_config "user.signingkey" == "ABC123DEF" {
    run "git" "tag" "-s" "v1.0.0";
}
```

### 6.2 Workspace-only conditions

Available in workspace `if` blocks (in addition to all common conditions).

| Syntax | Evaluates to `true` when… |
|--------|--------------------------|
| `standalone` | The workspace contains only a single crate (no workspace `members` array in `Cargo.toml`). |
| `has_members` | The workspace has multiple member crates. |

#### Examples

```text
if standalone {
    run "cargo" "publish";
} else {
    run "cargo" "publish" "--workspace";
}

if has_members {
    for crate in workspace {
        run "cargo" "publish" "--no-verify";
    }
}
```

### 6.3 Crate-only conditions

Available in crate `if` blocks (in addition to all common conditions).

| Syntax | Evaluates to `true` when… |
|--------|--------------------------|
| `type == bin` | The crate produces an executable binary. |
| `type == lib` | The crate is a library crate. |
| `type == proc_macro` | The crate is a procedural macro crate. |
| `type == cdylib` | The crate is a C-compatible dynamic library (e.g. for FFI or WebAssembly). |
| `type == dylib` | The crate is a Rust dynamic library. |
| `type == rlib` | The crate is a Rust static library (rlib). |
| `type == staticlib` | The crate is a C-compatible static library. |
| `type == bench` | The crate has a benchmark target. |
| `type == test` | The crate has an integration test target. |
| `type == example` | The crate has an example target. |
| `type == custom_build` | The crate has a custom build script (`build.rs`). |
| `standalone` | The crate lives in a standalone (single-crate) workspace. |

#### Examples

```text
if type == lib {
    run "cargo" "publish";
}

if type == proc_macro || type == lib {
    run "cargo" "test" "--all-features";
}

if standalone {
    run "cargo" "build" "--release";
}
```

### 6.4 Select-filter conditions

Used only in `select workspaces where` and `select crates where` clauses.
These are evaluated statically at task-creation time; dynamic conditions
(`ask_user`, `run`, `file_exists`, etc.) are not available here.

#### Workspace select filters

| Syntax | Selects the workspace when… |
|--------|----------------------------|
| `standalone` | The workspace contains only a single crate. |
| `has_members` | The workspace has multiple member crates. |

#### Crate select filters

| Syntax | Selects the crate when… |
|--------|------------------------|
| `standalone` | The crate lives in a standalone workspace. |
| `type == bin` | The crate is a binary crate. |
| `type == lib` | The crate is a library crate. |
| `type == proc_macro` | The crate is a procedural macro crate. |
| `type == cdylib` | The crate is a C-compatible dynamic library (e.g. for FFI or WebAssembly). |
| `type == dylib` | The crate is a Rust dynamic library. |
| `type == rlib` | The crate is a Rust static library (rlib). |
| `type == staticlib` | The crate is a C-compatible static library. |
| `type == bench` | The crate has a benchmark target. |
| `type == test` | The crate has an integration test target. |
| `type == example` | The crate has an example target. |
| `type == custom_build` | The crate has a custom build script (`build.rs`). |

### 6.5 Boolean operators

All condition contexts support the following operators. Operator precedence
(tightest to loosest): `!` → `&&` → `||`.  Parentheses `( )` may be used to
override precedence.

| Syntax | Meaning |
|--------|---------|
| `!cond` | True when `cond` is false. |
| `cond1 && cond2` | True when both are true (short-circuits on first false). |
| `cond1 \|\| cond2` | True when either is true (short-circuits on first true). |
| `(cond)` | Grouping. |

#### Examples

```text
if !standalone && working_directory_clean {
    run "cargo" "publish" "--workspace";
}

select crates where !(type == bin);

select workspaces where standalone || has_members;
```

---

## 7. String interpolation

Inside `run` arguments, `manual_step` title and instructions, the syntax
`${name.field}` is replaced at execution time with a value from a previously
taken [snapshot](#43-snapshot_metadata).

```text
${snapshot-name.field-path}
```

- `snapshot-name`: the name given to a `snapshot_metadata "name"` statement
  that has already executed for the current target.
- `field-path`: a `.`-separated path into the cargo metadata JSON of the
  current crate's package entry. For example `version`, `name`, or a nested
  field like `metadata.somekey`.

The snapshot stores the raw output of `cargo metadata --no-deps` for the
workspace; each interpolation looks up the package whose `manifest_path`
matches the current target's `Cargo.toml`.

### Commonly used fields

| Reference | Example value |
|-----------|---------------|
| `${meta.name}` | `my-crate` |
| `${meta.version}` | `1.2.3` |
| `${meta.description}` | `A useful crate.` |
| `${meta.license}` | `MIT OR Apache-2.0` |

#### Example

```text
snapshot_metadata "meta";
run "git" "tag" "-a" "v${meta.version}" "-m" "Release ${meta.name} v${meta.version}";
manual_step
    "Publish ${meta.name}"
    "Run cargo publish for ${meta.name} version ${meta.version}.";
```

---

## 8. Execution model

### Target ordering

Workspaces are executed in inter-workspace **dependency order**: if workspace A
has a member crate that depends on a crate in workspace B, then workspace B is
fully completed before workspace A begins.

Within a workspace, the `for crate in workspace` block iterates over member
crates in intra-workspace **dependency order**. Dev-dependencies are not
considered for ordering purposes.

Standalone crates (from `for crate { ... }`) are similarly executed in
dependency order.

### State and re-entrancy

Each statement's completion state is recorded on disk under the task state
directory. If execution is interrupted, it can be resumed by running the task
again — already-completed steps are skipped.

A `run` step with a non-zero exit status is marked as **failed** (❌) rather
than completed (✅). On resume, failed steps are **retried** from the
beginning of that step; preceding completed steps are not re-run.

An `if` block's condition is evaluated exactly once; the chosen branch is
recorded and re-used on resume without re-evaluating the condition.

### Task commands

| Command | Description |
|---------|-------------|
| `task create --name <n> --program <file>` | Create a task from a `.cfe` file. |
| `task create … --workspace <path>` | Override workspace selection with an explicit path (repeatable). |
| `task create … --crate <path>` | Override crate selection with an explicit path (repeatable). |
| `task describe --name <n>` | Show execution status for every target. |
| `task run single-step --name <n>` | Execute the next single statement. |
| `task run single-target --name <n>` | Run all statements for the first ready target. |
| `task run all-targets --name <n> [-j N] [-k]` | Run all targets; `-j` sets parallelism, `-k` keeps going on failure. |
| `task rewind single-step --name <n>` | Undo the last completed statement. |
| `task rewind single-target --name <n>` | Undo the last completed target. |
| `task rewind all-targets --name <n>` | Reset all execution state. |
| `task continue --name <n> --cursor <c>` | Release a `wait_for_continue` barrier at cursor `c`. |
| `task remove --name <n>` | Delete the task and all its state. |
| `task list` | List all tasks. |

---

## 9. Complete examples

### Publish all library crates in dependency order

```text
select crates where type == lib;

for crate {
    snapshot_metadata "meta";
    if working_directory_clean {
        run "cargo" "publish";
        wait_for_continue "Wait for crates.io to index v${meta.version} before continuing.";
    } else {
        manual_step
            "Uncommitted changes in ${meta.name}"
            "Commit or stash changes before publishing.";
    }
}
```

### Release all workspaces with a per-workspace `.env` file

```text
select workspaces;

for workspace {
    snapshot_metadata "meta";
    if file_exists ".env.release" {
        with_env_file ".env.release" {
            run "cargo" "publish" "--workspace";
            run "git" "tag" "v${meta.version}";
        }
    } else {
        run "cargo" "publish" "--workspace";
        run "git" "tag" "v${meta.version}";
    }
    run "git" "push" "--tags";
}
```

### Release a multi-crate workspace member by member

```text
select workspaces where has_members;

for workspace {
    for crate in workspace {
        snapshot_metadata "meta";
        if type == lib || type == proc_macro {
            run "cargo" "publish" "--no-verify";
            wait_for_continue "Wait for ${meta.name} to appear on crates.io.";
        }
    }
    manual_step
        "Tag the release"
        "Create and push a git tag for this workspace release.";
}
```

### Run tests only for binary crates

```text
select crates where type == bin;

for crate {
    run "cargo" "test" "--release";
    run "cargo" "clippy" "--" "-D" "warnings";
}
```
