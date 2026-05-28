//! AST node types for the workspace execution context.

use super::common::{
    AtLeastTwo, Branch, CommonCondition, IfBlock, ManualStepNode, RunStep, SnapshotMetadataNode,
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
    /// True if this workspace has a `[workspace]` table (one or more members).
    HasMembers,
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    /// The [`AtLeastTwo`] wrapper enforces that `&&` always has at least two
    /// operands.
    And(AtLeastTwo<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    /// Same `>= 2` invariant as [`Self::And`].
    Or(AtLeastTwo<Self>),
}

impl std::fmt::Display for WorkspaceCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Common(inner) => write!(f, "{inner}"),
            Self::Standalone => write!(f, "standalone"),
            Self::HasMembers => write!(f, "has_members"),
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
    /// True if the workspace has a `[workspace]` table (one or more members).
    HasMembers,
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    /// `&&` always has at least two operands (enforced by [`AtLeastTwo`]).
    And(AtLeastTwo<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    /// `||` always has at least two operands (enforced by [`AtLeastTwo`]).
    Or(AtLeastTwo<Self>),
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

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::{AtLeastTwo, CommonCondition, WorkspaceCondition};

    #[test]
    fn workspace_condition_display_leaf_variants() {
        assert_eq!(
            WorkspaceCondition::Common(CommonCondition::FileExists("Cargo.toml".to_owned()))
                .to_string(),
            r#"file_exists("Cargo.toml")"#,
        );
        assert_eq!(WorkspaceCondition::Standalone.to_string(), "standalone");
        assert_eq!(WorkspaceCondition::HasMembers.to_string(), "has_members");
    }

    #[test]
    fn workspace_condition_display_nested_not_and_or() {
        assert_eq!(
            WorkspaceCondition::Not(Box::new(WorkspaceCondition::HasMembers)).to_string(),
            "!has_members",
        );

        let and = WorkspaceCondition::And(AtLeastTwo::from_pair(
            WorkspaceCondition::Standalone,
            WorkspaceCondition::HasMembers,
        ));
        assert_eq!(and.to_string(), "(standalone && has_members)");

        let mut operands = AtLeastTwo::from_pair(
            WorkspaceCondition::Common(CommonCondition::WorkingDirectoryClean),
            and,
        );
        operands.push(WorkspaceCondition::Not(Box::new(
            WorkspaceCondition::Standalone,
        )));
        let or = WorkspaceCondition::Or(operands);
        assert_eq!(
            or.to_string(),
            "(working_directory_clean || (standalone && has_members) || !standalone)",
        );
    }
}
