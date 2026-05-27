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
//! | `env`            | Body of a `with_env_file` block              |
//!
//! The `s{N}` indices are scope-local: they restart at `s0` inside each
//! block (`if` branch, `else` branch, `with_env_file` body, loop body),
//! so siblings at different nesting depths can share the same `s{N}`.
//!
//! ### Examples
//!
//! ```text
//! w1/s2/            workspace 1, statement 2
//! w1/s3/if0/s1/     workspace 1, stmt 3 (if), branch 0, stmt 1
//! w0/c2/s0/         workspace 0, crate 2, statement 0
//! c1/s0/            global-crate-loop crate 1, statement 0
//! w0/s1/env/s0/     workspace 0, stmt 1 (with_env_file), nested stmt 0
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
    /// `env` — the body of a `with_env_file` block.
    WithEnvFile,
}

impl fmt::Display for CursorSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceIteration(n) => write!(f, "w{n}"),
            Self::CrateIteration(n) => write!(f, "c{n}"),
            Self::Statement(n) => write!(f, "s{n}"),
            Self::IfBranch(n) => write!(f, "if{n}"),
            Self::ElseBranch => write!(f, "else"),
            Self::WithEnvFile => write!(f, "env"),
        }
    }
}

/// Error returned when a cursor path string cannot be parsed.
///
/// Covers both per-segment failures (`InvalidSegment`) and structural
/// problems with the surrounding path (`LeadingSlash`, `ConsecutiveSlashes`,
/// `EmptyPath`).
#[expect(
    clippy::module_name_repetitions,
    reason = "name is intentional within the cursor module"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorSegmentParseError {
    /// A `/`-separated segment did not match any known cursor segment shape.
    InvalidSegment(String),
    /// The input started with `/`, which would imply an empty leading segment.
    LeadingSlash,
    /// The input contained `//`, which would imply an empty interior segment.
    ConsecutiveSlashes,
    /// The input was non-empty but contained no real segments (e.g. `"/"`).
    EmptyPath,
}

impl fmt::Display for CursorSegmentParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSegment(s) => write!(f, "invalid cursor segment: {s:?}"),
            Self::LeadingSlash => f.write_str("cursor path must not start with `/`"),
            Self::ConsecutiveSlashes => f.write_str("cursor path must not contain `//`"),
            Self::EmptyPath => f.write_str("cursor path contains no segments"),
        }
    }
}

impl std::error::Error for CursorSegmentParseError {}

/// Returns `true` if `s` is non-empty and contains only ASCII digits (`0`–`9`).
fn is_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

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
        if s == "env" {
            return Ok(Self::WithEnvFile);
        }
        // The single-letter prefix checks below require the rest to be all
        // ASCII digits; otherwise we fall through so a future segment kind
        // whose token happens to start with `w`, `c`, or `s` (e.g. `wait`,
        // `cont`, `skip`) is not silently consumed as an invalid numeric
        // segment.
        if let Some(rest) = s.strip_prefix("w")
            && is_ascii_digits(rest)
        {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError::InvalidSegment(s.to_owned()))?;
            return Ok(Self::WorkspaceIteration(n));
        }
        if let Some(rest) = s.strip_prefix("c")
            && is_ascii_digits(rest)
        {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError::InvalidSegment(s.to_owned()))?;
            return Ok(Self::CrateIteration(n));
        }
        if let Some(rest) = s.strip_prefix("s")
            && is_ascii_digits(rest)
        {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError::InvalidSegment(s.to_owned()))?;
            return Ok(Self::Statement(n));
        }
        if let Some(rest) = s.strip_prefix("if")
            && is_ascii_digits(rest)
        {
            let n = rest
                .parse::<usize>()
                .map_err(|_| CursorSegmentParseError::InvalidSegment(s.to_owned()))?;
            return Ok(Self::IfBranch(n));
        }
        Err(CursorSegmentParseError::InvalidSegment(s.to_owned()))
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
    /// The empty string is accepted and yields the empty cursor (matching
    /// `ProgramCursor::new().to_path_string()`).  Otherwise the input must
    /// contain at least one non-empty segment, must not start with `/`, and
    /// must not contain `//`.  A single trailing `/` is allowed so that
    /// strings produced by [`to_path_string`](Self::to_path_string) round-trip
    /// cleanly.
    ///
    /// # Errors
    ///
    /// Returns [`CursorSegmentParseError::LeadingSlash`] if the input starts
    /// with `/`, [`CursorSegmentParseError::ConsecutiveSlashes`] if it
    /// contains `//`, [`CursorSegmentParseError::EmptyPath`] if it is
    /// non-empty but yields no segments, and
    /// [`CursorSegmentParseError::InvalidSegment`] if any segment is
    /// individually unparsable.
    pub fn from_path_string(s: &str) -> Result<Self, CursorSegmentParseError> {
        if s.is_empty() {
            return Ok(Self::new());
        }
        if s.starts_with('/') {
            return Err(CursorSegmentParseError::LeadingSlash);
        }
        if s.contains("//") {
            return Err(CursorSegmentParseError::ConsecutiveSlashes);
        }
        // A single trailing `/` from `to_path_string` is the only legitimate
        // source of an empty segment after `split('/')`; allow it but require
        // at least one real segment before it.
        let segments: Vec<&str> = s.split('/').filter(|part| !part.is_empty()).collect();
        if segments.is_empty() {
            return Err(CursorSegmentParseError::EmptyPath);
        }
        let segments = segments
            .into_iter()
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
    fn segment_display_with_env_file() {
        assert_eq!(CursorSegment::WithEnvFile.to_string(), "env");
    }

    #[test]
    fn segment_parse_with_env_file() {
        assert_eq!(
            "env".parse::<CursorSegment>(),
            Ok(CursorSegment::WithEnvFile)
        );
    }

    #[test]
    fn cursor_roundtrip_with_env_file() {
        let original = ProgramCursor::from_segments(vec![
            CursorSegment::WorkspaceIteration(0),
            CursorSegment::Statement(1),
            CursorSegment::WithEnvFile,
            CursorSegment::Statement(0),
        ]);
        let s = original.to_path_string();
        assert_eq!(s, "w0/s1/env/s0/");
        let parsed = ProgramCursor::from_path_string(&s).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(parsed, original);
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

    fn expect_err(s: &str) -> CursorSegmentParseError {
        match ProgramCursor::from_path_string(s) {
            Ok(_) => panic!("expected parse error for {s:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn cursor_from_path_string_rejects_leading_slash() {
        assert_eq!(expect_err("/"), CursorSegmentParseError::LeadingSlash);
        assert_eq!(expect_err("/w0"), CursorSegmentParseError::LeadingSlash);
    }

    #[test]
    fn cursor_from_path_string_rejects_consecutive_slashes() {
        assert_eq!(
            expect_err("w0//s0"),
            CursorSegmentParseError::ConsecutiveSlashes,
        );
        // `//` alone trips the leading-slash check first, which is fine —
        // both are reasons to reject, and "starts with /" is the more
        // specific diagnostic.
        assert_eq!(expect_err("//"), CursorSegmentParseError::LeadingSlash);
    }

    #[test]
    fn cursor_from_path_string_accepts_trailing_slash() {
        // `to_path_string` always emits a trailing `/`; round-tripping must
        // still work.
        let cursor = ProgramCursor::from_path_string("w0/s2/").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cursor.segments().len(), 2);
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
