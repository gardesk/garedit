use crate::{EditCommand, Position, Selection};

/// Mutable text document with cursor/selection state.
#[derive(Debug, Clone)]
pub struct Document {
    lines: Vec<String>,
    cursor: Position,
    selection: Option<Selection>,
    dirty: bool,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: Position::default(),
            selection: None,
            dirty: false,
        }
    }

    pub fn from_text(text: &str) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(ToString::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            cursor: Position::default(),
            selection: None,
            dirty: false,
        }
    }

    pub fn to_text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, line: usize) -> Option<&str> {
        self.lines.get(line).map(String::as_str)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> Position {
        self.cursor
    }

    pub fn set_cursor(&mut self, position: Position) {
        self.cursor = self.clamp_position(position);
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    pub fn set_selection(&mut self, selection: Option<Selection>) {
        self.selection = selection.map(|sel| {
            Selection::new(
                self.clamp_position(sel.anchor),
                self.clamp_position(sel.active),
            )
        });
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn apply(&mut self, command: EditCommand) {
        match command {
            EditCommand::InsertChar(ch) => self.insert_char(ch),
            EditCommand::InsertText(text) => {
                for ch in text.chars() {
                    if ch == '\n' {
                        self.insert_newline();
                    } else {
                        self.insert_char(ch);
                    }
                }
            }
            EditCommand::Newline => self.insert_newline(),
            EditCommand::Backspace => self.backspace(),
            EditCommand::Delete => self.delete(),
            EditCommand::MoveLeft => self.move_left(),
            EditCommand::MoveRight => self.move_right(),
            EditCommand::MoveUp => self.move_up(),
            EditCommand::MoveDown => self.move_down(),
            EditCommand::MoveLineStart => self.cursor.column = 0,
            EditCommand::MoveLineEnd => {
                self.cursor.column = line_char_count(&self.lines[self.cursor.line]);
            }
        }
    }

    fn clamp_position(&self, position: Position) -> Position {
        let max_line = self.lines.len().saturating_sub(1);
        let line = position.line.min(max_line);
        let max_column = line_char_count(&self.lines[line]);
        Position::new(line, position.column.min(max_column))
    }

    fn insert_char(&mut self, ch: char) {
        self.delete_selection_if_needed();
        let line = &mut self.lines[self.cursor.line];
        let idx = column_to_byte_idx(line, self.cursor.column);
        line.insert(idx, ch);
        self.cursor.column += 1;
        self.dirty = true;
    }

    fn insert_newline(&mut self) {
        self.delete_selection_if_needed();
        let line_idx = self.cursor.line;
        let column = self.cursor.column;

        let split_idx = column_to_byte_idx(&self.lines[line_idx], column);
        let tail = self.lines[line_idx].split_off(split_idx);
        self.lines.insert(line_idx + 1, tail);

        self.cursor.line += 1;
        self.cursor.column = 0;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.delete_selection_if_needed() {
            return;
        }

        if self.cursor.column > 0 {
            let line = &mut self.lines[self.cursor.line];
            let start = column_to_byte_idx(line, self.cursor.column - 1);
            let end = column_to_byte_idx(line, self.cursor.column);
            line.replace_range(start..end, "");
            self.cursor.column -= 1;
            self.dirty = true;
            return;
        }

        if self.cursor.line > 0 {
            let current = self.lines.remove(self.cursor.line);
            self.cursor.line -= 1;
            let prev_len = line_char_count(&self.lines[self.cursor.line]);
            self.lines[self.cursor.line].push_str(&current);
            self.cursor.column = prev_len;
            self.dirty = true;
        }
    }

    fn delete(&mut self) {
        if self.delete_selection_if_needed() {
            return;
        }

        let line_idx = self.cursor.line;
        let column = self.cursor.column;
        let line_len = line_char_count(&self.lines[line_idx]);

        if column < line_len {
            let line = &mut self.lines[line_idx];
            let start = column_to_byte_idx(line, column);
            let end = column_to_byte_idx(line, column + 1);
            line.replace_range(start..end, "");
            self.dirty = true;
            return;
        }

        if line_idx + 1 < self.lines.len() {
            let next = self.lines.remove(line_idx + 1);
            self.lines[line_idx].push_str(&next);
            self.dirty = true;
        }
    }

    fn move_left(&mut self) {
        if self.cursor.column > 0 {
            self.cursor.column -= 1;
            return;
        }
        if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.column = line_char_count(&self.lines[self.cursor.line]);
        }
    }

    fn move_right(&mut self) {
        let line_len = line_char_count(&self.lines[self.cursor.line]);
        if self.cursor.column < line_len {
            self.cursor.column += 1;
            return;
        }
        if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.column = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor.line == 0 {
            return;
        }
        self.cursor.line -= 1;
        let max_column = line_char_count(&self.lines[self.cursor.line]);
        self.cursor.column = self.cursor.column.min(max_column);
    }

    fn move_down(&mut self) {
        if self.cursor.line + 1 >= self.lines.len() {
            return;
        }
        self.cursor.line += 1;
        let max_column = line_char_count(&self.lines[self.cursor.line]);
        self.cursor.column = self.cursor.column.min(max_column);
    }

    fn delete_selection_if_needed(&mut self) -> bool {
        let Some(selection) = self.selection.take() else {
            return false;
        };
        let (start, end) = selection.normalized();
        if start == end {
            self.cursor = start;
            return false;
        }

        if start.line == end.line {
            let line = &mut self.lines[start.line];
            let from = column_to_byte_idx(line, start.column);
            let to = column_to_byte_idx(line, end.column);
            line.replace_range(from..to, "");
            self.cursor = start;
            self.dirty = true;
            return true;
        }

        let start_prefix = {
            let line = &self.lines[start.line];
            let end = column_to_byte_idx(line, start.column);
            line[..end].to_string()
        };
        let end_suffix = {
            let line = &self.lines[end.line];
            let begin = column_to_byte_idx(line, end.column);
            line[begin..].to_string()
        };

        self.lines.splice(
            start.line..=end.line,
            [format!("{start_prefix}{end_suffix}")],
        );
        self.cursor = start;
        self.dirty = true;
        true
    }
}

fn line_char_count(line: &str) -> usize {
    line.chars().count()
}

fn column_to_byte_idx(line: &str, column: usize) -> usize {
    if column == 0 {
        return 0;
    }
    line.char_indices()
        .nth(column)
        .map(|(idx, _)| idx)
        .unwrap_or(line.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_text_and_newline() {
        let mut doc = Document::new();
        doc.apply(EditCommand::InsertText("hello".to_string()));
        doc.apply(EditCommand::Newline);
        doc.apply(EditCommand::InsertText("world".to_string()));
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line(0), Some("hello"));
        assert_eq!(doc.line(1), Some("world"));
    }

    #[test]
    fn backspace_merges_lines() {
        let mut doc = Document::from_text("ab\ncd");
        doc.set_cursor(Position::new(1, 0));
        doc.apply(EditCommand::Backspace);
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.line(0), Some("abcd"));
        assert_eq!(doc.cursor(), Position::new(0, 2));
    }

    #[test]
    fn delete_merges_next_line() {
        let mut doc = Document::from_text("ab\ncd");
        doc.set_cursor(Position::new(0, 2));
        doc.apply(EditCommand::Delete);
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.line(0), Some("abcd"));
    }

    #[test]
    fn deletes_multiline_selection() {
        let mut doc = Document::from_text("alpha\nbravo\ncharlie");
        doc.set_selection(Some(Selection::new(
            Position::new(0, 2),
            Position::new(2, 3),
        )));
        doc.apply(EditCommand::InsertChar('X'));
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.line(0), Some("alXrlie"));
    }
}
