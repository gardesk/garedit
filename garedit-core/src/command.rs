/// High-level editing commands applied to a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditCommand {
    InsertChar(char),
    InsertText(String),
    Newline,
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveLineStart,
    MoveLineEnd,
}
