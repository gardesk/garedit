use crate::{EditCommand, Position, Selection};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const DEFAULT_HISTORY_LIMIT: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewlineStyle {
    Lf,
    Crlf,
}

impl NewlineStyle {
    fn delimiter(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Position,
    selection: Option<Selection>,
    dirty: bool,
    path: Option<PathBuf>,
    newline_style: NewlineStyle,
}

#[derive(Debug, Clone)]
struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    limit: usize,
}

impl History {
    fn new(limit: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: limit.max(1),
        }
    }

    fn push_undo(&mut self, snapshot: Snapshot) {
        self.undo.push(snapshot);
        if self.undo.len() > self.limit {
            let overflow = self.undo.len() - self.limit;
            self.undo.drain(0..overflow);
        }
    }

    fn trim_to_limit(&mut self) {
        if self.undo.len() > self.limit {
            let overflow = self.undo.len() - self.limit;
            self.undo.drain(0..overflow);
        }
        if self.redo.len() > self.limit {
            let overflow = self.redo.len() - self.limit;
            self.redo.drain(0..overflow);
        }
    }
}

/// Mutable text document with cursor/selection state.
#[derive(Debug, Clone)]
pub struct Document {
    lines: Vec<String>,
    cursor: Position,
    selection: Option<Selection>,
    dirty: bool,
    path: Option<PathBuf>,
    newline_style: NewlineStyle,
    history: History,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self::from_normalized_text("", NewlineStyle::Lf, None)
    }

    pub fn from_text(text: &str) -> Self {
        let normalized = normalize_newlines(text);
        Self::from_normalized_text(&normalized, NewlineStyle::Lf, None)
    }

    pub fn open_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)?;
        let newline_style = detect_newline_style(&raw);
        let normalized = normalize_newlines(&raw);
        Ok(Self::from_normalized_text(
            &normalized,
            newline_style,
            Some(path.to_path_buf()),
        ))
    }

    fn from_normalized_text(text: &str, newline_style: NewlineStyle, path: Option<PathBuf>) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(ToString::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            cursor: Position::default(),
            selection: None,
            dirty: false,
            path,
            newline_style,
            history: History::new(DEFAULT_HISTORY_LIMIT),
        }
    }

    pub fn to_text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn serialized_text(&self) -> String {
        self.lines.join(self.newline_style.delimiter())
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

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, path: Option<PathBuf>) {
        self.path = path;
    }

    pub fn newline_style(&self) -> NewlineStyle {
        self.newline_style
    }

    pub fn set_newline_style(&mut self, newline_style: NewlineStyle) {
        if self.newline_style != newline_style {
            self.newline_style = newline_style;
            self.dirty = true;
        }
    }

    pub fn history_limit(&self) -> usize {
        self.history.limit
    }

    pub fn set_history_limit(&mut self, limit: usize) {
        self.history.limit = limit.max(1);
        self.history.trim_to_limit();
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn save(&mut self) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "document has no associated path",
            ));
        };
        self.write_to_path(&path)
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref().to_path_buf();
        self.write_to_path(&path)
    }

    fn write_to_path(&mut self, path: &Path) -> io::Result<()> {
        fs::write(path, self.serialized_text())?;
        self.path = Some(path.to_path_buf());
        self.mark_clean();
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.history.undo.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.history.redo.push(current);
        self.restore_snapshot(previous);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.history.redo.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.history.push_undo(current);
        self.restore_snapshot(next);
        true
    }

    pub fn apply(&mut self, command: EditCommand) {
        match command {
            EditCommand::InsertChar(ch) => self.apply_edit(|doc| doc.insert_char(ch)),
            EditCommand::InsertText(text) => self.apply_edit(|doc| doc.insert_text(&text)),
            EditCommand::Newline => self.apply_edit(|doc| doc.insert_newline()),
            EditCommand::Backspace => self.apply_edit(|doc| doc.backspace()),
            EditCommand::Delete => self.apply_edit(|doc| doc.delete()),
            EditCommand::DeleteWordBackward => self.apply_edit(|doc| doc.delete_word_backward()),
            EditCommand::DeleteWordForward => self.apply_edit(|doc| doc.delete_word_forward()),
            EditCommand::DeleteLine => self.apply_edit(|doc| doc.delete_line()),
            EditCommand::MoveLeft => self.move_left(),
            EditCommand::MoveRight => self.move_right(),
            EditCommand::MoveWordLeft => self.move_word_left(),
            EditCommand::MoveWordRight => self.move_word_right(),
            EditCommand::MoveUp => self.move_up(),
            EditCommand::MoveDown => self.move_down(),
            EditCommand::MovePageUp(lines) => {
                for _ in 0..lines {
                    self.move_up();
                }
            }
            EditCommand::MovePageDown(lines) => {
                for _ in 0..lines {
                    self.move_down();
                }
            }
            EditCommand::MoveLineStart => self.cursor.column = 0,
            EditCommand::MoveLineEnd => {
                self.cursor.column = line_char_count(&self.lines[self.cursor.line]);
            }
            EditCommand::Undo => {
                self.undo();
            }
            EditCommand::Redo => {
                self.redo();
            }
        }
    }

    fn apply_edit<F>(&mut self, mut edit: F)
    where
        F: FnMut(&mut Self),
    {
        let before = self.snapshot();
        edit(self);
        if !self.same_as_snapshot(&before) {
            self.history.push_undo(before);
            self.history.redo.clear();
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            selection: self.selection,
            dirty: self.dirty,
            path: self.path.clone(),
            newline_style: self.newline_style,
        }
    }

    fn restore_snapshot(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.cursor = snapshot.cursor;
        self.selection = snapshot.selection;
        self.dirty = snapshot.dirty;
        self.path = snapshot.path;
        self.newline_style = snapshot.newline_style;
    }

    fn same_as_snapshot(&self, snapshot: &Snapshot) -> bool {
        self.lines == snapshot.lines
            && self.cursor == snapshot.cursor
            && self.selection == snapshot.selection
            && self.dirty == snapshot.dirty
            && self.path == snapshot.path
            && self.newline_style == snapshot.newline_style
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

    fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.insert_newline();
            } else {
                self.insert_char(ch);
            }
        }
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

    fn move_word_left(&mut self) {
        self.cursor = self.previous_word_boundary(self.cursor);
    }

    fn move_word_right(&mut self) {
        self.cursor = self.next_word_boundary(self.cursor);
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

    fn delete_word_backward(&mut self) {
        if self.delete_selection_if_needed() {
            return;
        }

        let start = self.previous_word_boundary(self.cursor);
        if start == self.cursor {
            return;
        }
        self.selection = Some(Selection::new(start, self.cursor));
        self.delete_selection_if_needed();
    }

    fn delete_word_forward(&mut self) {
        if self.delete_selection_if_needed() {
            return;
        }

        let end = self.next_word_boundary(self.cursor);
        if end == self.cursor {
            return;
        }
        self.selection = Some(Selection::new(self.cursor, end));
        self.delete_selection_if_needed();
    }

    fn delete_line(&mut self) {
        if self.delete_selection_if_needed() {
            return;
        }

        if self.lines.len() == 1 {
            if !self.lines[0].is_empty() {
                self.lines[0].clear();
                self.cursor = Position::origin();
                self.dirty = true;
            }
            return;
        }

        self.lines.remove(self.cursor.line);
        if self.cursor.line >= self.lines.len() {
            self.cursor.line = self.lines.len() - 1;
        }
        self.cursor.column = 0;
        self.dirty = true;
    }

    fn previous_word_boundary(&self, from: Position) -> Position {
        if from.line == 0 && from.column == 0 {
            return from;
        }

        let mut line = from.line;
        let mut column = from.column;

        loop {
            if column > 0 {
                let new_column = prev_word_boundary_in_line(&self.lines[line], column);
                return Position::new(line, new_column);
            }
            if line == 0 {
                return Position::origin();
            }
            line -= 1;
            column = line_char_count(&self.lines[line]);
        }
    }

    fn next_word_boundary(&self, from: Position) -> Position {
        let mut line = from.line;
        let mut column = from.column;

        loop {
            let line_len = line_char_count(&self.lines[line]);
            if column < line_len {
                let new_column = next_word_boundary_in_line(&self.lines[line], column);
                return Position::new(line, new_column);
            }

            if line + 1 >= self.lines.len() {
                return Position::new(line, line_len);
            }

            line += 1;
            column = 0;
            let next_line_len = line_char_count(&self.lines[line]);
            if next_line_len == 0 {
                if line + 1 >= self.lines.len() {
                    return Position::new(line, 0);
                }
                continue;
            }
            let new_column = next_word_boundary_in_line(&self.lines[line], 0);
            return Position::new(line, new_column);
        }
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

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn prev_word_boundary_in_line(line: &str, column: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut i = column.min(chars.len());

    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }

    if is_word_char(chars[i - 1]) {
        while i > 0 && is_word_char(chars[i - 1]) {
            i -= 1;
        }
        return i;
    }

    while i > 0 && !chars[i - 1].is_whitespace() && !is_word_char(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

fn next_word_boundary_in_line(line: &str, column: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut i = column.min(chars.len());

    if i >= chars.len() {
        return chars.len();
    }

    if chars[i].is_whitespace() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        return i;
    }

    if is_word_char(chars[i]) {
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
    } else {
        while i < chars.len() && !chars[i].is_whitespace() && !is_word_char(chars[i]) {
            i += 1;
        }
    }
    while i < chars.len() && !is_word_char(chars[i]) {
        i += 1;
    }
    i
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

fn detect_newline_style(raw: &str) -> NewlineStyle {
    if raw.contains("\r\n") {
        NewlineStyle::Crlf
    } else {
        NewlineStyle::Lf
    }
}

fn normalize_newlines(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn undo_redo_round_trip() {
        let mut doc = Document::new();
        doc.mark_clean();
        doc.apply(EditCommand::InsertText("hello".to_string()));
        assert!(doc.is_dirty());
        doc.apply(EditCommand::Undo);
        assert_eq!(doc.to_text(), "");
        assert!(!doc.is_dirty());
        doc.apply(EditCommand::Redo);
        assert_eq!(doc.to_text(), "hello");
        assert!(doc.is_dirty());
    }

    #[test]
    fn moves_by_words_over_punctuation() {
        let mut doc = Document::from_text("foo::bar baz");
        doc.apply(EditCommand::MoveWordRight);
        assert_eq!(doc.cursor(), Position::new(0, 5));
        doc.apply(EditCommand::MoveWordRight);
        assert_eq!(doc.cursor(), Position::new(0, 9));
        doc.apply(EditCommand::MoveWordLeft);
        assert_eq!(doc.cursor(), Position::new(0, 5));
        doc.apply(EditCommand::MoveWordLeft);
        assert_eq!(doc.cursor(), Position::new(0, 0));
    }

    #[test]
    fn deletes_words() {
        let mut doc = Document::from_text("alpha beta  gamma");
        doc.set_cursor(Position::new(0, 17));
        doc.apply(EditCommand::DeleteWordBackward);
        assert_eq!(doc.to_text(), "alpha beta  ");
        doc.apply(EditCommand::DeleteWordBackward);
        assert_eq!(doc.to_text(), "alpha ");

        let mut doc = Document::from_text("alpha beta");
        doc.set_cursor(Position::new(0, 0));
        doc.apply(EditCommand::DeleteWordForward);
        assert_eq!(doc.to_text(), "beta");
    }

    #[test]
    fn deletes_current_line() {
        let mut doc = Document::from_text("one\ntwo\nthree");
        doc.set_cursor(Position::new(1, 2));
        doc.apply(EditCommand::DeleteLine);
        assert_eq!(doc.to_text(), "one\nthree");
        assert_eq!(doc.cursor(), Position::new(1, 0));
    }

    #[test]
    fn saves_and_opens_with_newline_normalization() -> io::Result<()> {
        let path = temp_test_path("newline-style");

        let mut doc = Document::from_text("alpha\nbeta");
        doc.set_newline_style(NewlineStyle::Crlf);
        doc.save_as(&path)?;

        let raw = fs::read_to_string(&path)?;
        assert_eq!(raw, "alpha\r\nbeta");

        let reopened = Document::open_path(&path)?;
        assert_eq!(reopened.to_text(), "alpha\nbeta");
        assert_eq!(reopened.newline_style(), NewlineStyle::Crlf);
        assert_eq!(reopened.path(), Some(path.as_path()));
        assert!(!reopened.is_dirty());

        let _ = fs::remove_file(path);
        Ok(())
    }

    fn temp_test_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        std::env::temp_dir().join(format!("garedit-core-{prefix}-{nanos}.txt"))
    }
}
