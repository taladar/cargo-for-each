//! Execution cursor: a position within a running `.cfe` program.
//!
//! A [`ProgramCursor`] identifies a specific statement that is either about
//! to be executed or was just executed.  It is serialized to a sequence of
//! path-segment strings and joined with `"/"` to form the state directory
//! path used by the task runner.
//!
//! ## Path encoding
//!
//! | Segment          | Meaning                                      |
//! |------------------|----------------------------------------------|
//! | `w{N}`           | Nth workspace iteration in `for workspace`   |
//! | `c{N}`           | Nth crate iteration in `for crate [in ws]`   |
//! | `s{N}`           | Nth statement in current scope               |
//! | `if{N}`          | Branch N chosen in an `if` block             |
//! | `else`           | Else branch chosen in an `if` block          |
//!
//! ### Examples
//!
//! ```text
//! w1/s2/            workspace 1, statement 2
//! w1/s3/if0/s1/     workspace 1, stmt 3 (if), branch 0, stmt 1
//! w0/c2/s0/         workspace 0, crate 2, statement 0
//! c1/s0/            global-crate-loop crate 1, statement 0
//! ```

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// A single component of a [`ProgramCursor`] path.
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the cursor module"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorSegment {
    /// `w{N}` — the Nth workspace in a `for workspace` loop (0-based).
    WorkspaceIteration(usize),
    /// `c{N}` — the Nth crate in a `for crate` or `for crate in workspace` loop (0-based).
    CrateIteration(usize),
    /// `s{N}` — the Nth statement in the current block (0-based).
    Statement(usize),
    /// `if{N}` — branch N was chosen in an `if` block (0-based).
    IfBranch(usize),
    /// `else` — the else branch was chosen in an `if` block.
    ElseBranch,
}

impl fmt::Display for CursorSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceIteration(n) => write!(f, "w{n}"),
            Self::CrateIteration(n) => write!(f, "c{n}"),
            Self::Statement(n) => write!(f, "s{n}"),
            Self::IfBranch(n) => write!(f, "if{n}"),
            Self::ElseBranch => write!(f, "else"),
        }
    }
}

/// Error returned when a cursor segment cannot be parsed.
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the cursor module"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSegmentParseError(String);

impl fmt::Display for CursorSegmentParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid cursor segment: {:?}", self.0)
    }
}

impl std::error::Error for CursorSegmentParseError {}

impl FromStr for CursorSegment {
    type Err = CursorSegmentParseError;

    #[expect(
        clippy::map_err_ignore,
        reason = "unit error type intentionally discards parse error details"
    )]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "else" {
            return Ok(Self::ElseBranch);
        }
        if let Some(rest) = s.strip_prefix("w") {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError(s.to_owned()))?;
            return Ok(Self::WorkspaceIteration(n));
        }
        if let Some(rest) = s.strip_prefix("c") {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError(s.to_owned()))?;
            return Ok(Self::CrateIteration(n));
        }
        if let Some(rest) = s.strip_prefix("s") {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError(s.to_owned()))?;
            return Ok(Self::Statement(n));
        }
        if let Some(rest) = s.strip_prefix("if") {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError(s.to_owned()))?;
            return Ok(Self::IfBranch(n));
        }
        Err(CursorSegmentParseError(s.to_owned()))
    }
}

/// A cursor pointing to a specific statement within a running `.cfe` program.
///
/// The cursor is a sequence of [`CursorSegment`]s that together form a path
/// through the program's nested loop and branch structure.  It is used to:
///
/// 1. Name the state directory for a single statement execution
///    (e.g. `w0/c1/s2/`).
/// 2. Find the next statement to execute after an interruption.
/// 3. Rewind execution to a previous point.
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the cursor module"
)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ProgramCursor {
    /// The ordered sequence of path segments.
    segments: Vec<CursorSegment>,
}

impl ProgramCursor {
    /// Creates an empty cursor (points to the very start of a program).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    /// Creates a cursor from a pre-built segment sequence.
    #[must_use]
    pub const fn from_segments(segments: Vec<CursorSegment>) -> Self {
        Self { segments }
    }

    /// Returns a reference to the underlying segments.
    #[must_use]
    pub fn segments(&self) -> &[CursorSegment] {
        &self.segments
    }

    /// Appends a segment and returns the extended cursor.
    #[must_use]
    pub fn with(mut self, segment: CursorSegment) -> Self {
        self.segments.push(segment);
        self
    }

    /// Converts the cursor into a relative [`PathBuf`] by joining each segment
    /// as a path component.
    ///
    /// For example `[WorkspaceIteration(0), CrateIteration(1), Statement(2)]`
    /// becomes `PathBuf::from("w0/c1/s2")`.
    #[must_use]
    pub fn to_path(&self) -> PathBuf {
        self.segments
            .iter()
            .map(CursorSegment::to_string)
            .fold(PathBuf::new(), |acc, s| acc.join(s))
    }

    /// Converts the cursor into a `/`-terminated path string suitable for use
    /// as a state directory suffix.
    #[must_use]
    pub fn to_path_string(&self) -> String {
        if self.segments.is_empty() {
            return String::new();
        }
        let mut s = self
            .segments
            .iter()
            .map(CursorSegment::to_string)
            .collect::<Vec<_>>()
            .join("/");
        s.push('/');
        s
    }

    /// Parses a cursor from a `/`-separated path string.
    ///
    /// # Errors
    ///
    /// Returns a [`CursorSegmentParseError`] if any segment cannot be parsed.
    pub fn from_path_string(s: &str) -> Result<Self, CursorSegmentParseError> {
        let segments = s
            .split('/')
            .filter(|part| !part.is_empty())
            .map(CursorSegment::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { segments })
    }

    /// Returns `true` if this cursor has no segments (i.e. points to the program root).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Returns the number of segments in this cursor.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.segments.len()
    }
}

impl fmt::Display for ProgramCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_path_string())
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helpers use panic! to fail on unexpected errors"
    )]
    #![expect(
        clippy::assertions_on_result_states,
        reason = "test code asserts is_err() on parse results"
    )]

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn segment_display_workspace() {
        assert_eq!(CursorSegment::WorkspaceIteration(3).to_string(), "w3");
    }

    #[test]
    fn segment_display_crate() {
        assert_eq!(CursorSegment::CrateIteration(0).to_string(), "c0");
    }

    #[test]
    fn segment_display_statement() {
        assert_eq!(CursorSegment::Statement(7).to_string(), "s7");
    }

    #[test]
    fn segment_display_if_branch() {
        assert_eq!(CursorSegment::IfBranch(2).to_string(), "if2");
    }

    #[test]
    fn segment_display_else() {
        assert_eq!(CursorSegment::ElseBranch.to_string(), "else");
    }

    #[test]
    fn segment_parse_workspace() {
        assert_eq!(
            "w5".parse::<CursorSegment>(),
            Ok(CursorSegment::WorkspaceIteration(5))
        );
    }

    #[test]
    fn segment_parse_crate() {
        assert_eq!(
            "c0".parse::<CursorSegment>(),
            Ok(CursorSegment::CrateIteration(0))
        );
    }

    #[test]
    fn segment_parse_statement() {
        assert_eq!(
            "s10".parse::<CursorSegment>(),
            Ok(CursorSegment::Statement(10))
        );
    }

    #[test]
    fn segment_parse_if_branch() {
        assert_eq!(
            "if1".parse::<CursorSegment>(),
            Ok(CursorSegment::IfBranch(1))
        );
    }

    #[test]
    fn segment_parse_else() {
        assert_eq!(
            "else".parse::<CursorSegment>(),
            Ok(CursorSegment::ElseBranch)
        );
    }

    #[test]
    fn segment_parse_invalid() {
        assert!("xyz".parse::<CursorSegment>().is_err());
        assert!("wX".parse::<CursorSegment>().is_err());
        assert!("".parse::<CursorSegment>().is_err());
    }

    #[test]
    fn cursor_to_path() {
        let cursor = ProgramCursor::from_segments(vec![
            CursorSegment::WorkspaceIteration(1),
            CursorSegment::Statement(2),
        ]);
        assert_eq!(cursor.to_path(), PathBuf::from("w1/s2"));
    }

    #[test]
    fn cursor_to_path_string() {
        let cursor = ProgramCursor::from_segments(vec![
            CursorSegment::WorkspaceIteration(0),
            CursorSegment::CrateIteration(2),
            CursorSegment::Statement(0),
        ]);
        assert_eq!(cursor.to_path_string(), "w0/c2/s0/");
    }

    #[test]
    fn cursor_empty_path_string() {
        let cursor = ProgramCursor::new();
        assert_eq!(cursor.to_path_string(), "");
    }

    #[test]
    fn cursor_roundtrip_from_path_string() {
        let original = ProgramCursor::from_segments(vec![
            CursorSegment::WorkspaceIteration(1),
            CursorSegment::Statement(3),
            CursorSegment::IfBranch(0),
            CursorSegment::Statement(1),
        ]);
        let s = original.to_path_string();
        let parsed = ProgramCursor::from_path_string(&s).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(parsed, original);
    }

    #[test]
    fn cursor_from_path_string_empty() {
        let cursor = ProgramCursor::from_path_string("").unwrap_or_else(|e| panic!("{e}"));
        assert!(cursor.is_empty());
    }

    #[test]
    fn cursor_with_builder() {
        let cursor = ProgramCursor::new()
            .with(CursorSegment::WorkspaceIteration(0))
            .with(CursorSegment::Statement(5));
        assert_eq!(cursor.segments().len(), 2);
        assert_eq!(cursor.to_path_string(), "w0/s5/");
    }

    #[test]
    fn cursor_len() {
        let cursor = ProgramCursor::from_segments(vec![
            CursorSegment::CrateIteration(0),
            CursorSegment::Statement(1),
        ]);
        assert_eq!(cursor.len(), 2);
    }
}
