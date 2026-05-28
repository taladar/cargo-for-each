//! AST node types shared across all execution contexts.

/// A step that executes an external command in the target's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStep {
    /// The command to execute.
    pub command: String,
    /// The arguments to pass to the command.
    pub args: Vec<String>,
}

/// A step that pauses for manual user intervention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualStepNode {
    /// A short title displayed to the user.
    pub title: String,
    /// Detailed instructions for the manual step.
    pub instructions: String,
}

/// A conditional if/else-if/else block parameterized over condition and statement types.
///
/// The type parameter `C` is the condition type for the context (e.g. `WorkspaceCondition`
/// or `CrateCondition`), and `S` is the statement type for the body of each branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfBlock<C, S> {
    /// The ordered list of if/else-if branches. The [`NonEmptyBranches`]
    /// newtype enforces "at least one always present" at the type level.
    pub branches: NonEmptyBranches<C, S>,
    /// Statements in the else block. Empty means no else clause.
    pub else_statements: Vec<S>,
}

/// A single conditional branch (if or else-if arm) in an [`IfBlock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch<C, S> {
    /// The condition that must be true for this branch to execute.
    pub condition: C,
    /// The statements executed when this branch is chosen.
    pub statements: Vec<S>,
}

/// A non-empty list of [`Branch`]es, used by [`IfBlock`] to enforce the
/// "at least one branch" invariant at the type level.  Construct via
/// [`Self::try_new`] or [`Self::from_first_and_rest`]; access via the
/// `Deref` impl to a `[Branch<C, S>]` slice (so `iter`, `get`, `len`,
/// etc. all work).
///
/// The wrapped `Vec` is private precisely to prevent direct construction
/// like `IfBlock { branches: vec![], … }` from bypassing the invariant —
/// that's the regression the original "doc-only" version of this rule
/// invited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyBranches<C, S>(Vec<Branch<C, S>>);

/// Error returned by [`NonEmptyBranches::try_new`] when the supplied
/// `Vec` is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyBranchesError;

impl std::fmt::Display for EmptyBranchesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an `if` block must have at least one branch")
    }
}

impl std::error::Error for EmptyBranchesError {}

impl<C, S> NonEmptyBranches<C, S> {
    /// Construct from a `Vec`, returning an error if it is empty.
    ///
    /// # Errors
    ///
    /// Returns [`EmptyBranchesError`] when `branches.is_empty()`.
    pub fn try_new(branches: Vec<Branch<C, S>>) -> Result<Self, EmptyBranchesError> {
        if branches.is_empty() {
            Err(EmptyBranchesError)
        } else {
            Ok(Self(branches))
        }
    }

    /// Construct from a guaranteed-non-empty `(first, rest)` pair.
    ///
    /// Useful at sites where non-emptiness is enforced upstream — e.g. the
    /// chumsky parser combinator that requires at least one branch before
    /// any `else if` repetitions.
    pub fn from_first_and_rest(first: Branch<C, S>, rest: Vec<Branch<C, S>>) -> Self {
        let mut all = Vec::with_capacity(rest.len().saturating_add(1));
        all.push(first);
        all.extend(rest);
        Self(all)
    }
}

impl<C, S> std::ops::Deref for NonEmptyBranches<C, S> {
    type Target = [Branch<C, S>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a, C, S> IntoIterator for &'a NonEmptyBranches<C, S> {
    type Item = &'a Branch<C, S>;
    type IntoIter = std::slice::Iter<'a, Branch<C, S>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A list of at least two items.  Used by the `And` and `Or` variants of
/// every condition enum: an `And` or `Or` with fewer than two operands
/// would either degenerate to its single operand or to a trivial constant
/// (`true` for empty `And`, `false` for empty `Or`), neither of which has
/// a use case in a hand-authored `.cfe` program.
///
/// Construct via [`Self::try_new`] (returns `Err` for < 2 items) or
/// [`Self::from_pair`] (always succeeds).  The wrapped `Vec` is private
/// so the only way to *shrink* below the invariant is to drop the value
/// entirely.  [`Self::push`] adds further items in place — always safe,
/// because adding never violates the lower bound.  Read access is via
/// `Deref<Target = [T]>`, so existing call sites that use `.iter()`,
/// `.len()`, indexing, etc. keep working unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtLeastTwo<T>(Vec<T>);

/// Error returned by [`AtLeastTwo::try_new`] when the supplied `Vec` has
/// fewer than two items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooFewItemsError {
    /// The actual number of items that were supplied.
    pub got: usize,
}

impl std::fmt::Display for TooFewItemsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "an `&&` or `||` expression must have at least two operands; got {}",
            self.got,
        )
    }
}

impl std::error::Error for TooFewItemsError {}

impl<T> AtLeastTwo<T> {
    /// Construct from a `Vec`, returning an error if it has fewer than
    /// two items.
    ///
    /// # Errors
    ///
    /// Returns [`TooFewItemsError`] when `items.len() < 2`.
    pub fn try_new(items: Vec<T>) -> Result<Self, TooFewItemsError> {
        if items.len() < 2 {
            Err(TooFewItemsError { got: items.len() })
        } else {
            Ok(Self(items))
        }
    }

    /// Construct from exactly two items.  Always succeeds; used at parser
    /// sites where the combinator already guarantees a pair.
    pub fn from_pair(first: T, second: T) -> Self {
        Self(vec![first, second])
    }

    /// Append another item.  The invariant is preserved trivially — we
    /// only ever grow the inner `Vec`, never shrink it.
    pub fn push(&mut self, item: T) {
        self.0.push(item);
    }
}

impl<T> std::ops::Deref for AtLeastTwo<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a, T> IntoIterator for &'a AtLeastTwo<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A step that captures the current workspace's cargo metadata under a user-specified name.
///
/// The captured metadata can be referenced in later steps using `${name.field}` syntax
/// in command arguments and manual step text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMetadataNode {
    /// The name under which the captured metadata is stored.
    ///
    /// This name is used to reference the snapshot in `${name.field}` interpolations.
    pub name: String,
}

/// A barrier that pauses execution of this target until the user releases it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitForContinueNode {
    /// Human-readable description shown when the barrier is reached.
    pub description: String,
}

/// A block that applies environment variables from a file to all nested statements.
///
/// The env file is read at execution time. Relative paths resolve against the target's
/// manifest directory; absolute paths are also accepted (useful for shared API-credential
/// files kept outside the project).
/// Variables from nested `with_env_file` blocks extend (and override, for duplicate keys)
/// variables from outer blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithEnvFileBlock<S> {
    /// Path to the env file. Relative paths resolve against the target's manifest
    /// directory; absolute paths are accepted as-is.
    pub env_file: String,
    /// Statements to execute with the env file's variables applied.
    pub statements: Vec<S>,
}

impl std::fmt::Display for CommonCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AskUser(q) => write!(f, "ask_user({q:?})"),
            Self::RunCommand { command, args } => {
                write!(f, "run {command:?}")?;
                for a in args {
                    write!(f, " {a:?}")?;
                }
                Ok(())
            }
            Self::FileExists(path) => write!(f, "file_exists({path:?})"),
            Self::WorkingDirectoryClean => write!(f, "working_directory_clean"),
            Self::GitConfigEquals { key, value } => {
                write!(f, "git_config.{key} == {value:?}")
            }
            Self::Not(inner) => write!(f, "!{inner}"),
            Self::And(conditions) => {
                write!(f, "(")?;
                for (i, c) in conditions.iter().enumerate() {
                    if i > 0 {
                        write!(f, " && ")?;
                    }
                    write!(f, "{c}")?;
                }
                write!(f, ")")
            }
            Self::Or(conditions) => {
                write!(f, "(")?;
                for (i, c) in conditions.iter().enumerate() {
                    if i > 0 {
                        write!(f, " || ")?;
                    }
                    write!(f, "{c}")?;
                }
                write!(f, ")")
            }
        }
    }
}

/// A boolean condition available in all execution contexts.
///
/// This represents the subset of conditions that do not depend on workspace- or
/// crate-specific information and can therefore be used anywhere.
#[expect(
    clippy::module_name_repetitions,
    reason = "The 'Common' prefix is semantically meaningful as it distinguishes this from WorkspaceCondition and CrateCondition; renaming would lose that clarity"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommonCondition {
    /// Ask the user a yes/no question. Evaluates to true if the user answers yes/y.
    AskUser(String),
    /// Run a command; evaluates to true if the command exits with code 0.
    RunCommand {
        /// The command to execute.
        command: String,
        /// The arguments to pass to the command.
        args: Vec<String>,
    },
    /// True if a file at the given path exists. Relative paths resolve against the target's
    /// directory; absolute paths and `..` traversal are accepted only if the resulting path
    /// stays within the enclosing workspace's manifest directory.
    FileExists(String),
    /// True if the working directory has no uncommitted changes (`git status --porcelain` is empty).
    WorkingDirectoryClean,
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    /// The [`AtLeastTwo`] wrapper enforces that `&&` always has at least two
    /// operands; a one-operand or empty conjunction has no use case in a
    /// hand-authored program.
    And(AtLeastTwo<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    /// Same `>= 2` invariant as [`Self::And`].
    Or(AtLeastTwo<Self>),
    /// True if the specified Git configuration key equals the specified value in the target's repository.
    GitConfigEquals {
        /// The Git configuration key to check (e.g. `user.name`, `init.defaultbranch`).
        key: String,
        /// The value to compare against.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helpers panic on unexpected error shapes; clearer than assert"
    )]

    use pretty_assertions::assert_eq;

    use super::{AtLeastTwo, Branch, CommonCondition, EmptyBranchesError, NonEmptyBranches};

    /// Trivial dummy condition / statement types so we can instantiate the
    /// generic [`NonEmptyBranches`] without pulling in the workspace/crate
    /// AST modules.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Cond;
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Stmt;

    fn branch() -> Branch<Cond, Stmt> {
        Branch {
            condition: Cond,
            statements: vec![],
        }
    }

    #[test]
    fn try_new_rejects_empty() {
        let result: Result<NonEmptyBranches<Cond, Stmt>, _> = NonEmptyBranches::try_new(vec![]);
        assert_eq!(result, Err(EmptyBranchesError));
    }

    #[test]
    fn try_new_accepts_non_empty() {
        let result = NonEmptyBranches::try_new(vec![branch()]);
        let nb = match result {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got {e}"),
        };
        assert_eq!(nb.len(), 1);
    }

    #[test]
    fn from_first_and_rest_carries_first_then_rest() {
        let nb = NonEmptyBranches::from_first_and_rest(branch(), vec![branch(), branch()]);
        assert_eq!(nb.len(), 3);
    }

    #[test]
    fn iter_via_deref_yields_all_branches() {
        let nb = NonEmptyBranches::from_first_and_rest(branch(), vec![branch()]);
        let count = nb.iter().count();
        assert_eq!(count, 2);
    }

    #[test]
    fn at_least_two_try_new_rejects_under_two() {
        for items in [vec![], vec![1_i32]] {
            let n = items.len();
            assert_eq!(
                super::AtLeastTwo::try_new(items),
                Err(super::TooFewItemsError { got: n }),
            );
        }
    }

    #[test]
    fn at_least_two_try_new_accepts_two_or_more() {
        let two = match super::AtLeastTwo::try_new(vec![1_i32, 2]) {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got {e}"),
        };
        let three = match super::AtLeastTwo::try_new(vec![1_i32, 2, 3]) {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got {e}"),
        };
        assert_eq!(two.len(), 2);
        assert_eq!(three.len(), 3);
    }

    #[test]
    fn at_least_two_from_pair_and_push_grow_correctly() {
        let mut v = super::AtLeastTwo::from_pair(1_i32, 2);
        assert_eq!(v.len(), 2);
        v.push(3);
        v.push(4);
        assert_eq!(v.len(), 4);
        // Deref to slice for iteration.
        let collected: Vec<_> = v.iter().copied().collect();
        assert_eq!(collected, vec![1, 2, 3, 4]);
    }

    // ── CommonCondition Display ───────────────────────────────────────────────

    #[test]
    fn common_condition_display_leaf_variants() {
        assert_eq!(
            CommonCondition::AskUser("Proceed?".to_owned()).to_string(),
            r#"ask_user("Proceed?")"#,
        );
        assert_eq!(
            CommonCondition::RunCommand {
                command: "test".to_owned(),
                args: vec!["-f".to_owned(), "Cargo.toml".to_owned()],
            }
            .to_string(),
            r#"run "test" "-f" "Cargo.toml""#,
        );
        assert_eq!(
            CommonCondition::RunCommand {
                command: "true".to_owned(),
                args: vec![],
            }
            .to_string(),
            r#"run "true""#,
        );
        assert_eq!(
            CommonCondition::FileExists("README.md".to_owned()).to_string(),
            r#"file_exists("README.md")"#,
        );
        assert_eq!(
            CommonCondition::WorkingDirectoryClean.to_string(),
            "working_directory_clean",
        );
        assert_eq!(
            CommonCondition::GitConfigEquals {
                key: "user.name".to_owned(),
                value: "Alice".to_owned(),
            }
            .to_string(),
            r#"git_config.user.name == "Alice""#,
        );
    }

    #[test]
    fn common_condition_display_nested_not_and_or() {
        let inner = CommonCondition::Not(Box::new(CommonCondition::WorkingDirectoryClean));
        assert_eq!(inner.to_string(), "!working_directory_clean");

        let and = CommonCondition::And(AtLeastTwo::from_pair(
            CommonCondition::FileExists("a".to_owned()),
            CommonCondition::FileExists("b".to_owned()),
        ));
        assert_eq!(and.to_string(), r#"(file_exists("a") && file_exists("b"))"#);

        // Three-operand Or, with a nested And and a negation to exercise recursion.
        let mut operands =
            AtLeastTwo::from_pair(CommonCondition::WorkingDirectoryClean, and.clone());
        operands.push(CommonCondition::Not(Box::new(CommonCondition::FileExists(
            "c".to_owned(),
        ))));
        let or = CommonCondition::Or(operands);
        assert_eq!(
            or.to_string(),
            r#"(working_directory_clean || (file_exists("a") && file_exists("b")) || !file_exists("c"))"#,
        );
    }
}
