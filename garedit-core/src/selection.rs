use crate::Position;
use serde::{Deserialize, Serialize};

/// Linear text selection represented by anchor and active endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub anchor: Position,
    pub active: Position,
}

impl Selection {
    pub const fn new(anchor: Position, active: Position) -> Self {
        Self { anchor, active }
    }

    pub fn is_collapsed(self) -> bool {
        self.anchor == self.active
    }

    pub fn normalized(self) -> (Position, Position) {
        if self.anchor.line < self.active.line
            || (self.anchor.line == self.active.line && self.anchor.column <= self.active.column)
        {
            (self.anchor, self.active)
        } else {
            (self.active, self.anchor)
        }
    }
}
