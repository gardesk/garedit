use anyhow::{Context, Result};
use garedit_core::{Document, EditCommand, Position};
use gartk_core::{InputEvent, Key, KeyEvent, MouseButton, Rect, Theme};
use gartk_render::{Renderer, Surface, TextStyle};
use gartk_x11::{Connection, EventLoop, EventLoopConfig, Window, WindowConfig};
use std::path::PathBuf;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

const GUTTER_WIDTH: i32 = 64;
const STATUS_BAR_HEIGHT: i32 = 28;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub width: u32,
    pub height: u32,
    pub font_family: String,
    pub font_size: f64,
    pub file: Option<PathBuf>,
}

pub struct App {
    window: Window,
    renderer: Renderer,
    gc: u32,
    theme: Theme,
    document: Document,
    open_path: Option<PathBuf>,
    status_message: Option<String>,
    viewport_top_line: usize,
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

        let (document, open_path, status_message) = if let Some(path) = config.file {
            match load_document_from_path(&path) {
                Ok(document) => {
                    let message = if path.exists() {
                        format!("opened {}", path.display())
                    } else {
                        format!("new file {}", path.display())
                    };
                    (document, Some(path), Some(message))
                }
                Err(err) => (Document::new(), None, Some(format!("open failed: {err}"))),
            }
        } else {
            (Document::new(), None, Some("scratch buffer".to_string()))
        };

        let mut app = Self {
            window,
            renderer,
            gc,
            theme,
            document,
            open_path,
            status_message,
            viewport_top_line: 0,
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
                    self.place_cursor_from_pointer(mouse_event.position.x, mouse_event.position.y);
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
            InputEvent::CloseRequested => self.should_quit = true,
            _ => {}
        }
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if key_event.modifiers.ctrl {
            match key_event.key {
                Key::Char('q') | Key::Char('Q') => {
                    self.should_quit = true;
                    return;
                }
                _ => {}
            }
        }

        match key_event.key {
            Key::Escape => self.should_quit = true,
            Key::Return => self.document.apply(EditCommand::Newline),
            Key::Backspace => self.document.apply(EditCommand::Backspace),
            Key::Delete => self.document.apply(EditCommand::Delete),
            Key::Left => self.document.apply(EditCommand::MoveLeft),
            Key::Right => self.document.apply(EditCommand::MoveRight),
            Key::Up => self.document.apply(EditCommand::MoveUp),
            Key::Down => self.document.apply(EditCommand::MoveDown),
            Key::Home => self.document.apply(EditCommand::MoveLineStart),
            Key::End => self.document.apply(EditCommand::MoveLineEnd),
            Key::PageUp => {
                let step = self.visible_line_capacity().saturating_sub(1).max(1);
                for _ in 0..step {
                    self.document.apply(EditCommand::MoveUp);
                }
            }
            Key::PageDown => {
                let step = self.visible_line_capacity().saturating_sub(1).max(1);
                for _ in 0..step {
                    self.document.apply(EditCommand::MoveDown);
                }
            }
            Key::Tab => self
                .document
                .apply(EditCommand::InsertText("    ".to_string())),
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

    fn place_cursor_from_pointer(&mut self, pointer_x: i32, pointer_y: i32) {
        let content_top = self.content_top();
        let content_bottom = self.content_bottom();
        if pointer_y < content_top || pointer_y >= content_bottom {
            return;
        }

        let line_height = self.line_height().max(1);
        let line_offset = ((pointer_y - content_top) / line_height) as usize;
        let line_index = (self.viewport_top_line + line_offset).min(self.document.line_count() - 1);

        let text_origin_x = GUTTER_WIDTH + self.theme.padding as i32;
        let local_x = (pointer_x - text_origin_x).max(0);
        let line_text = self.document.line(line_index).unwrap_or("");
        let column = self.column_from_x(line_text, local_x);

        self.document.set_cursor(Position::new(line_index, column));
        self.ensure_cursor_visible();
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
                // Snap to whichever side is closer.
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

        let editor_style = self.editor_text_style();
        let line_number_style = TextStyle::new()
            .font_family(&self.theme.font_family)
            .font_size((self.theme.font_size - 1.0).max(10.0))
            .color(self.theme.item_description);

        self.renderer.clear()?;

        // Gutter area.
        self.renderer.fill_rect(
            Rect::new(0, 0, GUTTER_WIDTH as u32, status_top.max(0) as u32),
            self.theme.input_background,
        )?;
        self.renderer.line(
            GUTTER_WIDTH as f64,
            0.0,
            GUTTER_WIDTH as f64,
            status_top as f64,
            self.theme.border,
            1.0,
        )?;

        for row in 0..visible_lines {
            let line_index = self.viewport_top_line + row;
            if line_index >= self.document.line_count() {
                break;
            }

            let y = content_top + row as i32 * line_height;
            if y + line_height > content_bottom {
                break;
            }

            // Current line highlight.
            if line_index == cursor.line {
                self.renderer.fill_rect(
                    Rect::new(
                        GUTTER_WIDTH,
                        y.saturating_sub(1),
                        size.width.saturating_sub(GUTTER_WIDTH as u32),
                        line_height as u32,
                    ),
                    self.theme.item_hover_background.with_alpha(0.6),
                )?;
            }

            let line_no = (line_index + 1).to_string();
            let line_no_width = self
                .renderer
                .measure_text(&line_no, &line_number_style)?
                .width as i32;
            let line_no_x = GUTTER_WIDTH - padding - line_no_width;
            self.renderer
                .text(&line_no, line_no_x as f64, y as f64, &line_number_style)?;

            let text = self.document.line(line_index).unwrap_or("");
            self.renderer.text(
                text,
                (GUTTER_WIDTH + padding) as f64,
                y as f64,
                &editor_style,
            )?;
        }

        // Caret.
        if cursor.line >= self.viewport_top_line
            && cursor.line < self.viewport_top_line + visible_lines
        {
            let cursor_row = cursor.line - self.viewport_top_line;
            let y = content_top + cursor_row as i32 * line_height;
            let line = self.document.line(cursor.line).unwrap_or("");
            let prefix = slice_to_column(line, cursor.column);
            let prefix_width = self.renderer.measure_text(prefix, &editor_style)?.width as i32;
            let caret_x = GUTTER_WIDTH + padding + prefix_width;

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

        let file_display = self
            .open_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "[scratch]".to_string());
        let dirty_marker = if self.document.is_dirty() { " [+]" } else { "" };
        let mut left_text = format!("{file_display}{dirty_marker}");
        if let Some(message) = &self.status_message {
            left_text.push_str("  |  ");
            left_text.push_str(message);
        }

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

fn load_document_from_path(path: &PathBuf) -> Result<Document> {
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("unable to read {}", path.display()))?;
        Ok(Document::from_text(&content))
    } else {
        Ok(Document::new())
    }
}
