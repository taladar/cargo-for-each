//! AST node types for the workspace execution context.

use super::common::{
    Branch, CommonCondition, IfBlock, ManualStepNode, RunStep, SnapshotMetadataNode,
    WaitForContinueNode, WithEnvFileBlock,
};
use super::crate_ctx::CrateStatement;

/// A block that iterates over all member crates of the current workspace in
/// intra-workspace dependency order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForCrateInWorkspaceBlock {
    /// The statements to execute for each member crate.
    pub statements: Vec<CrateStatement>,
}

/// A block that runs its body once for each selected workspace in inter-workspace
/// dependency order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForWorkspaceBlock {
    /// The statements to execute for each workspace.
    pub statements: Vec<WorkspaceStatement>,
}

/// A single statement in the workspace execution context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceStatement {
    /// Execute a command in the workspace root directory.
    Run(RunStep),
    /// Pause for a manual step with instructions for the user.
    ManualStep(ManualStepNode),
    /// Conditional branching using workspace-level conditions.
    If(IfBlock<WorkspaceCondition, Self>),
    /// Iterate over member crates of the current workspace in dependency order.
    ForCrateInWorkspace(ForCrateInWorkspaceBlock),
    /// Capture and store cargo metadata for the current workspace under a name.
    SnapshotMetadata(SnapshotMetadataNode),
    /// Execute nested statements with environment variables loaded from a file.
    WithEnvFile(WithEnvFileBlock<Self>),
    /// Pause execution until the user releases this barrier.
    WaitForContinue(WaitForContinueNode),
}

/// A boolean condition available in the workspace execution context.
///
/// Extends [`CommonCondition`] with conditions that inspect workspace-level properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCondition {
    /// A condition from the common set available in all contexts.
    Common(CommonCondition),
    /// True if this workspace is a standalone (single-crate) workspace.
    Standalone,
    /// True if this workspace has multiple member crates.
    HasMembers,
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    And(Vec<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    Or(Vec<Self>),
}

/// A condition allowed inside `select workspaces where ...` filters.
///
/// This is a restricted subset of [`WorkspaceCondition`] that can be evaluated
/// statically against the registered configuration at task-creation time. The
/// `ask_user` and `run` variants are excluded because they require interactive
/// evaluation which is not appropriate during target resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSelectCondition {
    /// True if the workspace is a standalone (single-crate) workspace.
    Standalone,
    /// True if the workspace has multiple member crates.
    HasMembers,
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    And(Vec<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    Or(Vec<Self>),
}

/// Converts a [`WorkspaceSelectCondition`] into a [`Branch`] condition by wrapping it.
impl From<WorkspaceSelectCondition> for WorkspaceCondition {
    fn from(cond: WorkspaceSelectCondition) -> Self {
        match cond {
            WorkspaceSelectCondition::Standalone => Self::Standalone,
            WorkspaceSelectCondition::HasMembers => Self::HasMembers,
            WorkspaceSelectCondition::Not(inner) => Self::Not(Box::new(Self::from(*inner))),
            WorkspaceSelectCondition::And(conditions) => {
                Self::And(conditions.into_iter().map(Self::from).collect())
            }
            WorkspaceSelectCondition::Or(conditions) => {
                Self::Or(conditions.into_iter().map(Self::from).collect())
            }
        }
    }
}

/// A filter applied to the set of workspaces selected by a `select workspaces` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFilter {
    /// Optional condition; if `None`, all registered workspaces are selected.
    pub condition: Option<WorkspaceSelectCondition>,
}

/// Type alias for a workspace if-branch used in the workspace context.
pub type WorkspaceBranch = Branch<WorkspaceCondition, WorkspaceStatement>;

/// Type alias for a workspace if-block used in the workspace context.
pub type WorkspaceIfBlock = IfBlock<WorkspaceCondition, WorkspaceStatement>;
