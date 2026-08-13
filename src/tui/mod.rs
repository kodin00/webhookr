//! Interactive management TUI for webhookr.
//!
//! A menu-driven terminal UI for browsing, adding, editing, deleting,
//! triggering, and inspecting the logs of webhook projects. It reads and
//! writes the shared [`config`] file directly, so changes made here are picked
//! up by the daemon on the next webhook.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::{self, AppConfig, ProjectConfig};
use crate::state;
use crate::util;

// ----- palette -----------------------------------------------------------

const ACCENT: Color = Color::LightCyan;
const MUTED: Color = Color::DarkGray;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::LightRed;
const TEXT: Color = Color::Gray;

// ----- menu --------------------------------------------------------------

const MENU: [(&str, &str, &str); 7] = [
    ("+", "Add project", "Register a checkout and deploy command"),
    ("=", "List projects", "Inspect configured webhook routes"),
    ("~", "Edit project", "Change a route or deploy command"),
    ("@", "Show secret", "Reveal or rotate webhook credentials"),
    (">", "Run project", "Trigger a deployment manually"),
    ("-", "Remove project", "Delete a configured route"),
    ("#", "View run log", "Read output from the latest run"),
];

/// Launch the interactive management TUI (blocks until the user quits).
pub fn run() -> Result<()> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    // Restore the terminal no matter how we exit (normal, error, or panic).
    let _guard = Cleanup;
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    run_loop(&mut terminal)
}

/// RAII guard that restores the terminal on drop, so a clean exit is guaranteed
/// even when the loop bails out early with an error.
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let config = config::load_config()?;
    let mut app = App::new(config);

    loop {
        // Refresh last-run status so the list stays current.
        app.refresh_runs();

        terminal.draw(|f| app.render(f))?;

        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // Ctrl-C always quits, as a friendly escape hatch.
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    if app.on_key(key.code)? {
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

// ----- screens -----------------------------------------------------------

/// Which screen is currently shown.
enum Screen {
    Menu,
    List { mode: ListMode },
    Wizard(Wizard),
    ConfirmDelete { index: usize },
    Key { index: usize },
    Log { text: String, scroll: u16 },
}

/// What the project-list screen does when a project is selected.
#[derive(Clone, Copy)]
enum ListMode {
    Browse,
    Edit,
    Run,
    Remove,
    Key,
    Log,
}

// ----- wizard ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Name,
    Path,
    Branch,
    Command,
    Verify,
    Confirm,
}

/// Step-by-step add/edit flow. The `id` is never edited by hand: it is derived
/// from the name on add, and immutable on edit.
struct Wizard {
    editing: Option<usize>,
    step: Step,
    name: Input,
    path: String,
    branch: Input,
    command: Input,
    verify: usize, // 0 = github, 1 = token
    browser: Option<DirBrowser>,
    error: Option<String>,
    secret: String,
}

impl Wizard {
    fn new_add() -> Self {
        Self {
            editing: None,
            step: Step::Name,
            name: Input::new(""),
            path: String::new(),
            branch: Input::new("main"),
            command: Input::new(""),
            verify: 0,
            browser: None,
            error: None,
            secret: util::generate_secret(),
        }
    }

    fn new_edit(p: &ProjectConfig, index: usize) -> Self {
        Self {
            editing: Some(index),
            step: Step::Name,
            name: Input::new(&p.name),
            path: p.path.clone(),
            branch: Input::new(&p.branch),
            command: Input::new(&p.command),
            verify: if p.verify_mode == "token" { 1 } else { 0 },
            browser: None,
            error: None,
            secret: p.secret.clone(),
        }
    }
}

// ----- text input --------------------------------------------------------

struct Input {
    buf: String,
    cursor: usize,
}

impl Input {
    fn new(s: &str) -> Self {
        Self {
            buf: s.to_string(),
            cursor: s.chars().count(),
        }
    }

    fn value(&self) -> &str {
        &self.buf
    }

    fn insert(&mut self, c: char) {
        let at = self.cursor;
        let mut out = String::with_capacity(self.buf.len() + c.len_utf8());
        let mut i = 0;
        let mut done = false;
        for ch in self.buf.chars() {
            if i == at {
                out.push(c);
                done = true;
            }
            out.push(ch);
            i += 1;
        }
        if !done {
            out.push(c);
        }
        self.buf = out;
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.cursor - 1;
        let mut out = String::new();
        let mut i = 0;
        for ch in self.buf.chars() {
            if i != at {
                out.push(ch);
            }
            i += 1;
        }
        self.buf = out;
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        let at = self.cursor;
        if at >= self.buf.chars().count() {
            return;
        }
        let mut out = String::new();
        let mut i = 0;
        for ch in self.buf.chars() {
            if i != at {
                out.push(ch);
            }
            i += 1;
        }
        self.buf = out;
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        let n = self.buf.chars().count();
        if self.cursor < n {
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.buf.chars().count();
    }
}

// ----- directory browser -------------------------------------------------

enum DirEntry {
    /// "use this folder" — confirm the current directory.
    Current,
    /// ".." — go up one level.
    Up,
    /// A subdirectory to descend into.
    Dir(PathBuf),
}

enum EnterResult {
    Stay,
    Choose,
}

struct DirBrowser {
    cwd: PathBuf,
    entries: Vec<DirEntry>,
    selected: usize,
    error: Option<String>,
}

impl DirBrowser {
    fn new(start: &str) -> Self {
        let cwd = if start.is_empty() {
            home_or_root()
        } else {
            let p = PathBuf::from(start);
            if p.is_dir() {
                p
            } else {
                home_or_root()
            }
        };
        let mut b = Self {
            cwd,
            entries: Vec::new(),
            selected: 0,
            error: None,
        };
        b.refresh();
        b
    }

    fn refresh(&mut self) {
        self.entries.clear();
        self.entries.push(DirEntry::Current);
        if self.cwd.parent().is_some() {
            self.entries.push(DirEntry::Up);
        }
        let mut dirs: Vec<PathBuf> = match fs::read_dir(&self.cwd) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect(),
            Err(e) => {
                self.error = Some(format!("cannot read {}: {e}", self.cwd.display()));
                self.selected = 0;
                return;
            }
        };
        dirs.sort_by_key(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        });
        for d in dirs {
            self.entries.push(DirEntry::Dir(d));
        }
        self.error = None;
        let first_dir = self
            .entries
            .iter()
            .position(|e| matches!(e, DirEntry::Dir(_)))
            .unwrap_or(0);
        self.selected = first_dir;
    }

    fn up(&mut self) {
        if let Some(parent) = self.cwd.parent() {
            let parent = parent.to_path_buf();
            if parent != self.cwd {
                self.cwd = parent;
                self.refresh();
            }
        }
    }

    fn jump(&mut self, p: PathBuf) {
        if p.is_dir() {
            self.cwd = p;
            self.refresh();
        }
    }

    fn move_sel(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
    }

    fn enter(&mut self) -> EnterResult {
        match self.entries.get(self.selected) {
            Some(DirEntry::Current) => EnterResult::Choose,
            Some(DirEntry::Up) => {
                self.up();
                EnterResult::Stay
            }
            Some(DirEntry::Dir(d)) => {
                self.cwd = d.clone();
                self.refresh();
                EnterResult::Stay
            }
            None => EnterResult::Stay,
        }
    }
}

fn home_or_root() -> PathBuf {
    dirs::home_dir()
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

// ----- app ---------------------------------------------------------------

struct App {
    config: AppConfig,
    screen: Screen,
    menu_selected: usize,
    list_state: ListState,
    last_msg: Option<String>,
    last_runs: HashMap<String, state::RunRecord>,
}

impl App {
    fn new(config: AppConfig) -> Self {
        let mut app = Self {
            config,
            screen: Screen::Menu,
            menu_selected: 0,
            list_state: ListState::default(),
            last_msg: None,
            last_runs: HashMap::new(),
        };
        if !app.config.projects.is_empty() {
            app.list_state.select(Some(0));
        }
        app.refresh_runs();
        app
    }

    /// Reload run history and index the newest run per project.
    fn refresh_runs(&mut self) {
        let mut map = HashMap::new();
        // `load_runs` is sorted newest-first, so the first entry per project wins.
        for run in state::load_runs() {
            map.entry(run.project_id.clone()).or_insert(run);
        }
        self.last_runs = map;
    }

    fn status_for(&self, id: &str) -> &'static str {
        match self.last_runs.get(id).map(|r| r.status.as_str()) {
            Some("success") => "[ok] success",
            Some("failed") => "[!!] failed",
            Some("running") => "[>>] running",
            Some(_) => "[??] unknown",
            None => "[--] never",
        }
    }

    fn selected_index(&self) -> Option<usize> {
        if self.config.projects.is_empty() {
            None
        } else {
            Some(self.list_state.selected().unwrap_or(0))
        }
    }

    fn select_next(&mut self) {
        if self.config.projects.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(s) => (s + 1) % self.config.projects.len(),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        if self.config.projects.is_empty() {
            return;
        }
        let len = self.config.projects.len();
        let i = match self.list_state.selected() {
            Some(0) | None => len - 1,
            Some(s) => s - 1,
        };
        self.list_state.select(Some(i));
    }

    // ----- key dispatch -------------------------------------------------

    /// Returns `Ok(true)` when the app should quit.
    fn on_key(&mut self, key: KeyCode) -> Result<bool> {
        match std::mem::replace(&mut self.screen, Screen::Menu) {
            Screen::Menu => Ok(self.handle_menu(key)),
            Screen::List { mode } => {
                self.handle_list(key, mode)?;
                Ok(false)
            }
            Screen::Wizard(w) => {
                self.handle_wizard(key, w)?;
                Ok(false)
            }
            Screen::ConfirmDelete { index } => {
                self.handle_confirm(key, index)?;
                Ok(false)
            }
            Screen::Key { index } => {
                self.handle_key(key, index)?;
                Ok(false)
            }
            Screen::Log { text, scroll } => {
                self.handle_log(key, text, scroll);
                Ok(false)
            }
        }
    }

    fn handle_menu(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Char('q') => return true,
            KeyCode::Char('j') | KeyCode::Down => {
                self.menu_selected = (self.menu_selected + 1) % MENU.len();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.menu_selected = (self.menu_selected + MENU.len() - 1) % MENU.len();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(n) = c.to_digit(10) {
                    let n = n as usize;
                    if (1..=MENU.len()).contains(&n) {
                        self.menu_selected = n - 1;
                        self.activate_menu(n - 1);
                    }
                }
            }
            KeyCode::Enter => self.activate_menu(self.menu_selected),
            _ => {}
        }
        false
    }

    fn activate_menu(&mut self, i: usize) {
        match i {
            0 => self.screen = Screen::Wizard(Wizard::new_add()),
            1 => self.open_list(ListMode::Browse),
            2 => self.open_list(ListMode::Edit),
            3 => self.open_list(ListMode::Key),
            4 => self.open_list(ListMode::Run),
            5 => self.open_list(ListMode::Remove),
            6 => self.open_list(ListMode::Log),
            _ => {}
        }
    }

    fn open_list(&mut self, mode: ListMode) {
        if !self.config.projects.is_empty() && self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
        }
        self.screen = Screen::List { mode };
    }

    fn handle_list(&mut self, key: KeyCode, mode: ListMode) -> Result<()> {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Menu,
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Enter => self.activate_project(mode),
            _ => {}
        }
        Ok(())
    }

    fn activate_project(&mut self, mode: ListMode) {
        let Some(i) = self.selected_index() else {
            return;
        };
        match mode {
            ListMode::Browse => {}
            ListMode::Edit => {
                let w = Wizard::new_edit(&self.config.projects[i], i);
                self.screen = Screen::Wizard(w);
            }
            ListMode::Run => {
                self.trigger_run(i);
                self.screen = Screen::Menu;
            }
            ListMode::Remove => self.screen = Screen::ConfirmDelete { index: i },
            ListMode::Key => self.screen = Screen::Key { index: i },
            ListMode::Log => self.open_log(i),
        }
    }

    fn trigger_run(&mut self, i: usize) {
        let p = &self.config.projects[i];
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .args(["run", "--id", p.id.as_str()])
                .spawn();
        }
        self.last_msg = Some(format!("triggered {} — check the run log", p.id));
    }

    fn open_log(&mut self, i: usize) {
        let p = &self.config.projects[i];
        let text = match state::latest_run(&p.id) {
            Some(run) => state::read_run_log(&run.id),
            None => String::new(),
        };
        let text = if text.trim().is_empty() {
            format!("No runs for {} yet.", p.id)
        } else {
            text
        };
        self.screen = Screen::Log { text, scroll: 0 };
    }

    // ----- wizard handling ----------------------------------------------

    fn handle_wizard(&mut self, key: KeyCode, mut w: Wizard) -> Result<()> {
        // A directory browser is open: it owns the keys.
        if w.browser.is_some() {
            match key {
                KeyCode::Esc => {
                    w.browser = None;
                    w.step = Step::Name;
                }
                KeyCode::Char('c') => self.finish_path(&mut w),
                KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                    w.browser.as_mut().unwrap().up();
                }
                KeyCode::Char('j') | KeyCode::Down => w.browser.as_mut().unwrap().move_sel(1),
                KeyCode::Char('k') | KeyCode::Up => w.browser.as_mut().unwrap().move_sel(-1),
                KeyCode::Char('/') => w.browser.as_mut().unwrap().jump(PathBuf::from("/")),
                KeyCode::Char('~') => {
                    if let Some(h) = dirs::home_dir() {
                        w.browser.as_mut().unwrap().jump(h);
                    }
                }
                KeyCode::Enter => {
                    if let EnterResult::Choose = w.browser.as_mut().unwrap().enter() {
                        self.finish_path(&mut w);
                    }
                }
                _ => {}
            }
            self.screen = Screen::Wizard(w);
            return Ok(());
        }

        match w.step {
            Step::Name => match key {
                KeyCode::Esc => {
                    self.screen = Screen::Menu;
                    return Ok(());
                }
                KeyCode::Enter => {
                    if w.name.value().trim().is_empty() {
                        w.error = Some("name is required".to_string());
                    } else {
                        w.error = None;
                        self.open_browser(&mut w);
                    }
                }
                KeyCode::Char(c) => w.name.insert(c),
                KeyCode::Backspace => w.name.backspace(),
                KeyCode::Delete => w.name.delete(),
                KeyCode::Left => w.name.left(),
                KeyCode::Right => w.name.right(),
                KeyCode::Home => w.name.home(),
                KeyCode::End => w.name.end(),
                _ => {}
            },
            Step::Path => self.open_browser(&mut w),
            Step::Branch => match key {
                KeyCode::Esc => self.open_browser(&mut w),
                KeyCode::Enter => {
                    w.error = None;
                    w.step = Step::Command;
                }
                KeyCode::Char(c) => w.branch.insert(c),
                KeyCode::Backspace => w.branch.backspace(),
                KeyCode::Delete => w.branch.delete(),
                KeyCode::Left => w.branch.left(),
                KeyCode::Right => w.branch.right(),
                KeyCode::Home => w.branch.home(),
                KeyCode::End => w.branch.end(),
                _ => {}
            },
            Step::Command => match key {
                KeyCode::Esc => w.step = Step::Branch,
                KeyCode::Enter => {
                    w.error = None;
                    w.step = Step::Verify;
                }
                KeyCode::Char(c) => w.command.insert(c),
                KeyCode::Backspace => w.command.backspace(),
                KeyCode::Delete => w.command.delete(),
                KeyCode::Left => w.command.left(),
                KeyCode::Right => w.command.right(),
                KeyCode::Home => w.command.home(),
                KeyCode::End => w.command.end(),
                _ => {}
            },
            Step::Verify => match key {
                KeyCode::Esc => w.step = Step::Command,
                KeyCode::Char('j')
                | KeyCode::Down
                | KeyCode::Right
                | KeyCode::Char('k')
                | KeyCode::Up
                | KeyCode::Left => {
                    w.verify = (w.verify + 1) % 2;
                }
                KeyCode::Enter => {
                    w.error = None;
                    w.step = Step::Confirm;
                }
                _ => {}
            },
            Step::Confirm => match key {
                KeyCode::Esc => w.step = Step::Verify,
                KeyCode::Enter => {
                    let name = w.name.value().trim().to_string();
                    let id = match w.editing {
                        Some(i) => self.config.projects[i].id.clone(),
                        None => slugify(&name),
                    };
                    let verify_mode = if w.verify == 0 {
                        "github".to_string()
                    } else {
                        "token".to_string()
                    };
                    let project = ProjectConfig {
                        id,
                        name,
                        path: w.path.trim().to_string(),
                        branch: w.branch.value().trim().to_string(),
                        command: w.command.value().trim().to_string(),
                        secret: w.secret.clone(),
                        verify_mode,
                    };
                    match project.validate() {
                        Ok(()) => {
                            self.config.upsert(project);
                            config::save_config(&self.config)?;
                            self.last_msg = Some(if w.editing.is_some() {
                                "project saved".to_string()
                            } else {
                                "project added".to_string()
                            });
                            self.screen = Screen::Menu;
                        }
                        Err(e) => {
                            w.error = Some(format!("{e:#}"));
                            self.screen = Screen::Wizard(w);
                        }
                    }
                    return Ok(());
                }
                _ => {}
            },
        }
        self.screen = Screen::Wizard(w);
        Ok(())
    }

    /// Open the directory browser for the `path` field and move to `Step::Path`.
    fn open_browser(&mut self, w: &mut Wizard) {
        let start = w.path.clone();
        let mut b = DirBrowser::new(&start);
        if !start.trim().is_empty() {
            // Existing path: preselect "use this folder" so Enter keeps it.
            b.selected = 0;
        }
        w.browser = Some(b);
        w.step = Step::Path;
    }

    /// Accept the browser's current directory as the path and advance.
    fn finish_path(&mut self, w: &mut Wizard) {
        if let Some(b) = w.browser.take() {
            w.path = b.cwd.to_string_lossy().to_string();
        }
        w.error = None;
        w.step = Step::Branch;
    }

    // ----- confirm-delete handling --------------------------------------

    fn handle_confirm(&mut self, key: KeyCode, index: usize) -> Result<()> {
        match key {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let id = self.config.projects[index].id.clone();
                self.config.remove(&id);
                config::save_config(&self.config)?;
                self.last_msg = Some(format!("deleted {id}"));
                self.list_state.select(if self.config.projects.is_empty() {
                    None
                } else {
                    Some(index.min(self.config.projects.len() - 1))
                });
                self.screen = Screen::Menu;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.screen = Screen::Menu;
            }
            _ => self.screen = Screen::ConfirmDelete { index },
        }
        Ok(())
    }

    // ----- key (secret) handling ----------------------------------------

    fn handle_key(&mut self, key: KeyCode, index: usize) -> Result<()> {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Menu,
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.config.projects[index].secret = util::generate_secret();
                config::save_config(&self.config)?;
                self.last_msg = Some("secret rotated".to_string());
                self.screen = Screen::Key { index };
            }
            _ => self.screen = Screen::Key { index },
        }
        Ok(())
    }

    // ----- log handling --------------------------------------------------

    fn handle_log(&mut self, key: KeyCode, text: String, scroll: u16) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.screen = Screen::Menu,
            KeyCode::Char('j') | KeyCode::Down => {
                let max = (text.lines().count() as u16).saturating_sub(1);
                self.screen = Screen::Log {
                    text,
                    scroll: (scroll + 1).min(max),
                };
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.screen = Screen::Log {
                    text,
                    scroll: scroll.saturating_sub(1),
                };
            }
            _ => self.screen = Screen::Log { text, scroll },
        }
    }

    // ----- rendering -----------------------------------------------------

    fn render(&mut self, f: &mut Frame) {
        match std::mem::replace(&mut self.screen, Screen::Menu) {
            Screen::Menu => {
                self.render_menu(f);
                self.screen = Screen::Menu;
            }
            Screen::List { mode } => {
                self.render_list(f, mode);
                self.screen = Screen::List { mode };
            }
            Screen::Wizard(w) => {
                self.render_wizard(f, &w);
                self.screen = Screen::Wizard(w);
            }
            Screen::ConfirmDelete { index } => {
                self.render_confirm(f, index);
                self.screen = Screen::ConfirmDelete { index };
            }
            Screen::Key { index } => {
                self.render_key(f, index);
                self.screen = Screen::Key { index };
            }
            Screen::Log { text, scroll } => {
                self.render_log(f, &text, scroll);
                self.screen = Screen::Log { text, scroll };
            }
        }
    }

    fn render_menu(&self, f: &mut Frame) {
        let area = f.area().inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Min(7),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        let banner = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "WEBHOOKR",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" // CONTROL PLANE", Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("listen  ", Style::default().fg(MUTED)),
                Span::styled(self.config.listen_addr.clone(), Style::default().fg(WARN)),
                Span::styled("    routes  ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{:02}", self.config.projects.len()),
                    Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
                ),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " webhook routing console ",
                    Style::default().fg(ACCENT),
                ))
                .border_style(Style::default().fg(MUTED)),
        );
        f.render_widget(banner, chunks[0]);

        let items: Vec<ListItem> = MENU
            .iter()
            .enumerate()
            .map(|(i, (mark, label, description))| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>2}  ", i + 1), Style::default().fg(MUTED)),
                    Span::styled(
                        format!("[{mark}]"),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {label:<18}"), Style::default().fg(TEXT)),
                    Span::styled(*description, Style::default().fg(MUTED)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT)
                    .title(Span::styled(" actions ", Style::default().fg(MUTED)))
                    .border_style(Style::default().fg(MUTED)),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(Some(self.menu_selected));
        f.render_stateful_widget(list, chunks[1], &mut state);

        if let Some(m) = &self.last_msg {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("* ", Style::default().fg(GOOD).add_modifier(Modifier::BOLD)),
                    Span::styled(m.as_str(), Style::default().fg(GOOD)),
                ])),
                chunks[2],
            );
        }
        f.render_widget(
            Paragraph::new(key_hints(&[
                ("j/k", "move"),
                ("enter", "select"),
                ("1-7", "jump"),
                ("q", "quit"),
            ])),
            chunks[3],
        );
    }

    fn render_list(&mut self, f: &mut Frame, mode: ListMode) {
        let title = match mode {
            ListMode::Browse => " routes // inspect ",
            ListMode::Edit => " routes // select to edit ",
            ListMode::Run => " routes // select to run ",
            ListMode::Remove => " routes // select to remove ",
            ListMode::Key => " routes // select for secret ",
            ListMode::Log => " routes // select for log ",
        };

        let area = f.area().inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        let h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(v[0]);

        let items: Vec<ListItem> = if self.config.projects.is_empty() {
            vec![ListItem::new(Line::from(vec![
                Span::styled("[--] ", Style::default().fg(MUTED)),
                Span::styled("No routes configured", Style::default().fg(TEXT)),
            ]))]
        } else {
            self.config
                .projects
                .iter()
                .map(|p| {
                    let status = self.status_for(&p.id);
                    ListItem::new(Line::from(vec![
                        Span::styled("/", Style::default().fg(MUTED)),
                        Span::styled(
                            p.id.as_str(),
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("  ", Style::default()),
                        Span::styled(p.name.as_str(), Style::default().fg(TEXT)),
                        Span::styled("  ", Style::default()),
                        Span::styled(status, Style::default().fg(status_color(status))),
                    ]))
                })
                .collect()
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(MUTED)),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        f.render_stateful_widget(list, h[0], &mut self.list_state);

        let detail = Paragraph::new(self.detail_text())
            .style(Style::default().fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(" route detail ", Style::default().fg(ACCENT)))
                    .border_style(Style::default().fg(MUTED)),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(detail, h[1]);

        let hint = match mode {
            ListMode::Browse => vec![("j/k", "move"), ("esc", "back")],
            ListMode::Edit => vec![("enter", "edit"), ("esc", "back")],
            ListMode::Run => vec![("enter", "run"), ("esc", "back")],
            ListMode::Remove => vec![("enter", "remove"), ("esc", "back")],
            ListMode::Key => vec![("enter", "show secret"), ("esc", "back")],
            ListMode::Log => vec![("enter", "view log"), ("esc", "back")],
        };
        f.render_widget(Paragraph::new(key_hints(&hint)), v[1]);
    }

    fn detail_text(&self) -> Vec<Line<'static>> {
        let Some(i) = self.selected_index() else {
            return vec![Line::from(Span::styled(
                "Select a route to inspect its configuration.",
                Style::default().fg(MUTED),
            ))];
        };
        let p = &self.config.projects[i];
        let mut lines = vec![
            field_line("id", &p.id),
            field_line("name", &p.name),
            field_line("path", &p.path),
            field_line("branch", &p.branch),
            field_line("command", &p.command),
            field_line("verify", &p.verify_mode),
            field_line(
                "webhook",
                &format!("http://{}/hooks/{}", self.config.listen_addr, p.id),
            ),
        ];
        let last_line = match self.last_runs.get(&p.id) {
            Some(r) => {
                let s = format!("{} ({}ms) {}", r.status, r.duration_ms, r.message);
                field_line("last run", &s)
            }
            None => field_line("last run", "never"),
        };
        lines.push(last_line);
        lines
    }

    fn render_wizard(&self, f: &mut Frame, w: &Wizard) {
        let area = f.area().inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        let title = if w.editing.is_some() {
            "EDIT ROUTE"
        } else {
            "ADD ROUTE"
        };
        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                title,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            step_progress(w),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " setup // guided ",
                    Style::default().fg(MUTED),
                ))
                .border_style(Style::default().fg(MUTED)),
        );
        f.render_widget(header, v[0]);

        match w.step {
            Step::Name => {
                let id = slugify(w.name.value());
                self.render_text_body(
                    f,
                    v[1],
                    "Name",
                    w.name.value(),
                    w.name.cursor,
                    Some(format!("id: {id}")),
                );
            }
            Step::Path => {
                if let Some(b) = &w.browser {
                    self.render_browser(f, v[1], b);
                }
            }
            Step::Branch => self.render_text_body(
                f,
                v[1],
                "Branch",
                w.branch.value(),
                w.branch.cursor,
                Some("git branch to pull before running the command".to_string()),
            ),
            Step::Command => self.render_text_body(
                f,
                v[1],
                "Command",
                w.command.value(),
                w.command.cursor,
                Some("shell command to run after pull, e.g. ./deploy.sh".to_string()),
            ),
            Step::Verify => self.render_verify(f, v[1], w),
            Step::Confirm => self.render_confirm_wizard(f, v[1], w),
        }

        if let Some(e) = &w.error {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("! ", Style::default().fg(BAD).add_modifier(Modifier::BOLD)),
                    Span::styled(e.as_str(), Style::default().fg(BAD)),
                ])),
                v[2],
            );
        }
        f.render_widget(Paragraph::new(wizard_hints(w)), v[3]);
    }

    fn render_text_body(
        &self,
        f: &mut Frame,
        area: Rect,
        label: &str,
        value: &str,
        cursor: usize,
        sub: Option<String>,
    ) {
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                label.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            input_line(value, cursor),
        ];
        if let Some(s) = sub {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(s, Style::default().fg(MUTED))));
        }
        let para = Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" input // {} ", label.to_ascii_lowercase()))
                    .border_style(Style::default().fg(MUTED)),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(para, area);
    }

    fn render_verify(&self, f: &mut Frame, area: Rect, w: &Wizard) {
        let options = [
            ("github", "verify X-Hub-Signature-256 (GitHub webhooks)"),
            ("token", "verify X-Webhookr-Key header (any sender)"),
        ];
        let items: Vec<ListItem> = options
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                let mark = if i == w.verify { "[x] " } else { "[ ] " };
                let style = if i == w.verify {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark}{name}"), style),
                    Span::raw("   "),
                    Span::styled(*desc, Style::default().fg(MUTED)),
                ]))
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" verification ", Style::default().fg(ACCENT)))
                .border_style(Style::default().fg(MUTED)),
        );
        f.render_widget(list, area);
    }

    fn render_confirm_wizard(&self, f: &mut Frame, area: Rect, w: &Wizard) {
        let id = match w.editing {
            Some(i) => self.config.projects[i].id.clone(),
            None => slugify(w.name.value()),
        };
        let verify = if w.verify == 0 { "github" } else { "token" };
        let lines = vec![
            Line::from(Span::styled(
                "Review and save",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            field_line("id", &id),
            field_line("name", w.name.value().trim()),
            field_line("path", w.path.trim()),
            field_line("branch", w.branch.value().trim()),
            field_line("command", w.command.value().trim()),
            field_line("verify", verify),
        ];
        let para = Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(
                        " confirm // route ",
                        Style::default().fg(ACCENT),
                    ))
                    .border_style(Style::default().fg(MUTED)),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(para, area);
    }

    fn render_browser(&self, f: &mut Frame, area: Rect, b: &DirBrowser) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Min(3)])
            .split(area);

        let guidance = match &b.error {
            Some(error) => Line::from(vec![
                Span::styled("! ", Style::default().fg(BAD).add_modifier(Modifier::BOLD)),
                Span::styled(error.clone(), Style::default().fg(BAD)),
            ]),
            None => Line::from(Span::styled(
                "Open a folder with enter, or choose this path with c.",
                Style::default().fg(MUTED),
            )),
        };
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Path: ", Style::default().fg(MUTED)),
                Span::styled(
                    b.cwd.display().to_string(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]),
            guidance,
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " path // browse ",
                    Style::default().fg(ACCENT),
                ))
                .border_style(Style::default().fg(MUTED)),
        );
        f.render_widget(header, chunks[0]);

        let items: Vec<ListItem> = b
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let (icon, name) = match e {
                    DirEntry::Current => ("[.]", "use this folder".to_string()),
                    DirEntry::Up => ("[..]", "parent directory".to_string()),
                    DirEntry::Dir(d) => (
                        "[/]",
                        d.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| d.display().to_string()),
                    ),
                };
                let style = if i == b.selected {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{icon} "), style),
                    Span::styled(name, style),
                ]))
            })
            .collect();
        let list = List::new(items).highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(Some(b.selected));
        f.render_stateful_widget(list, chunks[1], &mut state);
    }

    fn render_confirm(&self, f: &mut Frame, index: usize) {
        let area = centered_rect(60, 25, f.area());
        f.render_widget(Clear, area);
        let name = self
            .config
            .projects
            .get(index)
            .map(|p| p.name.as_str())
            .unwrap_or("");
        let para = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("! ", Style::default().fg(BAD).add_modifier(Modifier::BOLD)),
                Span::styled("Remove route ", Style::default().fg(TEXT)),
                Span::styled(
                    name.to_string(),
                    Style::default().fg(BAD).add_modifier(Modifier::BOLD),
                ),
                Span::styled("?", Style::default().fg(TEXT)),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("[y]", Style::default().fg(BAD).add_modifier(Modifier::BOLD)),
                Span::styled(" remove    ", Style::default().fg(MUTED)),
                Span::styled(
                    "[n]",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" keep", Style::default().fg(MUTED)),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " destructive action ",
                    Style::default().fg(BAD),
                ))
                .border_style(Style::default().fg(BAD)),
        );
        f.render_widget(para, area);
    }

    fn render_key(&self, f: &mut Frame, index: usize) {
        let p = &self.config.projects[index];
        let webhook = format!("http://{}/hooks/{}", self.config.listen_addr, p.id);
        let lines = vec![
            Line::from(vec![
                Span::styled("Project ", Style::default().fg(MUTED)),
                Span::styled(
                    p.name.clone(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::raw(""),
            field_line("webhook", &webhook),
            field_line_hl("secret", &p.secret),
            Line::raw(""),
            Line::from(Span::styled(
                "GitHub: Settings / Webhooks / Add webhook",
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                "  Payload URL = webhook above | Secret = secret above (application/json)",
                Style::default().fg(MUTED),
            )),
        ];
        let area = centered_rect(80, 50, f.area());
        f.render_widget(Clear, area);
        let para = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(
                        " credential // webhook ",
                        Style::default().fg(WARN),
                    ))
                    .border_style(Style::default().fg(MUTED)),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(para, area);

        let footer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(f.area());
        f.render_widget(
            Paragraph::new(key_hints(&[("r", "rotate secret"), ("esc", "back")])),
            footer[1],
        );
    }

    fn render_log(&self, f: &mut Frame, text: &str, scroll: u16) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(f.area());

        let para = Paragraph::new(text)
            .style(Style::default().fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(
                        " output // latest run ",
                        Style::default().fg(ACCENT),
                    ))
                    .border_style(Style::default().fg(MUTED)),
            )
            .scroll((scroll, 0));
        f.render_widget(para, chunks[0]);

        f.render_widget(
            Paragraph::new(key_hints(&[("j/k", "scroll"), ("q/esc", "back")])),
            chunks[1],
        );
    }
}

// ----- free helpers ------------------------------------------------------

/// Convert a display name into a URL-friendly slug.
fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in input.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

/// A `label:  value` line with a dim label (aligned to a fixed width).
fn field_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(WARN)),
        Span::styled("  ", Style::default()),
        Span::styled(value.to_string(), Style::default().fg(TEXT)),
    ])
}

/// Like [`field_line`] but with the value highlighted in the accent color.
fn field_line_hl(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(WARN)),
        Span::raw("  "),
        Span::styled(
            value.to_string(),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn status_color(s: &str) -> Color {
    if s.starts_with("[ok]") {
        GOOD
    } else if s.starts_with("[!!]") {
        BAD
    } else if s.starts_with("[>>]") {
        WARN
    } else {
        MUTED
    }
}

/// Render a value with an ASCII cursor marker at char index `cursor`.
fn input_line(value: &str, cursor: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    for ch in value.chars() {
        if i == cursor {
            spans.push(Span::styled(
                "|",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::raw(ch.to_string()));
        i += 1;
    }
    if i == cursor || spans.is_empty() {
        spans.push(Span::styled(
            "|",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// Progress breadcrumb across the top of the wizard.
fn step_progress(w: &Wizard) -> Line<'static> {
    let steps = ["Name", "Path", "Branch", "Command", "Verify", "Confirm"];
    let cur = match w.step {
        Step::Name => 0,
        Step::Path => 1,
        Step::Branch => 2,
        Step::Command => 3,
        Step::Verify => 4,
        Step::Confirm => 5,
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, s) in steps.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" > ", Style::default().fg(MUTED)));
        }
        if i < cur {
            spans.push(Span::styled(format!("[x] {s}"), Style::default().fg(GOOD)));
        } else if i == cur {
            spans.push(Span::styled(
                format!("[>] {s}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(format!("[ ] {s}"), Style::default().fg(MUTED)));
        }
    }
    Line::from(spans)
}

fn wizard_hints(w: &Wizard) -> Line<'static> {
    if w.browser.is_some() {
        key_hints(&[
            ("j/k", "move"),
            ("enter", "open"),
            ("c", "choose"),
            ("h", "up"),
            ("/", "root"),
            ("~", "home"),
            ("esc", "back"),
        ])
    } else {
        match w.step {
            Step::Name => key_hints(&[("enter", "next"), ("esc", "cancel")]),
            Step::Path => key_hints(&[("c", "choose"), ("esc", "back")]),
            Step::Branch | Step::Command => key_hints(&[("enter", "next"), ("esc", "back")]),
            Step::Verify => key_hints(&[("j/k", "toggle"), ("enter", "next"), ("esc", "back")]),
            Step::Confirm => key_hints(&[("enter", "save"), ("esc", "back")]),
        }
    }
}

/// Consistent footer hints with colored key names and quiet descriptions.
fn key_hints(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, action)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default()));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {action}"),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

/// A popup rectangle centered within `r`, sized by percentages of `r`.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::MENU;

    #[test]
    fn menu_marks_are_unique_ascii() {
        let mut marks = HashSet::new();
        for (mark, _, _) in MENU {
            assert!(mark.is_ascii(), "menu mark must be ASCII: {mark}");
            assert!(marks.insert(mark), "duplicate menu mark: {mark}");
        }
    }
}
