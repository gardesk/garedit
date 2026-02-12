use serde::{Deserialize, Serialize};

/// Zero-based text position in document coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }

    pub const fn origin() -> Self {
        Self::new(0, 0)
    }
}

impl Default for Position {
    fn default() -> Self {
        Self::origin()
    }
}
