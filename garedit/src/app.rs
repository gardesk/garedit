use anyhow::Result;
use garedit_core::{Document, EditCommand, Position, Selection};
use gartk_core::{InputEvent, Key, KeyEvent, MouseButton, Rect, Theme};
use gartk_render::{Renderer, Surface, TextStyle};
use gartk_x11::{Connection, EventLoop, EventLoopConfig, Window, WindowConfig};
use std::path::{Path, PathBuf};
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

const GUTTER_WIDTH: i32 = 64;
const STATUS_BAR_HEIGHT: i32 = 28;

#[derive(Debug, Clone, Copy)]
enum PendingAction {
    OpenPathPrompt,
    Quit,
}

#[derive(Debug, Clone, Copy)]
enum PromptKind {
    OpenPath,
    SaveAsPath,
    ConfirmDiscard { next: PendingAction },
}

#[derive(Debug, Clone)]
struct PromptState {
    kind: PromptKind,
    input: String,
}

impl PromptState {
    fn open_path() -> Self {
        Self {
            kind: PromptKind::OpenPath,
            input: String::new(),
        }
    }

    fn save_as_path(seed: Option<&Path>) -> Self {
        Self {
            kind: PromptKind::SaveAsPath,
            input: seed
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    fn confirm_discard(next: PendingAction) -> Self {
        Self {
            kind: PromptKind::ConfirmDiscard { next },
            input: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub width: u32,
    pub height: u32,
    pub font_family: String,
    pub font_size: f64,
    pub tab_width: usize,
    pub show_line_numbers: bool,
    pub file: Option<PathBuf>,
}

pub struct App {
    window: Window,
    renderer: Renderer,
    gc: u32,
    theme: Theme,
    document: Document,
    status_message: Option<String>,
    tab_width: usize,
    show_line_numbers: bool,
    viewport_top_line: usize,
    pointer_drag_anchor: Option<Position>,
    prompt: Option<PromptState>,
    should_quit: bool,
}

impl App {
    pub fn new(config: AppConfig) -> Result<Self> {
        let conn = Connection::connect(None)?;
        let monitor = gartk_x11::monitor_at_pointer(&conn)?;

        let width = config.width.min(monitor.rect.width);
        let height = config.height.min(monitor.rect.height);
        let x = monitor.rect.x + (monitor.rect.width as i32 - width as i32) / 2;
        let y = monitor.rect.y + (monitor.rect.height as i32 - height as i32) / 2;

        let window = Window::create(
            conn.clone(),
            WindowConfig::default()
                .title("garedit")
                .class("garedit")
                .position(x, y)
                .size(width, height)
                .transparent(false),
        )?;
        window.focus()?;

        let theme = Theme::builder()
            .font_family(config.font_family)
            .font_size(config.font_size)
            .build();
        let renderer = Renderer::with_theme(width, height, theme.clone())?;

        let gc = conn.generate_id()?;
        conn.inner()
            .create_gc(gc, window.id(), &Default::default())?;
        conn.flush()?;

        let (document, status_message) = if let Some(path) = config.file {
            let exists = path.exists();
            match open_or_create_document(&path) {
                Ok(document) => {
                    let message = if exists {
                        format!("opened {}", path.display())
                    } else {
                        format!("new file {}", path.display())
                    };
                    (document, Some(message))
                }
                Err(err) => (Document::new(), Some(format!("open failed: {err}"))),
            }
        } else {
            (Document::new(), Some("scratch buffer".to_string()))
        };

        let mut app = Self {
            window,
            renderer,
            gc,
            theme,
            document,
            status_message,
            tab_width: config.tab_width.max(1),
            show_line_numbers: config.show_line_numbers,
            viewport_top_line: 0,
            pointer_drag_anchor: None,
            prompt: None,
            should_quit: false,
        };
        app.clamp_viewport();
        Ok(app)
    }

    pub fn run(&mut self) -> Result<()> {
        let mut event_loop = EventLoop::new(&self.window, EventLoopConfig::default())?;
        self.render()?;

        event_loop.run(|ev, event| {
            self.handle_event(ev, event);

            if ev.needs_redraw() {
                if let Err(err) = self.render() {
                    tracing::error!("render error: {err}");
                    self.should_quit = true;
                }
                ev.redraw_done();
            }

            Ok(!self.should_quit)
        })?;

        Ok(())
    }

    fn handle_event(&mut self, ev: &mut EventLoop, event: InputEvent) {
        match event {
            InputEvent::Key(key_event) if key_event.pressed => {
                self.handle_key_event(key_event);
                ev.request_redraw();
            }
            InputEvent::MousePress(mouse_event) => {
                if mouse_event.button == Some(MouseButton::Left) {
                    self.handle_mouse_press(
                        mouse_event.position.x,
                        mouse_event.position.y,
                        mouse_event.modifiers.shift,
                    );
                    ev.request_redraw();
                }
            }
            InputEvent::MouseMove(mouse_event) => {
                if self.pointer_drag_anchor.is_some() {
                    self.handle_mouse_drag(mouse_event.position.x, mouse_event.position.y);
                    ev.request_redraw();
                }
            }
            InputEvent::MouseRelease(mouse_event) => {
                if mouse_event.button == Some(MouseButton::Left) {
                    self.pointer_drag_anchor = None;
                    ev.request_redraw();
                }
            }
            InputEvent::Scroll(scroll_event) => {
                let step = (self.visible_line_capacity() / 6).max(1);
                if scroll_event.delta_y < 0 {
                    self.viewport_top_line = self.viewport_top_line.saturating_sub(step);
                } else if scroll_event.delta_y > 0 {
                    self.viewport_top_line += step;
                }
                self.clamp_viewport();
                ev.request_redraw();
            }
            InputEvent::Resize { width, height } => {
                if let Err(err) = self.renderer.resize(width, height) {
                    tracing::error!("resize failed: {err}");
                    self.should_quit = true;
                }
                self.clamp_viewport();
                ev.request_redraw();
            }
            InputEvent::Expose => ev.request_redraw(),
            InputEvent::CloseRequested => {
                self.request_quit();
                ev.request_redraw();
            }
            _ => {}
        }
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if self.prompt.is_some() {
            self.handle_prompt_key_event(key_event);
            return;
        }

        if self.handle_ctrl_shortcuts(&key_event) || self.handle_alt_shortcuts(&key_event) {
            self.ensure_cursor_visible();
            return;
        }

        match key_event.key {
            Key::Escape => {
                if self.document.selection().is_some() {
                    self.document.clear_selection();
                } else {
                    self.request_quit();
                }
            }
            Key::Return => self.document.apply(EditCommand::Newline),
            Key::Backspace => self.document.apply(EditCommand::Backspace),
            Key::Delete => self.document.apply(EditCommand::Delete),
            Key::Left => self.move_cursor(EditCommand::MoveLeft, key_event.modifiers.shift),
            Key::Right => self.move_cursor(EditCommand::MoveRight, key_event.modifiers.shift),
            Key::Up => self.move_cursor(EditCommand::MoveUp, key_event.modifiers.shift),
            Key::Down => self.move_cursor(EditCommand::MoveDown, key_event.modifiers.shift),
            Key::Home => self.move_cursor(EditCommand::MoveLineStart, key_event.modifiers.shift),
            Key::End => self.move_cursor(EditCommand::MoveLineEnd, key_event.modifiers.shift),
            Key::PageUp => {
                let step = self.visible_line_capacity().saturating_sub(1).max(1);
                self.move_cursor(EditCommand::MovePageUp(step), key_event.modifiers.shift);
            }
            Key::PageDown => {
                let step = self.visible_line_capacity().saturating_sub(1).max(1);
                self.move_cursor(EditCommand::MovePageDown(step), key_event.modifiers.shift);
            }
            Key::Tab => self
                .document
                .apply(EditCommand::InsertText(" ".repeat(self.tab_width))),
            Key::Space => self.document.apply(EditCommand::InsertChar(' ')),
            Key::Char(c) => {
                if !(key_event.modifiers.ctrl
                    || key_event.modifiers.alt
                    || key_event.modifiers.super_key)
                {
                    self.document.apply(EditCommand::InsertChar(c));
                }
            }
            _ => {}
        }

        self.ensure_cursor_visible();
    }

    fn handle_ctrl_shortcuts(&mut self, key_event: &KeyEvent) -> bool {
        if !key_event.modifiers.ctrl || key_event.modifiers.alt || key_event.modifiers.super_key {
            return false;
        }

        match key_event.key {
            Key::Left => self.move_cursor(EditCommand::MoveWordLeft, key_event.modifiers.shift),
            Key::Right => self.move_cursor(EditCommand::MoveWordRight, key_event.modifiers.shift),
            Key::Backspace => self.document.apply(EditCommand::DeleteWordBackward),
            Key::Delete => self.document.apply(EditCommand::DeleteWordForward),
            Key::Char(c) => {
                let lower = c.to_ascii_lowercase();
                match lower {
                    'q' => self.request_quit(),
                    'o' => self.begin_open_file_flow(),
                    's' => {
                        if key_event.modifiers.shift {
                            self.begin_save_as_prompt();
                        } else {
                            self.save_current_document();
                        }
                    }
                    'a' => {
                        if key_event.modifiers.shift {
                            self.move_cursor(EditCommand::MoveLineStart, false);
                        } else {
                            self.select_all();
                        }
                    }
                    'e' => self.move_cursor(EditCommand::MoveLineEnd, key_event.modifiers.shift),
                    'b' => self.move_cursor(EditCommand::MoveLeft, key_event.modifiers.shift),
                    'f' => self.move_cursor(EditCommand::MoveRight, key_event.modifiers.shift),
                    'p' => self.move_cursor(EditCommand::MoveUp, key_event.modifiers.shift),
                    'n' => self.move_cursor(EditCommand::MoveDown, key_event.modifiers.shift),
                    'h' => self.document.apply(EditCommand::Backspace),
                    'd' => self.document.apply(EditCommand::Delete),
                    'w' => self.document.apply(EditCommand::DeleteWordBackward),
                    'u' => self.delete_to_line_start(),
                    'k' => self.delete_to_line_end(),
                    'z' => {
                        if key_event.modifiers.shift {
                            self.document.apply(EditCommand::Redo);
                        } else {
                            self.document.apply(EditCommand::Undo);
                        }
                    }
                    'y' => self.document.apply(EditCommand::Redo),
                    _ => return false,
                }
            }
            _ => return false,
        }

        true
    }

    fn handle_alt_shortcuts(&mut self, key_event: &KeyEvent) -> bool {
        if !key_event.modifiers.alt || key_event.modifiers.ctrl || key_event.modifiers.super_key {
            return false;
        }

        match key_event.key {
            Key::Left => self.move_cursor(EditCommand::MoveWordLeft, key_event.modifiers.shift),
            Key::Right => self.move_cursor(EditCommand::MoveWordRight, key_event.modifiers.shift),
            Key::Backspace => self.document.apply(EditCommand::DeleteWordBackward),
            Key::Delete => self.document.apply(EditCommand::DeleteWordForward),
            Key::Char(c) => {
                let lower = c.to_ascii_lowercase();
                match lower {
                    'b' => self.move_cursor(EditCommand::MoveWordLeft, key_event.modifiers.shift),
                    'f' => self.move_cursor(EditCommand::MoveWordRight, key_event.modifiers.shift),
                    'd' => self.document.apply(EditCommand::DeleteWordForward),
                    _ => return false,
                }
            }
            _ => return false,
        }

        true
    }

    fn handle_prompt_key_event(&mut self, key_event: KeyEvent) {
        let kind = match self.prompt.as_ref() {
            Some(prompt) => prompt.kind,
            None => return,
        };

        if let PromptKind::ConfirmDiscard { next } = kind {
            match key_event.key {
                Key::Escape | Key::Char('n') | Key::Char('N') => {
                    self.prompt = None;
                    self.status_message = Some("cancelled".to_string());
                }
                Key::Return | Key::Char('y') | Key::Char('Y') => {
                    self.prompt = None;
                    match next {
                        PendingAction::OpenPathPrompt => {
                            self.prompt = Some(PromptState::open_path())
                        }
                        PendingAction::Quit => self.should_quit = true,
                    }
                }
                _ => {}
            }
            return;
        }

        let mut submit: Option<(PromptKind, String)> = None;
        let mut cancel = false;

        if let Some(prompt) = self.prompt.as_mut() {
            match key_event.key {
                Key::Escape => cancel = true,
                Key::Return => {
                    submit = Some((prompt.kind, prompt.input.trim().to_string()));
                }
                Key::Backspace => {
                    prompt.input.pop();
                }
                Key::Space => prompt.input.push(' '),
                Key::Char(c) => {
                    if key_event.modifiers.ctrl {
                        let lower = c.to_ascii_lowercase();
                        if lower == 'u' {
                            prompt.input.clear();
                        }
                    } else if !(key_event.modifiers.alt || key_event.modifiers.super_key) {
                        prompt.input.push(c);
                    }
                }
                _ => {}
            }
        }

        if cancel {
            self.prompt = None;
            self.status_message = Some("prompt cancelled".to_string());
            return;
        }

        let Some((kind, raw_input)) = submit else {
            return;
        };
        self.prompt = None;

        if raw_input.is_empty() {
            self.status_message = Some("path is empty".to_string());
            return;
        }

        let path = resolve_user_path(&raw_input);
        match kind {
            PromptKind::OpenPath => self.open_path(path),
            PromptKind::SaveAsPath => self.save_as_path(path),
            PromptKind::ConfirmDiscard { .. } => {}
        }
    }

    fn begin_open_file_flow(&mut self) {
        if self.document.is_dirty() {
            self.prompt = Some(PromptState::confirm_discard(PendingAction::OpenPathPrompt));
            return;
        }
        self.prompt = Some(PromptState::open_path());
    }

    fn begin_save_as_prompt(&mut self) {
        self.prompt = Some(PromptState::save_as_path(self.document.path()));
    }

    fn request_quit(&mut self) {
        if self.document.is_dirty() {
            self.prompt = Some(PromptState::confirm_discard(PendingAction::Quit));
            return;
        }
        self.should_quit = true;
    }

    fn open_path(&mut self, path: PathBuf) {
        let existed = path.exists();
        match open_or_create_document(&path) {
            Ok(document) => {
                self.document = document;
                self.viewport_top_line = 0;
                self.pointer_drag_anchor = None;
                self.status_message = Some(if existed {
                    format!("opened {}", path.display())
                } else {
                    format!("new file {}", path.display())
                });
            }
            Err(err) => {
                self.status_message = Some(format!("open failed: {err}"));
            }
        }
    }

    fn save_current_document(&mut self) {
        if self.document.path().is_some() {
            match self.document.save() {
                Ok(()) => {
                    if let Some(path) = self.document.path() {
                        self.status_message = Some(format!("saved {}", path.display()));
                    } else {
                        self.status_message = Some("saved".to_string());
                    }
                }
                Err(err) => {
                    self.status_message = Some(format!("save failed: {err}"));
                }
            }
            return;
        }

        self.begin_save_as_prompt();
    }

    fn save_as_path(&mut self, path: PathBuf) {
        match self.document.save_as(&path) {
            Ok(()) => self.status_message = Some(format!("saved {}", path.display())),
            Err(err) => self.status_message = Some(format!("save failed: {err}")),
        }
    }

    fn select_all(&mut self) {
        let last_line = self.document.line_count().saturating_sub(1);
        let last_column = self
            .document
            .line(last_line)
            .map(|line| line.chars().count())
            .unwrap_or(0);
        let start = Position::origin();
        let end = Position::new(last_line, last_column);

        self.document.set_cursor(end);
        if start == end {
            self.document.clear_selection();
        } else {
            self.document
                .set_selection(Some(Selection::new(start, end)));
        }
    }

    fn move_cursor(&mut self, command: EditCommand, extend_selection: bool) {
        if extend_selection {
            let anchor = self
                .document
                .selection()
                .map(|selection| selection.anchor)
                .unwrap_or(self.document.cursor());
            self.document.apply(command);
            let active = self.document.cursor();
            if active == anchor {
                self.document.clear_selection();
            } else {
                self.document
                    .set_selection(Some(Selection::new(anchor, active)));
            }
            return;
        }

        if let Some(selection) = self.document.selection() {
            let (start, end) = selection.normalized();
            match command {
                EditCommand::MoveLeft
                | EditCommand::MoveWordLeft
                | EditCommand::MoveUp
                | EditCommand::MovePageUp(_) => {
                    self.document.clear_selection();
                    self.document.set_cursor(start);
                    return;
                }
                EditCommand::MoveRight
                | EditCommand::MoveWordRight
                | EditCommand::MoveDown
                | EditCommand::MovePageDown(_) => {
                    self.document.clear_selection();
                    self.document.set_cursor(end);
                    return;
                }
                _ => {
                    self.document.clear_selection();
                }
            }
        }

        self.document.apply(command);
        self.document.clear_selection();
    }

    fn delete_to_line_start(&mut self) {
        let cursor = self.document.cursor();
        if cursor.column > 0 {
            self.document
                .set_selection(Some(Selection::new(Position::new(cursor.line, 0), cursor)));
            self.document.apply(EditCommand::Delete);
            return;
        }

        if cursor.line > 0 {
            self.document.apply(EditCommand::Backspace);
        }
    }

    fn delete_to_line_end(&mut self) {
        let cursor = self.document.cursor();
        let line_len = self
            .document
            .line(cursor.line)
            .map(|line| line.chars().count())
            .unwrap_or(cursor.column);

        if cursor.column < line_len {
            self.document.set_selection(Some(Selection::new(
                cursor,
                Position::new(cursor.line, line_len),
            )));
            self.document.apply(EditCommand::Delete);
            return;
        }

        self.document.apply(EditCommand::Delete);
    }

    fn handle_mouse_press(&mut self, pointer_x: i32, pointer_y: i32, extend_selection: bool) {
        let Some(position) = self.cursor_from_pointer(pointer_x, pointer_y) else {
            return;
        };

        if extend_selection {
            let anchor = self
                .document
                .selection()
                .map(|selection| selection.anchor)
                .unwrap_or(self.document.cursor());
            self.document.set_cursor(position);
            if anchor == position {
                self.document.clear_selection();
            } else {
                self.document
                    .set_selection(Some(Selection::new(anchor, position)));
            }
            self.pointer_drag_anchor = Some(anchor);
        } else {
            self.document.set_cursor(position);
            self.document.clear_selection();
            self.pointer_drag_anchor = Some(position);
        }
        self.ensure_cursor_visible();
    }

    fn handle_mouse_drag(&mut self, pointer_x: i32, pointer_y: i32) {
        let Some(anchor) = self.pointer_drag_anchor else {
            return;
        };
        let Some(position) = self.cursor_from_pointer(pointer_x, pointer_y) else {
            return;
        };

        self.document.set_cursor(position);
        if anchor == position {
            self.document.clear_selection();
        } else {
            self.document
                .set_selection(Some(Selection::new(anchor, position)));
        }
        self.ensure_cursor_visible();
    }

    fn cursor_from_pointer(&self, pointer_x: i32, pointer_y: i32) -> Option<Position> {
        let content_top = self.content_top();
        let content_bottom = self.content_bottom();
        if pointer_y < content_top || pointer_y >= content_bottom {
            return None;
        }

        let line_height = self.line_height().max(1);
        let line_offset = ((pointer_y - content_top) / line_height) as usize;
        let line_index = (self.viewport_top_line + line_offset).min(self.document.line_count() - 1);

        let text_origin_x = self.text_origin_x();
        let local_x = (pointer_x - text_origin_x).max(0);
        let line_text = self.document.line(line_index).unwrap_or("");
        let column = self.column_from_x(line_text, local_x);

        Some(Position::new(line_index, column))
    }

    fn ensure_cursor_visible(&mut self) {
        self.clamp_viewport();
        let line = self.document.cursor().line;
        let visible_lines = self.visible_line_capacity();

        if line < self.viewport_top_line {
            self.viewport_top_line = line;
        } else {
            let viewport_bottom = self.viewport_top_line + visible_lines.saturating_sub(1);
            if line > viewport_bottom {
                self.viewport_top_line = line.saturating_sub(visible_lines.saturating_sub(1));
            }
        }
        self.clamp_viewport();
    }

    fn visible_line_capacity(&self) -> usize {
        let line_height = self.line_height().max(1);
        let usable_height = self.content_bottom() - self.content_top();
        (usable_height.max(1) / line_height) as usize
    }

    fn line_height(&self) -> i32 {
        let style = self.editor_text_style();
        self.renderer
            .measure_text("Mg", &style)
            .map(|size| size.height as i32 + 4)
            .unwrap_or((self.theme.font_size as i32) + 6)
    }

    fn content_top(&self) -> i32 {
        self.theme.padding as i32
    }

    fn status_bar_top(&self) -> i32 {
        self.renderer.size().height as i32 - STATUS_BAR_HEIGHT
    }

    fn content_bottom(&self) -> i32 {
        let top = self.content_top();
        let bottom = self.status_bar_top() - self.theme.padding as i32;
        bottom.max(top + 1)
    }

    fn clamp_viewport(&mut self) {
        let visible_lines = self.visible_line_capacity().max(1);
        let max_top = self.document.line_count().saturating_sub(visible_lines);
        self.viewport_top_line = self.viewport_top_line.min(max_top);
    }

    fn gutter_width(&self) -> i32 {
        if self.show_line_numbers {
            GUTTER_WIDTH
        } else {
            0
        }
    }

    fn text_origin_x(&self) -> i32 {
        self.gutter_width() + self.theme.padding as i32
    }

    fn column_from_x(&self, line: &str, target_x: i32) -> usize {
        if line.is_empty() || target_x <= 0 {
            return 0;
        }

        let style = self.editor_text_style();
        let mut best_column = 0usize;
        let mut prev_width = 0i32;

        for column in 1..=line.chars().count() {
            let prefix = slice_to_column(line, column);
            let width = self
                .renderer
                .measure_text(prefix, &style)
                .map(|size| size.width as i32)
                .unwrap_or(prev_width);

            if width >= target_x {
                let dist_prev = (target_x - prev_width).abs();
                let dist_next = (width - target_x).abs();
                return if dist_prev <= dist_next {
                    column.saturating_sub(1)
                } else {
                    column
                };
            }

            best_column = column;
            prev_width = width;
        }

        best_column
    }

    fn text_x_for_column(&self, line: &str, column: usize, style: &TextStyle) -> i32 {
        let prefix = slice_to_column(line, column);
        self.renderer
            .measure_text(prefix, style)
            .map(|size| size.width as i32)
            .unwrap_or(0)
    }

    fn render_selection_for_line(
        &mut self,
        line_index: usize,
        y: i32,
        line_height: i32,
        line_text: &str,
        style: &TextStyle,
    ) -> Result<()> {
        let Some(selection) = self.document.selection() else {
            return Ok(());
        };
        if selection.is_collapsed() {
            return Ok(());
        }

        let (start, end) = selection.normalized();
        if line_index < start.line || line_index > end.line {
            return Ok(());
        }

        let start_col = if line_index == start.line {
            start.column
        } else {
            0
        };
        let end_col = if line_index == end.line {
            end.column
        } else {
            line_text.chars().count()
        };
        if end_col <= start_col {
            return Ok(());
        }

        let text_origin_x = self.text_origin_x();
        let x_start = text_origin_x + self.text_x_for_column(line_text, start_col, style);
        let x_end = text_origin_x + self.text_x_for_column(line_text, end_col, style);
        let width = (x_end - x_start).max(2) as u32;

        self.renderer.fill_rect(
            Rect::new(x_start, y.saturating_sub(1), width, line_height as u32),
            self.theme.selection_background.with_alpha(0.9),
        )?;
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        self.clamp_viewport();
        let size = self.renderer.size();
        let padding = self.theme.padding as i32;
        let content_top = self.content_top();
        let content_bottom = self.content_bottom();
        let status_top = self.status_bar_top();
        let line_height = self.line_height().max(1);
        let visible_lines = self.visible_line_capacity();
        let cursor = self.document.cursor();
        let gutter_width = self.gutter_width();
        let text_origin_x = self.text_origin_x();

        let editor_style = self.editor_text_style();
        let line_number_style = TextStyle::new()
            .font_family(&self.theme.font_family)
            .font_size((self.theme.font_size - 1.0).max(10.0))
            .color(self.theme.item_description);

        self.renderer.clear()?;

        if gutter_width > 0 {
            self.renderer.fill_rect(
                Rect::new(0, 0, gutter_width as u32, status_top.max(0) as u32),
                self.theme.input_background,
            )?;
            self.renderer.line(
                gutter_width as f64,
                0.0,
                gutter_width as f64,
                status_top as f64,
                self.theme.border,
                1.0,
            )?;
        }

        for row in 0..visible_lines {
            let line_index = self.viewport_top_line + row;
            if line_index >= self.document.line_count() {
                break;
            }

            let y = content_top + row as i32 * line_height;
            if y + line_height > content_bottom {
                break;
            }

            if line_index == cursor.line {
                self.renderer.fill_rect(
                    Rect::new(
                        gutter_width,
                        y.saturating_sub(1),
                        size.width.saturating_sub(gutter_width as u32),
                        line_height as u32,
                    ),
                    self.theme.item_hover_background.with_alpha(0.6),
                )?;
            }

            let text = self.document.line(line_index).unwrap_or("").to_string();
            self.render_selection_for_line(line_index, y, line_height, &text, &editor_style)?;

            if gutter_width > 0 {
                let line_no = (line_index + 1).to_string();
                let line_no_width = self
                    .renderer
                    .measure_text(&line_no, &line_number_style)?
                    .width as i32;
                let line_no_x = gutter_width - padding - line_no_width;
                self.renderer
                    .text(&line_no, line_no_x as f64, y as f64, &line_number_style)?;
            }

            self.renderer
                .text(&text, text_origin_x as f64, y as f64, &editor_style)?;
        }

        if cursor.line >= self.viewport_top_line
            && cursor.line < self.viewport_top_line + visible_lines
        {
            let cursor_row = cursor.line - self.viewport_top_line;
            let y = content_top + cursor_row as i32 * line_height;
            let line = self.document.line(cursor.line).unwrap_or("");
            let prefix_width = self.text_x_for_column(line, cursor.column, &editor_style);
            let caret_x = text_origin_x + prefix_width;

            self.renderer.fill_rect(
                Rect::new(caret_x, y, 2, (line_height - 1).max(1) as u32),
                self.theme.input_cursor,
            )?;
        }

        self.render_status_bar()?;

        self.renderer.flush();
        self.blit_surface()
    }

    fn render_status_bar(&mut self) -> Result<()> {
        let size = self.renderer.size();
        let status_top = self.status_bar_top();
        let padding = self.theme.padding as i32;
        let cursor = self.document.cursor();

        self.renderer.fill_rect(
            Rect::new(0, status_top, size.width, STATUS_BAR_HEIGHT as u32),
            self.theme.input_background.darken(0.18),
        )?;
        self.renderer.line(
            0.0,
            status_top as f64,
            size.width as f64,
            status_top as f64,
            self.theme.border,
            1.0,
        )?;

        let right_text = format!(
            "Ln {}, Col {}  •  {} lines",
            cursor.line + 1,
            cursor.column + 1,
            self.document.line_count()
        );
        let right_style = TextStyle::new()
            .font_family(&self.theme.font_family)
            .font_size((self.theme.font_size - 1.0).max(10.0))
            .color(self.theme.foreground);
        let right_size = self.renderer.measure_text(&right_text, &right_style)?;
        let text_y = status_top + (STATUS_BAR_HEIGHT - right_size.height as i32) / 2;
        let right_x = size.width as i32 - padding - right_size.width as i32;
        self.renderer
            .text(&right_text, right_x as f64, text_y as f64, &right_style)?;

        let left_text = if let Some(prompt) = &self.prompt {
            self.prompt_display_text(prompt)
        } else {
            let file_display = self
                .document
                .path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "[scratch]".to_string());
            let dirty_marker = if self.document.is_dirty() { " [+]" } else { "" };
            let mut text = format!("{file_display}{dirty_marker}");
            if let Some(message) = &self.status_message {
                text.push_str("  |  ");
                text.push_str(message);
            }
            text
        };

        let max_left_width = (right_x - padding * 2).max(40);
        let left_style = TextStyle::new()
            .font_family(&self.theme.font_family)
            .font_size((self.theme.font_size - 1.0).max(10.0))
            .color(self.theme.item_description)
            .ellipsize(true)
            .max_width(max_left_width);
        self.renderer
            .text(&left_text, padding as f64, text_y as f64, &left_style)?;

        Ok(())
    }

    fn prompt_display_text(&self, prompt: &PromptState) -> String {
        match prompt.kind {
            PromptKind::OpenPath => format!("open path: {}_", prompt.input),
            PromptKind::SaveAsPath => format!("save as: {}_", prompt.input),
            PromptKind::ConfirmDiscard {
                next: PendingAction::OpenPathPrompt,
            } => "unsaved changes: discard and open file? [y/N]".to_string(),
            PromptKind::ConfirmDiscard {
                next: PendingAction::Quit,
            } => "unsaved changes: discard and quit? [y/N]".to_string(),
        }
    }

    fn blit_surface(&mut self) -> Result<()> {
        let size = self.renderer.size();
        let conn = self.window.connection();

        let ctx = self.renderer.context()?;
        ctx.target().flush();

        let mut temp_surface = Surface::new(size.width, size.height)?;
        let temp_ctx = temp_surface.context()?;
        temp_ctx.set_source_surface(self.renderer.surface().cairo_surface(), 0.0, 0.0)?;
        temp_ctx.paint()?;
        drop(temp_ctx);

        let data = temp_surface.data()?;
        conn.inner().put_image(
            ImageFormat::Z_PIXMAP,
            self.window.id(),
            self.gc,
            size.width as u16,
            size.height as u16,
            0,
            0,
            0,
            self.window.depth(),
            &data,
        )?;
        conn.flush()?;

        Ok(())
    }

    fn editor_text_style(&self) -> TextStyle {
        TextStyle::new()
            .font_family(&self.theme.font_family)
            .font_size(self.theme.font_size)
            .color(self.theme.foreground)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.window.connection().inner().free_gc(self.gc);
    }
}

fn slice_to_column(line: &str, column: usize) -> &str {
    if column == 0 {
        return "";
    }

    match line.char_indices().nth(column) {
        Some((idx, _)) => &line[..idx],
        None => line,
    }
}

fn open_or_create_document(path: &Path) -> Result<Document> {
    if path.exists() {
        return Ok(Document::open_path(path)?);
    }

    let mut document = Document::new();
    document.set_path(Some(path.to_path_buf()));
    Ok(document)
}

fn resolve_user_path(input: &str) -> PathBuf {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(trimmed)
}
