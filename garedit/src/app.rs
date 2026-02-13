use anyhow::Result;
use garedit_core::{Document, EditCommand, Position, Selection};
use garedit_ipc::{Command, Response, ResponseData};
use gartk_core::{
    InputEvent, Key, KeyEvent, MouseButton, Rect, SelectionNotifyEvent, SelectionRequestEvent,
    Theme, ThemeBuilder,
};
use gartk_render::{Renderer, Surface, TextStyle};
use gartk_x11::{Atoms, Connection, EventLoop, EventLoopConfig, Window, WindowConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, EventMask, ImageFormat, PropMode,
    SelectionNotifyEvent as XSelectionNotifyEvent,
};
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

const GUTTER_WIDTH: i32 = 64;
const TAB_BAR_HEIGHT: i32 = 30;
const TAB_WIDTH: i32 = 220;
const STATUS_BAR_HEIGHT: i32 = 28;
const MAX_RECENT_FILES: usize = 20;
const MAX_PALETTE_PREVIEW: usize = 6;
const SESSION_SCHEMA_VERSION: u32 = 1;
const DEFAULT_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy)]
enum PendingAction {
    Quit,
}

#[derive(Debug, Clone, Copy)]
enum PromptKind {
    OpenPath,
    SaveAsPath,
    FindQuery,
    GoToLine,
    OpenRecent,
    CommandPalette,
    ConfirmDiscard { next: PendingAction },
}

#[derive(Debug, Clone)]
struct PromptState {
    kind: PromptKind,
    input: String,
    replace_on_type: bool,
    selection_index: usize,
}

impl PromptState {
    fn open_path() -> Self {
        Self {
            kind: PromptKind::OpenPath,
            input: String::new(),
            replace_on_type: false,
            selection_index: 0,
        }
    }

    fn save_as_path(seed: Option<&Path>) -> Self {
        Self {
            kind: PromptKind::SaveAsPath,
            input: seed
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            replace_on_type: false,
            selection_index: 0,
        }
    }

    fn find_query(seed: Option<&str>, replace_on_type: bool) -> Self {
        Self {
            kind: PromptKind::FindQuery,
            input: seed.unwrap_or_default().to_string(),
            replace_on_type,
            selection_index: 0,
        }
    }

    fn go_to_line(seed_line: usize) -> Self {
        Self {
            kind: PromptKind::GoToLine,
            input: seed_line.to_string(),
            replace_on_type: false,
            selection_index: 0,
        }
    }

    fn open_recent() -> Self {
        Self {
            kind: PromptKind::OpenRecent,
            input: String::new(),
            replace_on_type: false,
            selection_index: 0,
        }
    }

    fn command_palette() -> Self {
        Self {
            kind: PromptKind::CommandPalette,
            input: String::new(),
            replace_on_type: false,
            selection_index: 0,
        }
    }

    fn confirm_discard(next: PendingAction) -> Self {
        Self {
            kind: PromptKind::ConfirmDiscard { next },
            input: String::new(),
            replace_on_type: false,
            selection_index: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SearchMatch {
    start: Position,
    end: Position,
}

#[derive(Debug, Clone)]
struct SearchState {
    query: String,
    last_match: Option<SearchMatch>,
}

impl SearchState {
    fn new(query: String) -> Self {
        Self {
            query,
            last_match: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteCommand {
    OpenFile,
    OpenRecent,
    Save,
    SaveAs,
    NewTab,
    NextTab,
    PrevTab,
    Find,
    FindNext,
    FindPrev,
    GoToLine,
    Copy,
    Cut,
    Paste,
    Quit,
}

impl PaletteCommand {
    fn id(self) -> &'static str {
        match self {
            Self::OpenFile => "open",
            Self::OpenRecent => "recent",
            Self::Save => "save",
            Self::SaveAs => "save_as",
            Self::NewTab => "new_tab",
            Self::NextTab => "next_tab",
            Self::PrevTab => "prev_tab",
            Self::Find => "find",
            Self::FindNext => "find_next",
            Self::FindPrev => "find_prev",
            Self::GoToLine => "go_to_line",
            Self::Copy => "copy",
            Self::Cut => "cut",
            Self::Paste => "paste",
            Self::Quit => "quit",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::OpenFile => "open file",
            Self::OpenRecent => "open recent",
            Self::Save => "save file",
            Self::SaveAs => "save as",
            Self::NewTab => "new tab",
            Self::NextTab => "next tab",
            Self::PrevTab => "previous tab",
            Self::Find => "find",
            Self::FindNext => "find next",
            Self::FindPrev => "find previous",
            Self::GoToLine => "go to line",
            Self::Copy => "copy selection",
            Self::Cut => "cut selection",
            Self::Paste => "paste clipboard",
            Self::Quit => "quit",
        }
    }

    fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::OpenFile => &["o", "open_file", "openfile", "load"],
            Self::OpenRecent => &["r", "recent_files", "recentfiles"],
            Self::Save => &["s", "write", "w"],
            Self::SaveAs => &["saveas", "save_as", "writeas"],
            Self::NewTab => &["t", "tabnew"],
            Self::NextTab => &["tabnext", "tn", "next"],
            Self::PrevTab => &["tabprev", "tp", "prev", "previous"],
            Self::Find => &["search", "f"],
            Self::FindNext => &["n", "next_match", "search_next"],
            Self::FindPrev => &["p", "prev_match", "search_prev"],
            Self::GoToLine => &["goto", "go", "line", "jump"],
            Self::Copy => &["yank", "cp"],
            Self::Cut => &["delete_selection", "x"],
            Self::Paste => &["insert_clipboard", "v"],
            Self::Quit => &["q", "exit", "close"],
        }
    }
}

const PALETTE_COMMANDS: [PaletteCommand; 15] = [
    PaletteCommand::OpenFile,
    PaletteCommand::OpenRecent,
    PaletteCommand::Save,
    PaletteCommand::SaveAs,
    PaletteCommand::NewTab,
    PaletteCommand::NextTab,
    PaletteCommand::PrevTab,
    PaletteCommand::Find,
    PaletteCommand::FindNext,
    PaletteCommand::FindPrev,
    PaletteCommand::GoToLine,
    PaletteCommand::Copy,
    PaletteCommand::Cut,
    PaletteCommand::Paste,
    PaletteCommand::Quit,
];

#[derive(Debug, Clone, Copy)]
struct PendingPaste {
    selection: Atom,
}

#[derive(Debug, Clone)]
struct OpenTab {
    document: Document,
    viewport_top_line: usize,
    search: Option<SearchState>,
}

impl OpenTab {
    fn from_active(
        document: Document,
        viewport_top_line: usize,
        search: Option<SearchState>,
    ) -> Self {
        Self {
            document,
            viewport_top_line,
            search,
        }
    }
}

#[derive(Debug, Clone)]
struct SessionPaths {
    session_file: PathBuf,
    autosave_dir: PathBuf,
}

impl SessionPaths {
    fn resolve() -> Self {
        let base = dirs::state_dir()
            .or_else(dirs::cache_dir)
            .unwrap_or_else(std::env::temp_dir);
        let state_dir = base.join("garedit");
        Self {
            session_file: state_dir.join("session.json"),
            autosave_dir: state_dir.join("autosave"),
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        if let Some(parent) = self.session_file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&self.autosave_dir)?;
        Ok(())
    }

    fn autosave_path_for_index(&self, index: usize) -> PathBuf {
        self.autosave_dir.join(format!("tab-{index}.txt"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionSnapshot {
    version: u32,
    active_tab: usize,
    tabs: Vec<SessionTabSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionTabSnapshot {
    path: Option<PathBuf>,
    cursor_line: usize,
    cursor_column: usize,
    viewport_top_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    autosave_name: Option<String>,
    dirty: bool,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub width: u32,
    pub height: u32,
    pub font_family: String,
    pub font_size: f64,
    pub theme_name: String,
    pub tab_width: usize,
    pub show_line_numbers: bool,
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub start_hidden: bool,
    pub disable_ipc: bool,
    pub restore_session: bool,
    pub autosave_interval: Duration,
}

pub struct App {
    window: Window,
    renderer: Renderer,
    gc: u32,
    theme: Theme,
    tabs: Vec<OpenTab>,
    active_tab: usize,
    document: Document,
    status_message: Option<String>,
    tab_width: usize,
    show_line_numbers: bool,
    viewport_top_line: usize,
    pointer_drag_anchor: Option<Position>,
    recent_files: Vec<PathBuf>,
    search: Option<SearchState>,
    prompt: Option<PromptState>,
    clipboard_atoms: Atoms,
    selection_text: Option<String>,
    local_clipboard: Option<String>,
    paste_property: Atom,
    pending_paste: Option<PendingPaste>,
    ipc_listener: Option<UnixListener>,
    ipc_socket_path: Option<PathBuf>,
    window_visible: bool,
    session_paths: SessionPaths,
    autosave_interval: Duration,
    last_session_persist: Instant,
    should_quit: bool,
}

impl App {
    pub fn new(config: AppConfig) -> Result<Self> {
        let conn = Connection::connect(None)?;
        let clipboard_atoms = Atoms::new(&conn)?;
        let paste_property = conn.intern_atom("GAREDIT_CLIPBOARD", false)?;
        let monitor = gartk_x11::monitor_at_pointer(&conn)?;
        let session_paths = SessionPaths::resolve();
        let startup_file = config.file.clone();

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
                .transparent(false)
                .map_on_create(!config.start_hidden),
        )?;
        if !config.start_hidden {
            window.activate()?;
            window.focus()?;
            conn.flush()?;
        }

        let (ipc_listener, ipc_socket_path) = Self::setup_ipc_listener(config.disable_ipc)?;

        let theme = ThemeBuilder::from(resolve_theme(&config.theme_name))
            .font_family(config.font_family)
            .font_size(config.font_size)
            .build();
        let renderer = Renderer::with_theme(width, height, theme.clone())?;

        let gc = conn.generate_id()?;
        conn.inner()
            .create_gc(gc, window.id(), &Default::default())?;
        conn.flush()?;

        let (document, status_message) = if let Some(path) = startup_file.as_ref() {
            let exists = path.exists();
            match open_or_create_document(path) {
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
            tabs: vec![OpenTab::from_active(document.clone(), 0, None)],
            active_tab: 0,
            document,
            status_message,
            tab_width: config.tab_width.max(1),
            show_line_numbers: config.show_line_numbers,
            viewport_top_line: 0,
            pointer_drag_anchor: None,
            recent_files: Vec::new(),
            search: None,
            prompt: None,
            clipboard_atoms,
            selection_text: None,
            local_clipboard: None,
            paste_property,
            pending_paste: None,
            ipc_listener,
            ipc_socket_path,
            window_visible: !config.start_hidden,
            session_paths,
            autosave_interval: if config.autosave_interval.is_zero() {
                DEFAULT_AUTOSAVE_INTERVAL
            } else {
                config.autosave_interval
            },
            last_session_persist: Instant::now(),
            should_quit: false,
        };
        if startup_file.is_none() && config.restore_session {
            if let Err(err) = app.restore_session() {
                tracing::warn!("failed to restore session: {err}");
            }
        }

        let recent_files: Vec<PathBuf> = app
            .tabs
            .iter()
            .filter_map(|tab| tab.document.path().map(Path::to_path_buf))
            .collect();
        for path in recent_files {
            app.remember_recent(&path);
        }
        if config.line.is_some() || config.column.is_some() {
            app.jump_to_position(config.line, config.column);
        }
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

        if let Err(err) = self.persist_session_snapshot() {
            tracing::warn!("failed to persist session on shutdown: {err}");
        }

        Ok(())
    }

    fn setup_ipc_listener(disable_ipc: bool) -> Result<(Option<UnixListener>, Option<PathBuf>)> {
        if disable_ipc {
            return Ok((None, None));
        }

        let socket_path = garedit_ipc::socket_path();
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
        }

        if socket_path.exists() {
            match UnixStream::connect(&socket_path) {
                Ok(_) => {
                    anyhow::bail!(
                        "another garedit instance is already listening at {}",
                        socket_path.display()
                    );
                }
                Err(_) => {
                    let _ = fs::remove_file(&socket_path);
                }
            }
        }

        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        Ok((Some(listener), Some(socket_path)))
    }

    fn persist_active_tab(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.document = self.document.clone();
            tab.viewport_top_line = self.viewport_top_line;
            tab.search = self.search.clone();
        }
    }

    fn activate_tab(&mut self, index: usize) {
        if index >= self.tabs.len() || index == self.active_tab {
            return;
        }

        self.persist_active_tab();
        self.active_tab = index;
        if let Some(tab) = self.tabs.get(index) {
            self.document = tab.document.clone();
            self.viewport_top_line = tab.viewport_top_line;
            self.search = tab.search.clone();
        }
        self.pointer_drag_anchor = None;
        self.prompt = None;
        self.clamp_viewport();
    }

    fn switch_tab(&mut self, direction: isize) {
        if self.tabs.len() <= 1 {
            return;
        }

        let len = self.tabs.len() as isize;
        let next = (self.active_tab as isize + direction).rem_euclid(len) as usize;
        self.activate_tab(next);
        self.status_message = Some(format!("tab {}/{}", self.active_tab + 1, self.tabs.len()));
    }

    fn new_tab(&mut self) {
        self.persist_active_tab();
        self.document = Document::new();
        self.search = None;
        self.viewport_top_line = 0;
        self.pointer_drag_anchor = None;
        self.prompt = None;
        self.tabs
            .push(OpenTab::from_active(self.document.clone(), 0, None));
        self.active_tab = self.tabs.len() - 1;
        self.status_message = Some("new tab".to_string());
    }

    fn tab_title(&self, index: usize) -> String {
        let (path, dirty) = if index == self.active_tab {
            (
                self.document.path().map(Path::to_path_buf),
                self.document.is_dirty(),
            )
        } else {
            let tab = &self.tabs[index];
            (
                tab.document.path().map(Path::to_path_buf),
                tab.document.is_dirty(),
            )
        };

        let mut title = path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "[scratch]".to_string());
        if dirty {
            title.push_str(" *");
        }
        title
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
                } else if mouse_event.button == Some(MouseButton::Middle) {
                    self.request_clipboard_paste(self.clipboard_atoms.primary);
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
            InputEvent::SelectionRequest(selection_request) => {
                if let Err(err) = self.handle_selection_request(&selection_request) {
                    self.status_message = Some(format!("clipboard request failed: {err}"));
                }
            }
            InputEvent::SelectionNotify(selection_notify) => {
                if let Err(err) = self.handle_selection_notify(&selection_notify) {
                    self.status_message = Some(format!("paste failed: {err}"));
                } else {
                    self.ensure_cursor_visible();
                }
                ev.request_redraw();
            }
            InputEvent::SelectionClear => {
                self.pending_paste = None;
            }
            InputEvent::Idle => {
                let ipc_redraw = self.poll_ipc_commands();
                let autosave_redraw = self.maybe_persist_session();
                if ipc_redraw || autosave_redraw {
                    ev.request_redraw();
                }
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
            Key::F3 => self.repeat_find_or_prompt(key_event.modifiers.shift),
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
            Key::Tab => {
                if key_event.modifiers.shift {
                    self.switch_tab(-1);
                } else {
                    self.switch_tab(1);
                }
            }
            Key::Left => self.move_cursor(EditCommand::MoveWordLeft, key_event.modifiers.shift),
            Key::Right => self.move_cursor(EditCommand::MoveWordRight, key_event.modifiers.shift),
            Key::Backspace => self.document.apply(EditCommand::DeleteWordBackward),
            Key::Delete => self.document.apply(EditCommand::DeleteWordForward),
            Key::Char(c) => {
                let lower = c.to_ascii_lowercase();
                match lower {
                    'q' => self.request_quit(),
                    't' => self.new_tab(),
                    'o' => self.begin_open_file_flow(),
                    'r' => self.begin_open_recent_flow(),
                    's' => {
                        if key_event.modifiers.shift {
                            self.begin_save_as_prompt();
                        } else {
                            self.save_current_document();
                        }
                    }
                    'g' => {
                        self.prompt = Some(PromptState::go_to_line(self.document.cursor().line + 1))
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
                    'f' => self.open_find_prompt(),
                    'p' => {
                        if key_event.modifiers.shift {
                            self.open_command_palette();
                        } else {
                            self.move_cursor(EditCommand::MoveUp, key_event.modifiers.shift);
                        }
                    }
                    'n' => self.move_cursor(EditCommand::MoveDown, key_event.modifiers.shift),
                    'h' => self.document.apply(EditCommand::Backspace),
                    'd' => self.document.apply(EditCommand::Delete),
                    'w' => self.document.apply(EditCommand::DeleteWordBackward),
                    'c' => self.copy_selection_to_clipboard(),
                    'x' => self.cut_selection_to_clipboard(),
                    'v' => self.request_clipboard_paste(self.clipboard_atoms.clipboard),
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
                        PendingAction::Quit => self.should_quit = true,
                    }
                }
                _ => {}
            }
            return;
        }

        if matches!(kind, PromptKind::CommandPalette) {
            let palette_move = match key_event.key {
                Key::Up => Some(-1isize),
                Key::Down => Some(1isize),
                Key::Tab => Some(if key_event.modifiers.shift { -1 } else { 1 }),
                Key::Char('n')
                    if key_event.modifiers.ctrl
                        && !key_event.modifiers.alt
                        && !key_event.modifiers.super_key =>
                {
                    Some(1isize)
                }
                Key::Char('p')
                    if key_event.modifiers.ctrl
                        && !key_event.modifiers.alt
                        && !key_event.modifiers.super_key =>
                {
                    Some(-1isize)
                }
                _ => None,
            };
            if let Some(delta) = palette_move {
                self.move_palette_selection(delta);
                return;
            }
        }

        let mut submit: Option<(PromptKind, String, bool, Option<usize>)> = None;
        let mut cancel = false;

        if let Some(prompt) = self.prompt.as_mut() {
            match key_event.key {
                Key::Escape => cancel = true,
                Key::Return => {
                    submit = Some((
                        prompt.kind,
                        prompt.input.trim().to_string(),
                        matches!(prompt.kind, PromptKind::FindQuery) && key_event.modifiers.shift,
                        matches!(prompt.kind, PromptKind::CommandPalette)
                            .then_some(prompt.selection_index),
                    ));
                }
                Key::Backspace => {
                    if prompt.replace_on_type {
                        prompt.input.clear();
                        prompt.replace_on_type = false;
                    } else {
                        prompt.input.pop();
                    }
                    if matches!(prompt.kind, PromptKind::CommandPalette) {
                        prompt.selection_index = 0;
                    }
                }
                Key::Space => {
                    if prompt.replace_on_type {
                        prompt.input.clear();
                        prompt.replace_on_type = false;
                    }
                    prompt.input.push(' ');
                    if matches!(prompt.kind, PromptKind::CommandPalette) {
                        prompt.selection_index = 0;
                    }
                }
                Key::Char(c) => {
                    if key_event.modifiers.ctrl {
                        let lower = c.to_ascii_lowercase();
                        if lower == 'u' {
                            prompt.input.clear();
                            prompt.replace_on_type = false;
                            if matches!(prompt.kind, PromptKind::CommandPalette) {
                                prompt.selection_index = 0;
                            }
                        }
                    } else if !(key_event.modifiers.alt || key_event.modifiers.super_key) {
                        if prompt.replace_on_type {
                            prompt.input.clear();
                            prompt.replace_on_type = false;
                        }
                        prompt.input.push(c);
                        if matches!(prompt.kind, PromptKind::CommandPalette) {
                            prompt.selection_index = 0;
                        }
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

        let Some((kind, raw_input, reverse, selected_index)) = submit else {
            return;
        };
        if !matches!(kind, PromptKind::FindQuery) {
            self.prompt = None;
        }

        match kind {
            PromptKind::OpenPath => {
                if raw_input.is_empty() {
                    self.status_message = Some("path is empty".to_string());
                    return;
                }
                let path = resolve_user_path(&raw_input);
                self.open_path(path);
            }
            PromptKind::SaveAsPath => {
                if raw_input.is_empty() {
                    self.status_message = Some("path is empty".to_string());
                    return;
                }
                let path = resolve_user_path(&raw_input);
                self.save_as_path(path);
            }
            PromptKind::FindQuery => {
                if raw_input.is_empty() {
                    self.status_message = Some("search query is empty".to_string());
                    return;
                }
                self.set_search_query(raw_input);
                let _ = self.find_next_match(reverse);
            }
            PromptKind::GoToLine => self.go_to_line_prompt_submit(&raw_input),
            PromptKind::OpenRecent => self.open_recent_prompt_submit(&raw_input),
            PromptKind::CommandPalette => self.command_palette_submit(&raw_input, selected_index),
            PromptKind::ConfirmDiscard { .. } => {}
        }
    }

    fn begin_open_file_flow(&mut self) {
        self.prompt = Some(PromptState::open_path());
    }

    fn begin_open_recent_flow(&mut self) {
        self.prompt = Some(PromptState::open_recent());
    }

    fn begin_save_as_prompt(&mut self) {
        self.prompt = Some(PromptState::save_as_path(self.document.path()));
    }

    fn open_command_palette(&mut self) {
        self.prompt = Some(PromptState::command_palette());
    }

    fn open_find_prompt(&mut self) {
        let seed = self
            .search
            .as_ref()
            .map(|state| state.query.trim())
            .filter(|query| !query.is_empty());
        self.prompt = Some(PromptState::find_query(seed, seed.is_some()));
    }

    fn repeat_find_or_prompt(&mut self, reverse: bool) {
        let has_query = self
            .search
            .as_ref()
            .map(|state| !state.query.trim().is_empty())
            .unwrap_or(false);
        if has_query {
            let _ = self.find_next_match(reverse);
        } else {
            self.open_find_prompt();
        }
    }

    fn set_search_query(&mut self, query: String) {
        match self.search.as_mut() {
            Some(search) if search.query == query => {}
            Some(search) => {
                search.query = query;
                search.last_match = None;
            }
            None => {
                self.search = Some(SearchState::new(query));
            }
        }
    }

    fn copy_selection_to_clipboard(&mut self) {
        let Some(text) = self.selected_text() else {
            self.status_message = Some("nothing selected".to_string());
            return;
        };
        let copied_chars = text.chars().count();
        if let Err(err) = self.set_clipboard_text(text) {
            self.status_message = Some(format!("copy failed: {err}"));
            return;
        }
        self.status_message = Some(format!("copied {copied_chars} chars"));
    }

    fn cut_selection_to_clipboard(&mut self) {
        let Some(text) = self.selected_text() else {
            self.status_message = Some("nothing selected".to_string());
            return;
        };
        let cut_chars = text.chars().count();
        if let Err(err) = self.set_clipboard_text(text) {
            self.status_message = Some(format!("cut failed: {err}"));
            return;
        }
        self.document.apply(EditCommand::Delete);
        self.status_message = Some(format!("cut {cut_chars} chars"));
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.document.selection()?;
        let (start, end) = selection.normalized();
        if start == end {
            return None;
        }

        if start.line == end.line {
            let line = self.document.line(start.line).unwrap_or("");
            let start_byte = column_to_byte_idx(line, start.column);
            let end_byte = column_to_byte_idx(line, end.column);
            return Some(line[start_byte..end_byte].to_string());
        }

        let mut out = String::new();
        for line_idx in start.line..=end.line {
            let line = self.document.line(line_idx).unwrap_or("");
            if line_idx == start.line {
                let start_byte = column_to_byte_idx(line, start.column);
                out.push_str(&line[start_byte..]);
            } else if line_idx == end.line {
                let end_byte = column_to_byte_idx(line, end.column);
                out.push_str(&line[..end_byte]);
            } else {
                out.push_str(line);
            }
            if line_idx < end.line {
                out.push('\n');
            }
        }
        Some(out)
    }

    fn set_clipboard_text(&mut self, text: String) -> Result<()> {
        self.local_clipboard = Some(text.clone());
        self.selection_text = Some(text);

        let conn = self.window.connection();
        conn.inner().set_selection_owner(
            self.window.id(),
            self.clipboard_atoms.clipboard,
            x11rb::CURRENT_TIME,
        )?;
        conn.inner().set_selection_owner(
            self.window.id(),
            self.clipboard_atoms.primary,
            x11rb::CURRENT_TIME,
        )?;
        conn.flush()?;
        Ok(())
    }

    fn request_clipboard_paste(&mut self, selection: Atom) {
        if self.pending_paste.is_some() {
            return;
        }

        let paste_error = {
            let conn = self.window.connection();
            if let Err(err) = conn.inner().convert_selection(
                self.window.id(),
                selection,
                self.clipboard_atoms.utf8_string,
                self.paste_property,
                x11rb::CURRENT_TIME,
            ) {
                Some(err.to_string())
            } else if let Err(err) = conn.flush() {
                Some(err.to_string())
            } else {
                None
            }
        };

        if let Some(err) = paste_error {
            if !self.paste_from_local_clipboard() {
                self.status_message = Some(format!("paste request failed: {err}"));
            }
            return;
        }

        self.pending_paste = Some(PendingPaste { selection });
    }

    fn paste_from_local_clipboard(&mut self) -> bool {
        let Some(text) = self.local_clipboard.clone() else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        let pasted_chars = text.chars().count();
        self.document.apply(EditCommand::InsertText(text));
        self.status_message = Some(format!("pasted {pasted_chars} chars"));
        true
    }

    fn handle_selection_request(&mut self, event: &SelectionRequestEvent) -> Result<()> {
        if event.selection != self.clipboard_atoms.clipboard
            && event.selection != self.clipboard_atoms.primary
        {
            self.send_selection_notify(event, AtomEnum::NONE.into())?;
            return Ok(());
        }

        let Some(text) = self.selection_text.as_ref() else {
            self.send_selection_notify(event, AtomEnum::NONE.into())?;
            return Ok(());
        };

        let property = if event.property == 0 {
            event.target
        } else {
            event.property
        };
        let conn = self.window.connection();

        if event.target == self.clipboard_atoms.targets {
            let targets: [Atom; 6] = [
                self.clipboard_atoms.targets,
                self.clipboard_atoms.utf8_string,
                self.clipboard_atoms.text_plain_utf8,
                self.clipboard_atoms.text_plain,
                self.clipboard_atoms.text,
                self.clipboard_atoms.string,
            ];
            conn.inner().change_property32(
                PropMode::REPLACE,
                event.requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )?;
            self.send_selection_notify(event, property)?;
            return Ok(());
        }

        if event.target == self.clipboard_atoms.utf8_string
            || event.target == self.clipboard_atoms.text_plain_utf8
            || event.target == self.clipboard_atoms.text_plain
            || event.target == self.clipboard_atoms.text
            || event.target == self.clipboard_atoms.string
        {
            let property_type = if event.target == self.clipboard_atoms.string {
                AtomEnum::STRING.into()
            } else {
                self.clipboard_atoms.utf8_string
            };
            conn.inner().change_property8(
                PropMode::REPLACE,
                event.requestor,
                property,
                property_type,
                text.as_bytes(),
            )?;
            self.send_selection_notify(event, property)?;
            return Ok(());
        }

        self.send_selection_notify(event, AtomEnum::NONE.into())?;
        Ok(())
    }

    fn send_selection_notify(&self, event: &SelectionRequestEvent, property: Atom) -> Result<()> {
        let notify = XSelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: event.time,
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property,
        };
        let conn = self.window.connection();
        conn.inner()
            .send_event(false, event.requestor, EventMask::NO_EVENT, notify)?;
        conn.flush()?;
        Ok(())
    }

    fn handle_selection_notify(&mut self, event: &SelectionNotifyEvent) -> Result<()> {
        let Some(pending) = self.pending_paste.take() else {
            return Ok(());
        };
        if pending.selection != event.selection {
            return Ok(());
        }

        if event.property == u32::from(AtomEnum::NONE) {
            if !self.paste_from_local_clipboard() {
                self.status_message = Some("clipboard is empty".to_string());
            }
            return Ok(());
        }

        let conn = self.window.connection();
        let reply = conn
            .inner()
            .get_property(
                true,
                self.window.id(),
                event.property,
                AtomEnum::ANY,
                0,
                u32::MAX,
            )?
            .reply()?;
        let mut text = String::from_utf8_lossy(&reply.value).into_owned();
        while text.ends_with('\0') {
            text.pop();
        }
        if text.is_empty() {
            if !self.paste_from_local_clipboard() {
                self.status_message = Some("clipboard is empty".to_string());
            }
            return Ok(());
        }
        let pasted_chars = text.chars().count();
        self.local_clipboard = Some(text.clone());
        self.document.apply(EditCommand::InsertText(text));
        self.status_message = Some(format!("pasted {pasted_chars} chars"));
        Ok(())
    }

    fn maybe_persist_session(&mut self) -> bool {
        if self.last_session_persist.elapsed() < self.autosave_interval {
            return false;
        }

        self.last_session_persist = Instant::now();
        if let Err(err) = self.persist_session_snapshot() {
            self.status_message = Some(format!("autosave failed: {err}"));
            return true;
        }
        false
    }

    fn persist_session_snapshot(&mut self) -> Result<()> {
        self.persist_active_tab();
        self.session_paths.ensure_dirs()?;

        let mut keep_autosaves = HashSet::new();
        let mut tabs = Vec::with_capacity(self.tabs.len());

        for (index, tab) in self.tabs.iter().enumerate() {
            let document = &tab.document;
            let dirty = document.is_dirty();
            let autosave_name = if dirty {
                let autosave_path = self.session_paths.autosave_path_for_index(index);
                fs::write(&autosave_path, document.serialized_text())?;
                let file_name = autosave_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string();
                if !file_name.is_empty() {
                    keep_autosaves.insert(file_name.clone());
                    Some(file_name)
                } else {
                    None
                }
            } else {
                None
            };

            let cursor = document.cursor();
            tabs.push(SessionTabSnapshot {
                path: document.path().map(Path::to_path_buf),
                cursor_line: cursor.line,
                cursor_column: cursor.column,
                viewport_top_line: tab.viewport_top_line,
                search_query: tab
                    .search
                    .as_ref()
                    .map(|search| search.query.trim().to_string())
                    .filter(|query| !query.is_empty()),
                autosave_name,
                dirty,
            });
        }

        self.prune_autosave_files(&keep_autosaves)?;

        let snapshot = SessionSnapshot {
            version: SESSION_SCHEMA_VERSION,
            active_tab: self.active_tab.min(tabs.len().saturating_sub(1)),
            tabs,
        };
        let payload = serde_json::to_vec_pretty(&snapshot)?;
        let temp_path = self.session_paths.session_file.with_extension("json.tmp");
        fs::write(&temp_path, payload)?;
        fs::rename(&temp_path, &self.session_paths.session_file)?;
        Ok(())
    }

    fn prune_autosave_files(&self, keep_autosaves: &HashSet<String>) -> Result<()> {
        if !self.session_paths.autosave_dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&self.session_paths.autosave_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if keep_autosaves.contains(file_name) {
                continue;
            }
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    fn restore_session(&mut self) -> Result<()> {
        let session_file = &self.session_paths.session_file;
        if !session_file.exists() {
            return Ok(());
        }

        let raw = fs::read_to_string(session_file)?;
        let snapshot: SessionSnapshot = match serde_json::from_str(&raw) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::warn!("invalid session snapshot {}: {err}", session_file.display());
                return Ok(());
            }
        };
        if snapshot.version != SESSION_SCHEMA_VERSION {
            tracing::warn!(
                "ignoring session snapshot version {} (expected {})",
                snapshot.version,
                SESSION_SCHEMA_VERSION
            );
            return Ok(());
        }
        if snapshot.tabs.is_empty() {
            return Ok(());
        }

        let active_tab = snapshot.active_tab;
        let mut restored_tabs = Vec::with_capacity(snapshot.tabs.len());
        for tab_snapshot in snapshot.tabs {
            restored_tabs.push(self.restore_session_tab(tab_snapshot)?);
        }
        if restored_tabs.is_empty() {
            return Ok(());
        }

        self.tabs = restored_tabs;
        self.active_tab = active_tab.min(self.tabs.len() - 1);
        if let Some(active_tab) = self.tabs.get(self.active_tab).cloned() {
            self.document = active_tab.document;
            self.viewport_top_line = active_tab.viewport_top_line;
            self.search = active_tab.search;
        }
        self.pointer_drag_anchor = None;
        self.prompt = None;
        self.clamp_viewport();
        self.status_message = Some(format!("restored {} tab(s)", self.tabs.len()));
        Ok(())
    }

    fn restore_session_tab(&self, tab: SessionTabSnapshot) -> Result<OpenTab> {
        let mut document = self.restore_document_from_snapshot(&tab)?;
        document.set_cursor(Position::new(tab.cursor_line, tab.cursor_column));

        let search = tab
            .search_query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(|query| SearchState::new(query.to_string()));
        let viewport_top_line = tab
            .viewport_top_line
            .min(document.line_count().saturating_sub(1));

        Ok(OpenTab::from_active(document, viewport_top_line, search))
    }

    fn restore_document_from_snapshot(&self, tab: &SessionTabSnapshot) -> Result<Document> {
        if tab.dirty {
            if let Some(autosave_name) = tab.autosave_name.as_deref() {
                if is_safe_autosave_name(autosave_name) {
                    let autosave_path = self.session_paths.autosave_dir.join(autosave_name);
                    if autosave_path.exists() {
                        let raw = fs::read_to_string(&autosave_path)?;
                        let mut document = Document::from_text(&raw);
                        document.set_path(tab.path.clone());
                        document.mark_dirty();
                        return Ok(document);
                    }
                }
            }
        }

        let mut document = match tab.path.as_ref() {
            Some(path) if path.exists() => Document::open_path(path)?,
            Some(path) => {
                let mut document = Document::new();
                document.set_path(Some(path.clone()));
                document
            }
            None => Document::new(),
        };
        if tab.dirty {
            document.mark_dirty();
        }
        Ok(document)
    }

    fn poll_ipc_commands(&mut self) -> bool {
        let mut needs_redraw = false;

        loop {
            let accept_result = {
                let Some(listener) = self.ipc_listener.as_ref() else {
                    break;
                };
                listener.accept()
            };

            match accept_result {
                Ok((stream, _addr)) => {
                    if self.handle_ipc_stream(stream) {
                        needs_redraw = true;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    self.status_message = Some(format!("ipc accept failed: {err}"));
                    break;
                }
            }
        }

        needs_redraw
    }

    fn handle_ipc_stream(&mut self, mut stream: UnixStream) -> bool {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));

        let mut payload = Vec::new();
        let (response, needs_redraw) = match stream.read_to_end(&mut payload) {
            Ok(_) if payload.is_empty() => (Response::err("empty IPC request"), false),
            Ok(_) => match serde_json::from_slice::<Command>(&payload) {
                Ok(command) => self.apply_ipc_command(command),
                Err(err) => (Response::err(format!("invalid IPC payload: {err}")), false),
            },
            Err(err) => (
                Response::err(format!("failed to read IPC payload: {err}")),
                false,
            ),
        };

        if let Ok(serialized) = serde_json::to_vec(&response) {
            let _ = stream.write_all(&serialized);
            let _ = stream.flush();
        }

        needs_redraw
    }

    fn apply_ipc_command(&mut self, command: Command) -> (Response, bool) {
        match command {
            Command::Open { path, line, column } => {
                let requested = normalize_path_for_match(&path);
                self.open_path(requested);
                if let Some(message) = self.status_message.as_ref() {
                    if message.starts_with("open failed:") {
                        return (Response::err(message.clone()), true);
                    }
                }

                if line.is_some() || column.is_some() {
                    self.jump_to_position(line, column);
                }

                if let Err(err) = self.show_window() {
                    return (Response::err(format!("failed to show window: {err}")), true);
                }
                (Response::ok(), true)
            }
            Command::Show => match self.show_window() {
                Ok(()) => (Response::ok(), true),
                Err(err) => (
                    Response::err(format!("failed to show window: {err}")),
                    false,
                ),
            },
            Command::Hide => match self.hide_window() {
                Ok(()) => (Response::ok(), true),
                Err(err) => (
                    Response::err(format!("failed to hide window: {err}")),
                    false,
                ),
            },
            Command::Toggle => {
                let result = if self.window_visible {
                    self.hide_window()
                } else {
                    self.show_window()
                };
                match result {
                    Ok(()) => (Response::ok(), true),
                    Err(err) => (
                        Response::err(format!("failed to toggle window: {err}")),
                        false,
                    ),
                }
            }
            Command::Status => (
                Response::ok_with_data(ResponseData::Status {
                    visible: self.window_visible,
                    open_documents: self.tabs.len(),
                    focused_document: self.document.path().map(Path::to_path_buf),
                }),
                false,
            ),
            Command::Quit => {
                if self.has_unsaved_changes() {
                    (
                        Response::err("unsaved changes present; refusing quit request"),
                        false,
                    )
                } else {
                    self.should_quit = true;
                    (Response::ok(), true)
                }
            }
        }
    }

    fn jump_to_position(&mut self, line: Option<usize>, column: Option<usize>) {
        let line_idx = line
            .unwrap_or(1)
            .saturating_sub(1)
            .min(self.document.line_count().saturating_sub(1));
        let max_col = self
            .document
            .line(line_idx)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        let col_idx = column.unwrap_or(1).saturating_sub(1).min(max_col);

        self.document.clear_selection();
        self.document.set_cursor(Position::new(line_idx, col_idx));
        self.ensure_cursor_visible();
    }

    fn show_window(&mut self) -> Result<()> {
        if !self.window_visible {
            self.window.map()?;
            self.window_visible = true;
        }
        self.window.raise()?;
        self.window.activate()?;
        self.window.focus()?;
        self.window.connection().flush()?;
        Ok(())
    }

    fn hide_window(&mut self) -> Result<()> {
        if !self.window_visible {
            return Ok(());
        }
        self.window.unmap()?;
        self.window.connection().flush()?;
        self.window_visible = false;
        Ok(())
    }

    fn has_unsaved_changes(&self) -> bool {
        if self.document.is_dirty() {
            return true;
        }
        self.tabs
            .iter()
            .enumerate()
            .any(|(idx, tab)| idx != self.active_tab && tab.document.is_dirty())
    }

    fn request_quit(&mut self) {
        if self.has_unsaved_changes() {
            self.prompt = Some(PromptState::confirm_discard(PendingAction::Quit));
            return;
        }
        self.should_quit = true;
    }

    fn open_path(&mut self, path: PathBuf) {
        let requested = normalize_path_for_match(&path);
        if let Some(index) = self.find_tab_index_by_path(&requested) {
            self.activate_tab(index);
            self.status_message = Some(format!("switched to {}", path.display()));
            return;
        }

        let existed = path.exists();
        match open_or_create_document(&path) {
            Ok(document) => {
                self.persist_active_tab();
                self.document = document;
                self.tabs.push(OpenTab::from_active(
                    self.document.clone(),
                    0,
                    self.search.clone(),
                ));
                self.active_tab = self.tabs.len() - 1;
                self.viewport_top_line = 0;
                self.pointer_drag_anchor = None;
                self.search = None;
                if let Some(opened) = self.document.path().map(Path::to_path_buf) {
                    self.remember_recent(&opened);
                }
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

    fn find_tab_index_by_path(&self, target: &Path) -> Option<usize> {
        if let Some(path) = self.document.path() {
            if normalize_path_for_match(path) == target {
                return Some(self.active_tab);
            }
        }

        for (idx, tab) in self.tabs.iter().enumerate() {
            if idx == self.active_tab {
                continue;
            }
            let Some(path) = tab.document.path() else {
                continue;
            };
            if normalize_path_for_match(path) == target {
                return Some(idx);
            }
        }
        None
    }

    fn save_current_document(&mut self) {
        if self.document.path().is_some() {
            match self.document.save() {
                Ok(()) => {
                    if let Some(path) = self.document.path() {
                        let path_buf = path.to_path_buf();
                        self.remember_recent(&path_buf);
                        self.status_message = Some(format!("saved {}", path_buf.display()));
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
            Ok(()) => {
                self.remember_recent(&path);
                self.status_message = Some(format!("saved {}", path.display()));
            }
            Err(err) => self.status_message = Some(format!("save failed: {err}")),
        }
    }

    fn remember_recent(&mut self, path: &Path) {
        let path = path.to_path_buf();
        self.recent_files.retain(|existing| existing != &path);
        self.recent_files.insert(0, path);
        if self.recent_files.len() > MAX_RECENT_FILES {
            self.recent_files.truncate(MAX_RECENT_FILES);
        }
    }

    fn open_recent_prompt_submit(&mut self, raw_input: &str) {
        if self.recent_files.is_empty() {
            self.status_message = Some("no recent files".to_string());
            return;
        }

        if raw_input.is_empty() {
            let path = self.recent_files[0].clone();
            self.open_path(path);
            return;
        }

        if let Ok(index) = raw_input.parse::<usize>() {
            if index == 0 || index > self.recent_files.len() {
                self.status_message = Some(format!("recent index out of range: {index}"));
                return;
            }
            let path = self.recent_files[index - 1].clone();
            self.open_path(path);
            return;
        }

        let query = raw_input.to_ascii_lowercase();
        if let Some(path) = self
            .recent_files
            .iter()
            .find(|path| path.to_string_lossy().to_ascii_lowercase().contains(&query))
            .cloned()
        {
            self.open_path(path);
        } else {
            self.status_message = Some(format!("no recent file matching '{raw_input}'"));
        }
    }

    fn has_active_selection(&self) -> bool {
        self.document
            .selection()
            .map(|selection| !selection.is_collapsed())
            .unwrap_or(false)
    }

    fn palette_command_available(&self, command: PaletteCommand) -> bool {
        match command {
            PaletteCommand::Copy | PaletteCommand::Cut => self.has_active_selection(),
            _ => true,
        }
    }

    fn palette_token_score(command: PaletteCommand, token: &str) -> Option<usize> {
        let mut best: Option<usize> = None;
        for candidate in std::iter::once(command.id())
            .chain(std::iter::once(command.label()))
            .chain(command.aliases().iter().copied())
        {
            let score = if candidate == token {
                Some(0usize)
            } else if candidate.starts_with(token) {
                Some(1usize)
            } else if candidate.contains(token) {
                Some(2usize)
            } else if is_fuzzy_subsequence(candidate, token) {
                Some(3usize)
            } else {
                None
            };
            if let Some(score) = score {
                best = Some(best.map_or(score, |current| current.min(score)));
            }
        }
        best
    }

    fn palette_match_score(&self, command: PaletteCommand, tokens: &[&str]) -> Option<usize> {
        if !self.palette_command_available(command) {
            return None;
        }
        if tokens.is_empty() {
            return Some(0);
        }

        let mut score = 0usize;
        for token in tokens {
            score += Self::palette_token_score(command, token)?;
        }
        Some(score)
    }

    fn matching_palette_commands(&self, raw_query: &str) -> Vec<PaletteCommand> {
        let query = raw_query.trim().to_ascii_lowercase();
        let tokens: Vec<&str> = query.split_whitespace().collect();

        let mut scored: Vec<(usize, usize, PaletteCommand)> = PALETTE_COMMANDS
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(order, command)| {
                self.palette_match_score(command, &tokens)
                    .map(|score| (score, order, command))
            })
            .collect();

        scored.sort_by_key(|(score, order, _)| (*score, *order));
        scored.into_iter().map(|(_, _, command)| command).collect()
    }

    fn move_palette_selection(&mut self, delta: isize) {
        let (input, selection_index) = match self.prompt.as_ref() {
            Some(prompt) if matches!(prompt.kind, PromptKind::CommandPalette) => {
                (prompt.input.clone(), prompt.selection_index)
            }
            _ => return,
        };
        let len = self.matching_palette_commands(&input).len();
        if len == 0 {
            return;
        }

        let next = (selection_index as isize + delta).rem_euclid(len as isize) as usize;
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.selection_index = next;
        }
    }

    fn command_palette_submit(&mut self, raw_input: &str, selected_index: Option<usize>) {
        let query = raw_input.trim();
        let matches = self.matching_palette_commands(query);
        if matches.is_empty() {
            self.status_message = Some(format!("no command matching '{query}'"));
            return;
        }

        let chosen = if let Some(index) = selected_index {
            matches[index.min(matches.len() - 1)]
        } else if let Ok(index) = query.parse::<usize>() {
            if index == 0 || index > matches.len() {
                self.status_message = Some(format!("command index out of range: {index}"));
                return;
            }
            matches[index - 1]
        } else if query.is_empty() {
            matches[0]
        } else {
            let lowered = query.to_ascii_lowercase();
            matches
                .iter()
                .copied()
                .find(|cmd| {
                    cmd.id() == lowered
                        || cmd.label() == lowered
                        || cmd.aliases().iter().any(|alias| *alias == lowered)
                })
                .unwrap_or(matches[0])
        };

        self.execute_palette_command(chosen);
    }

    fn execute_palette_command(&mut self, command: PaletteCommand) {
        match command {
            PaletteCommand::OpenFile => self.begin_open_file_flow(),
            PaletteCommand::OpenRecent => self.begin_open_recent_flow(),
            PaletteCommand::Save => self.save_current_document(),
            PaletteCommand::SaveAs => self.begin_save_as_prompt(),
            PaletteCommand::NewTab => self.new_tab(),
            PaletteCommand::NextTab => self.switch_tab(1),
            PaletteCommand::PrevTab => self.switch_tab(-1),
            PaletteCommand::Find => self.open_find_prompt(),
            PaletteCommand::FindNext => self.repeat_find_or_prompt(false),
            PaletteCommand::FindPrev => self.repeat_find_or_prompt(true),
            PaletteCommand::GoToLine => {
                self.prompt = Some(PromptState::go_to_line(self.document.cursor().line + 1))
            }
            PaletteCommand::Copy => self.copy_selection_to_clipboard(),
            PaletteCommand::Cut => self.cut_selection_to_clipboard(),
            PaletteCommand::Paste => self.request_clipboard_paste(self.clipboard_atoms.clipboard),
            PaletteCommand::Quit => self.request_quit(),
        }
    }

    fn go_to_line_prompt_submit(&mut self, raw_input: &str) {
        if raw_input.is_empty() {
            self.status_message = Some("line is empty".to_string());
            return;
        }

        let mut parts = raw_input.splitn(2, ':');
        let line_part = parts.next().unwrap_or_default().trim();
        let col_part = parts.next().map(str::trim);

        let line_num = match line_part.parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => {
                self.status_message = Some(format!("invalid line: {line_part}"));
                return;
            }
        };

        let col_num = match col_part {
            Some("") | None => 1usize,
            Some(raw_col) => match raw_col.parse::<usize>() {
                Ok(value) if value > 0 => value,
                _ => {
                    self.status_message = Some(format!("invalid column: {raw_col}"));
                    return;
                }
            },
        };

        let line_idx = line_num
            .saturating_sub(1)
            .min(self.document.line_count() - 1);
        let max_col = self
            .document
            .line(line_idx)
            .map(|line| line.chars().count())
            .unwrap_or(0);
        let col_idx = col_num.saturating_sub(1).min(max_col);

        self.document.clear_selection();
        self.document.set_cursor(Position::new(line_idx, col_idx));
        self.ensure_cursor_visible();
        self.status_message = Some(format!("jumped to {}:{}", line_idx + 1, col_idx + 1));
    }

    fn find_next_match(&mut self, reverse: bool) -> bool {
        let Some(search) = self.search.as_ref() else {
            return false;
        };
        if search.query.is_empty() {
            return false;
        }

        let query = search.query.clone();
        let from = if reverse {
            self.search
                .as_ref()
                .and_then(|state| state.last_match.map(|m| m.start))
                .unwrap_or_else(|| self.document.cursor())
        } else {
            self.search
                .as_ref()
                .and_then(|state| state.last_match.map(|m| m.end))
                .unwrap_or_else(|| self.document.cursor())
        };

        let next = if reverse {
            self.find_match_reverse(&query, from)
                .or_else(|| self.find_match_reverse(&query, self.document_end_position()))
        } else {
            self.find_match_forward(&query, from)
                .or_else(|| self.find_match_forward(&query, Position::origin()))
        };

        let Some(found) = next else {
            self.status_message = Some(format!("no match for '{}'", query));
            return false;
        };

        self.document.set_cursor(found.end);
        self.document
            .set_selection(Some(Selection::new(found.start, found.end)));
        self.ensure_cursor_visible();
        self.status_message = Some(format!(
            "match {}:{} for '{}'",
            found.start.line + 1,
            found.start.column + 1,
            query
        ));

        if let Some(search) = self.search.as_mut() {
            search.last_match = Some(found);
        }
        true
    }

    fn document_end_position(&self) -> Position {
        let line = self.document.line_count().saturating_sub(1);
        let column = self
            .document
            .line(line)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        Position::new(line, column)
    }

    fn find_match_forward(&self, query: &str, from: Position) -> Option<SearchMatch> {
        if query.is_empty() {
            return None;
        }
        let query_chars = query.chars().count();

        for line_idx in from.line..self.document.line_count() {
            let line = self.document.line(line_idx).unwrap_or("");
            let start_col = if line_idx == from.line {
                from.column
            } else {
                0
            };
            let start_byte = column_to_byte_idx(line, start_col);
            if start_byte >= line.len() {
                continue;
            }

            let hay = &line[start_byte..];
            if let Some(rel_idx) = hay.find(query) {
                let start_byte_match = start_byte + rel_idx;
                let start_col_match = byte_to_column_idx(line, start_byte_match);
                let end_col_match = start_col_match + query_chars;
                return Some(SearchMatch {
                    start: Position::new(line_idx, start_col_match),
                    end: Position::new(line_idx, end_col_match),
                });
            }
        }
        None
    }

    fn find_match_reverse(&self, query: &str, from: Position) -> Option<SearchMatch> {
        if query.is_empty() {
            return None;
        }
        let query_chars = query.chars().count();

        for line_idx in (0..=from.line).rev() {
            let line = self.document.line(line_idx).unwrap_or("");
            let end_col = if line_idx == from.line {
                from.column
            } else {
                line.chars().count()
            };
            let end_byte = column_to_byte_idx(line, end_col);
            let hay = &line[..end_byte.min(line.len())];
            if let Some(start_byte_match) = hay.rfind(query) {
                let start_col_match = byte_to_column_idx(line, start_byte_match);
                let end_col_match = start_col_match + query_chars;
                return Some(SearchMatch {
                    start: Position::new(line_idx, start_col_match),
                    end: Position::new(line_idx, end_col_match),
                });
            }
        }
        None
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
        if pointer_y < TAB_BAR_HEIGHT {
            let tab_index = (pointer_x.max(0) / TAB_WIDTH) as usize;
            if tab_index < self.tabs.len() {
                self.activate_tab(tab_index);
            }
            return;
        }

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
        TAB_BAR_HEIGHT + self.theme.padding as i32
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

    fn render_tab_bar(&mut self) -> Result<()> {
        let size = self.renderer.size();
        let tab_height = TAB_BAR_HEIGHT as u32;

        self.renderer.fill_rect(
            Rect::new(0, 0, size.width, tab_height),
            self.theme.input_background.darken(0.1),
        )?;

        let style = TextStyle::new()
            .font_family(&self.theme.font_family)
            .font_size((self.theme.font_size - 1.0).max(10.0))
            .color(self.theme.foreground)
            .ellipsize(true)
            .max_width((TAB_WIDTH - 16).max(50));

        let mut x = 0i32;
        for idx in 0..self.tabs.len() {
            if x >= size.width as i32 {
                break;
            }
            let width = (size.width as i32 - x).min(TAB_WIDTH).max(1) as u32;
            let active = idx == self.active_tab;
            let bg = if active {
                self.theme.item_selected_background
            } else {
                self.theme.item_background
            };
            self.renderer
                .fill_rect(Rect::new(x, 0, width, tab_height), bg)?;
            self.renderer.line(
                x as f64,
                0.0,
                x as f64,
                TAB_BAR_HEIGHT as f64,
                self.theme.border.with_alpha(0.7),
                1.0,
            )?;

            let title = self.tab_title(idx);
            self.renderer.text(&title, (x + 8) as f64, 7.0, &style)?;

            x += TAB_WIDTH;
        }

        self.renderer.line(
            0.0,
            TAB_BAR_HEIGHT as f64,
            size.width as f64,
            TAB_BAR_HEIGHT as f64,
            self.theme.border,
            1.0,
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
        self.render_tab_bar()?;

        if gutter_width > 0 {
            self.renderer.fill_rect(
                Rect::new(
                    0,
                    TAB_BAR_HEIGHT,
                    gutter_width as u32,
                    (status_top - TAB_BAR_HEIGHT).max(0) as u32,
                ),
                self.theme.input_background,
            )?;
            self.renderer.line(
                gutter_width as f64,
                TAB_BAR_HEIGHT as f64,
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
            let mut text = format!(
                "[tab {}/{}] {file_display}{dirty_marker}",
                self.active_tab + 1,
                self.tabs.len()
            );
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
            PromptKind::FindQuery => format!(
                "find: {}_  |  Enter next  Shift+Enter prev  Esc close",
                prompt.input
            ),
            PromptKind::GoToLine => format!("go to line[:col]: {}_", prompt.input),
            PromptKind::OpenRecent => {
                let preview = self
                    .recent_files
                    .iter()
                    .take(5)
                    .enumerate()
                    .map(|(idx, path)| format!("[{}] {}", idx + 1, path.display()))
                    .collect::<Vec<_>>()
                    .join("  ");
                if preview.is_empty() {
                    format!("open recent (none): {}_", prompt.input)
                } else {
                    format!("open recent {}  |  pick #: {}_", preview, prompt.input)
                }
            }
            PromptKind::CommandPalette => {
                let commands = self.matching_palette_commands(&prompt.input);
                if commands.is_empty() {
                    format!("command: {}_  |  no matches", prompt.input)
                } else {
                    let selected = prompt.selection_index.min(commands.len() - 1);
                    let mut start = selected.saturating_sub(MAX_PALETTE_PREVIEW / 2);
                    if start + MAX_PALETTE_PREVIEW > commands.len() {
                        start = commands.len().saturating_sub(MAX_PALETTE_PREVIEW);
                    }
                    let preview = commands
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(MAX_PALETTE_PREVIEW)
                        .map(|(idx, cmd)| {
                            let marker = if idx == selected { ">" } else { " " };
                            format!("{}[{}] {}", marker, idx + 1, cmd.label())
                        })
                        .collect::<Vec<_>>()
                        .join("  ");
                    format!(
                        "command: {}_  |  {}  |  Enter run  Up/Down move  Tab next  Esc close",
                        prompt.input, preview
                    )
                }
            }
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
        let _ = self.persist_session_snapshot();
        if let Some(path) = self.ipc_socket_path.as_ref() {
            let _ = fs::remove_file(path);
        }
        let _ = self.window.connection().inner().free_gc(self.gc);
    }
}

fn resolve_theme(name: &str) -> Theme {
    match name {
        "light" => Theme::light(),
        "high-contrast" => Theme::high_contrast(),
        _ => Theme::dark(),
    }
}

fn is_safe_autosave_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..")
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

fn column_to_byte_idx(line: &str, column: usize) -> usize {
    if column == 0 {
        return 0;
    }
    line.char_indices()
        .nth(column)
        .map(|(idx, _)| idx)
        .unwrap_or(line.len())
}

fn byte_to_column_idx(line: &str, byte_idx: usize) -> usize {
    line[..byte_idx.min(line.len())].chars().count()
}

fn is_fuzzy_subsequence(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }

    let mut chars = needle.chars();
    let mut current = chars.next();
    for candidate in haystack.chars() {
        let Some(target) = current else {
            return true;
        };
        if candidate == target {
            current = chars.next();
        }
    }
    current.is_none()
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
            return normalize_path_for_match(&home.join(rest));
        }
    }
    normalize_path_for_match(&PathBuf::from(trimmed))
}

fn normalize_path_for_match(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}
