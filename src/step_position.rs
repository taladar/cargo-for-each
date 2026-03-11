//! Defines the `StepPosition` type for addressing steps in a plan hierarchy.

/// Represents a position in a (possibly nested) plan step hierarchy.
///
/// Each component is a 1-based positive integer. The serialized/display form
/// uses `.` as a separator, e.g. `"1"`, `"1.2"`, `"3.1.2"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepPosition(Vec<std::num::NonZeroUsize>);

impl std::fmt::Display for StepPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut iter = self.0.iter();
        if let Some(first) = iter.next() {
            write!(f, "{first}")?;
            for component in iter {
                write!(f, ".{component}")?;
            }
        }
        Ok(())
    }
}

/// Error returned when parsing a [`StepPosition`] from a string fails.
#[expect(
    clippy::module_name_repetitions,
    reason = "full name needed for external clarity"
)]
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum StepPositionParseError {
    /// The input string was empty.
    #[error("step position string is empty")]
    Empty,
    /// A component of the position was zero (components must be >= 1).
    #[error("step position component must be >= 1, got 0")]
    ZeroComponent,
    /// A component could not be parsed as a positive integer.
    #[error("invalid step position component {0:?}: {1}")]
    InvalidComponent(String, std::num::ParseIntError),
}

impl std::str::FromStr for StepPosition {
    type Err = StepPositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(StepPositionParseError::Empty);
        }
        let mut components = Vec::new();
        for part in s.split('.') {
            let n: usize = part
                .parse()
                .map_err(|e| StepPositionParseError::InvalidComponent(part.to_owned(), e))?;
            let nz = std::num::NonZeroUsize::new(n).ok_or(StepPositionParseError::ZeroComponent)?;
            components.push(nz);
        }
        Ok(Self(components))
    }
}

impl StepPosition {
    /// Returns a `StepPosition` for a single 1-based position value.
    /// Returns `None` if `n` is zero.
    #[must_use]
    pub fn from_one_based(n: usize) -> Option<Self> {
        std::num::NonZeroUsize::new(n).map(|nz| Self(vec![nz]))
    }

    /// Creates a `StepPosition` from a 0-based step array index (from `enumerate()`).
    /// Returns `None` only if `idx == usize::MAX` (impossible in practice).
    #[must_use]
    pub fn from_step_index(idx: usize) -> Option<Self> {
        idx.checked_add(1)
            .and_then(std::num::NonZeroUsize::new)
            .map(|nz| Self(vec![nz]))
    }

    /// Returns the 0-based array index corresponding to the top-level component.
    /// Returns `None` if the position has no components (impossible after construction).
    #[must_use]
    pub fn to_top_level_index(&self) -> Option<usize> {
        self.0.first().map(|nz| nz.get().saturating_sub(1))
    }

    /// Returns the depth (number of components) of this position.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if this is a single-component (top-level only) position.
    #[must_use]
    pub const fn is_top_level(&self) -> bool {
        self.0.len() == 1
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::{StepPosition, StepPositionParseError};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn test_display_single() -> TestResult {
        let pos = StepPosition::from_one_based(1).ok_or("position 1 is always valid")?;
        assert_eq!(pos.to_string(), "1");
        Ok(())
    }

    #[test]
    fn test_display_nested() -> TestResult {
        let pos: StepPosition = "3.1.2".parse()?;
        assert_eq!(pos.to_string(), "3.1.2");
        Ok(())
    }

    #[test]
    fn test_from_str_valid_single() -> TestResult {
        let pos: StepPosition = "5".parse()?;
        assert_eq!(
            pos,
            StepPosition::from_one_based(5).ok_or("position 5 is always valid")?
        );
        Ok(())
    }

    #[test]
    fn test_from_str_valid_nested() -> TestResult {
        let pos: StepPosition = "1.2".parse()?;
        assert_eq!(pos.to_string(), "1.2");
        assert_eq!(pos.depth(), 2);
        Ok(())
    }

    #[test]
    fn test_from_str_empty() {
        let result: Result<StepPosition, _> = "".parse();
        assert_eq!(result, Err(StepPositionParseError::Empty));
    }

    #[test]
    fn test_from_str_zero_component() {
        let result: Result<StepPosition, _> = "0".parse();
        assert_eq!(result, Err(StepPositionParseError::ZeroComponent));
    }

    #[test]
    fn test_from_str_zero_in_nested() {
        let result: Result<StepPosition, _> = "1.0".parse();
        assert_eq!(result, Err(StepPositionParseError::ZeroComponent));
    }

    #[test]
    fn test_from_str_invalid_component() {
        let result: Result<StepPosition, _> = "abc".parse();
        assert!(matches!(
            result,
            Err(StepPositionParseError::InvalidComponent(ref s, _)) if s == "abc"
        ));
    }

    #[test]
    fn test_to_top_level_index() -> TestResult {
        let pos = StepPosition::from_one_based(3).ok_or("position 3 is always valid")?;
        assert_eq!(pos.to_top_level_index(), Some(2));
        Ok(())
    }

    #[test]
    fn test_from_step_index() -> TestResult {
        let pos = StepPosition::from_step_index(0).ok_or("index 0 is always valid")?;
        assert_eq!(
            pos,
            StepPosition::from_one_based(1).ok_or("position 1 is always valid")?
        );
        assert_eq!(pos.to_string(), "1");
        Ok(())
    }

    #[test]
    fn test_from_step_index_two() -> TestResult {
        let pos = StepPosition::from_step_index(2).ok_or("index 2 is always valid")?;
        assert_eq!(pos.to_string(), "3");
        assert_eq!(pos.to_top_level_index(), Some(2));
        Ok(())
    }

    #[test]
    fn test_is_top_level_true() -> TestResult {
        let pos = StepPosition::from_one_based(7).ok_or("position 7 is always valid")?;
        assert!(pos.is_top_level());
        Ok(())
    }

    #[test]
    fn test_is_top_level_false() -> TestResult {
        let pos: StepPosition = "1.2".parse()?;
        assert!(!pos.is_top_level());
        Ok(())
    }

    #[test]
    fn test_display_roundtrip() -> TestResult {
        for s in ["1", "2", "10", "1.2", "3.1.2", "99.1.4"] {
            let pos: StepPosition = s.parse()?;
            assert_eq!(pos.to_string(), s);
        }
        Ok(())
    }
}
