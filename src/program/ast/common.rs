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
    /// The ordered list of if/else-if branches. At least one is always present.
    pub branches: Vec<Branch<C, S>>,
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
    /// True if a file with the given name (or relative path) exists in the target's directory.
    FileExists(String),
    /// True if the working directory has no uncommitted changes (`git status --porcelain` is empty).
    WorkingDirectoryClean,
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    And(Vec<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    Or(Vec<Self>),
}
