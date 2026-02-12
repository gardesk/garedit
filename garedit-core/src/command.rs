/// High-level editing commands applied to a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditCommand {
    InsertChar(char),
    InsertText(String),
    Newline,
    Backspace,
    Delete,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteLine,
    MoveLeft,
    MoveRight,
    MoveWordLeft,
    MoveWordRight,
    MoveUp,
    MoveDown,
    MovePageUp(usize),
    MovePageDown(usize),
    MoveLineStart,
    MoveLineEnd,
    Undo,
    Redo,
}
