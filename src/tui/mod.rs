//! Interactive management TUI for webhookr.
//!
//! A single-file, dependency-light terminal UI for browsing, adding, editing,
//! deleting, triggering, and inspecting the logs of webhook projects. It reads
//! and writes the shared [`config`] file directly, so changes made here are
//! picked up by the daemon on the next webhook.

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::config::{self, AppConfig, ProjectConfig};
use crate::state;
use crate::util;

/// Field labels shown in the add/edit form, in focus order.
const FIELDS: [&str; 7] = ["Name", "ID", "Path", "Branch", "Command", "Verify mode", "Secret"];
const FIELD_COUNT: usize = FIELDS.len();

const FOOTER_MAIN: &str = "j/k or ↑/↓ select · a add · e edit · d delete · r run · l log · q quit";
const FOOTER_FORM: &str = "Tab/↑/↓ move · ←/→ cursor · type to edit · Enter save · Esc cancel";

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

/// Which screen is currently shown.
enum Screen {
    Main,
    Form {
        editing: Option<usize>,
        fields: FormFields,
        focus: usize,
        cursor: usize,
    },
    ConfirmDelete {
        index: usize,
    },
    Log {
        text: String,
        scroll: u16,
    },
}

/// Editable values of the add/edit form.
struct FormFields {
    name: String,
    id: String,
    path: String,
    branch: String,
    command: String,
    verify_mode: String,
    secret: String,
}

impl FormFields {
    fn get(&self, i: usize) -> &str {
        match i {
            0 => &self.name,
            1 => &self.id,
            2 => &self.path,
            3 => &self.branch,
            4 => &self.command,
            5 => &self.verify_mode,
            _ => &self.secret,
        }
    }

    fn get_mut(&mut self, i: usize) -> &mut String {
        match i {
            0 => &mut self.name,
            1 => &mut self.id,
            2 => &mut self.path,
            3 => &mut self.branch,
            4 => &mut self.command,
            5 => &mut self.verify_mode,
            _ => &mut self.secret,
        }
    }

    /// Insert `c` at char index `at` of field `i`.
    fn insert(&mut self, i: usize, c: char, at: usize) {
        let field = self.get_mut(i);
        let mut out = String::with_capacity(field.len() + c.len_utf8());
        let mut idx = 0;
        let mut inserted = false;
        for ch in field.chars() {
            if idx == at {
                out.push(c);
                inserted = true;
            }
            out.push(ch);
            idx += 1;
        }
        if !inserted {
            out.push(c);
        }
        *field = out;
    }

    /// Remove the char at char index `at` of field `i`.
    fn remove_at(&mut self, i: usize, at: usize) {
        let field = self.get_mut(i);
        let mut out = String::with_capacity(field.len());
        let mut idx = 0;
        for ch in field.chars() {
            if idx != at {
                out.push(ch);
            }
            idx += 1;
        }
        *field = out;
    }
}

/// Top-level application state.
struct App {
    config: AppConfig,
    list_state: ListState,
    screen: Screen,
    last_msg: Option<String>,
    last_runs: HashMap<String, state::RunRecord>,
}

impl App {
    fn new(config: AppConfig) -> Self {
        let mut app = Self {
            config,
            list_state: ListState::default(),
            screen: Screen::Main,
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
            Some("success") => "✓ success",
            Some("failed") => "✗ failed",
            Some("running") => "… running",
            Some(_) => "· unknown",
            None => "· never",
        }
    }

    fn selected_index(&self) -> Option<usize> {
        if self.config.projects.is_empty() {
            None
        } else {
            Some(self.list_state.selected().unwrap_or(0))
        }
    }

    // ----- key dispatch -------------------------------------------------

    /// Returns `Ok(true)` when the app should quit.
    fn on_key(&mut self, key: KeyCode) -> Result<bool> {
        match std::mem::replace(&mut self.screen, Screen::Main) {
            Screen::Main => Ok(self.handle_main(key)),
            Screen::Form { editing, fields, focus, cursor } => {
                self.handle_form(key, editing, fields, focus, cursor)?;
                Ok(false)
            }
            Screen::ConfirmDelete { index } => {
                self.handle_confirm(key, index)?;
                Ok(false)
            }
            Screen::Log { text, scroll } => {
                self.handle_log(key, text, scroll);
                Ok(false)
            }
        }
    }

    fn handle_main(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Char('q') => return true,
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Char('a') => self.open_add_form(),
            KeyCode::Char('e') => self.open_edit_form(),
            KeyCode::Char('d') => self.open_confirm_delete(),
            KeyCode::Char('r') => self.trigger_run(),
            KeyCode::Char('l') => self.open_log(),
            _ => {}
        }
        false
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

    fn open_add_form(&mut self) {
        let fields = FormFields {
            name: String::new(),
            id: String::new(),
            path: String::new(),
            branch: "main".to_string(),
            command: String::new(),
            verify_mode: "github".to_string(),
            secret: util::generate_secret(),
        };
        self.screen = Screen::Form { editing: None, fields, focus: 0, cursor: 0 };
    }

    fn open_edit_form(&mut self) {
        let Some(i) = self.selected_index() else {
            self.last_msg = Some("no project selected".to_string());
            return;
        };
        let p = &self.config.projects[i];
        let fields = FormFields {
            name: p.name.clone(),
            id: p.id.clone(),
            path: p.path.clone(),
            branch: p.branch.clone(),
            command: p.command.clone(),
            verify_mode: p.verify_mode.clone(),
            secret: p.secret.clone(),
        };
        let cursor = fields.name.chars().count();
        self.screen = Screen::Form { editing: Some(i), fields, focus: 0, cursor };
    }

    fn open_confirm_delete(&mut self) {
        let Some(i) = self.selected_index() else {
            self.last_msg = Some("no project selected".to_string());
            return;
        };
        self.screen = Screen::ConfirmDelete { index: i };
    }

    fn trigger_run(&mut self) {
        let Some(i) = self.selected_index() else {
            self.last_msg = Some("no project selected".to_string());
            return;
        };
        let p = &self.config.projects[i];
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .args(["run", "--id", p.id.as_str()])
                .spawn();
        }
        self.last_msg = Some(format!("triggered {}", p.id));
    }

    fn open_log(&mut self) {
        let Some(i) = self.selected_index() else {
            self.last_msg = Some("no project selected".to_string());
            return;
        };
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

    // ----- form handling ------------------------------------------------

    fn handle_form(
        &mut self,
        key: KeyCode,
        editing: Option<usize>,
        mut fields: FormFields,
        mut focus: usize,
        mut cursor: usize,
    ) -> Result<()> {
        match key {
            KeyCode::Esc => {
                self.screen = Screen::Main;
                return Ok(());
            }
            KeyCode::Tab | KeyCode::Down => {
                focus = (focus + 1) % FIELD_COUNT;
                cursor = fields.get(focus).chars().count();
            }
            KeyCode::BackTab | KeyCode::Up => {
                focus = (focus + FIELD_COUNT - 1) % FIELD_COUNT;
                cursor = fields.get(focus).chars().count();
            }
            KeyCode::Enter => {
                return self.save_form(editing, fields, focus, cursor);
            }
            KeyCode::Char(c) => {
                fields.insert(focus, c, cursor);
                cursor += 1;
            }
            KeyCode::Backspace => {
                if cursor > 0 {
                    fields.remove_at(focus, cursor - 1);
                    cursor -= 1;
                }
            }
            KeyCode::Left => cursor = cursor.saturating_sub(1),
            KeyCode::Right => {
                let len = fields.get(focus).chars().count();
                if cursor < len {
                    cursor += 1;
                }
            }
            _ => {}
        }
        self.screen = Screen::Form { editing, fields, focus, cursor };
        Ok(())
    }

    fn save_form(
        &mut self,
        editing: Option<usize>,
        fields: FormFields,
        focus: usize,
        cursor: usize,
    ) -> Result<()> {
        let mut id = fields.id.trim().to_string();
        if id.is_empty() {
            id = slugify(&fields.name);
        }
        // Regenerate a secret if the field was left (or made) empty.
        let secret = if fields.secret.is_empty() {
            util::generate_secret()
        } else {
            fields.secret.clone()
        };

        let project = ProjectConfig {
            id,
            name: fields.name.trim().to_string(),
            path: fields.path.trim().to_string(),
            branch: fields.branch.trim().to_string(),
            command: fields.command.trim().to_string(),
            secret,
            verify_mode: fields.verify_mode.trim().to_string(),
        };

        match project.validate() {
            Ok(()) => {
                self.config.upsert(project);
                config::save_config(&self.config)?;
                self.last_msg = Some(match editing {
                    Some(_) => "saved".to_string(),
                    None => "added".to_string(),
                });
                self.screen = Screen::Main;
            }
            Err(e) => {
                self.last_msg = Some(format!("error: {e:#}"));
                self.screen = Screen::Form { editing, fields, focus, cursor };
            }
        }
        Ok(())
    }

    // ----- confirm-delete handling --------------------------------------

    fn handle_confirm(&mut self, key: KeyCode, index: usize) -> Result<()> {
        match key {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let id = self.config.projects[index].id.clone();
                self.config.remove(&id);
                config::save_config(&self.config)?;
                self.last_msg = Some(format!("deleted {}", id));
                self.list_state.select(if self.config.projects.is_empty() {
                    None
                } else {
                    Some(index.min(self.config.projects.len() - 1))
                });
                self.screen = Screen::Main;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.screen = Screen::Main;
            }
            _ => {
                self.screen = Screen::ConfirmDelete { index };
            }
        }
        Ok(())
    }

    // ----- log handling --------------------------------------------------

    fn handle_log(&mut self, key: KeyCode, text: String, scroll: u16) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = Screen::Main;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let max = (text.lines().count() as u16).saturating_sub(1);
                self.screen = Screen::Log { text, scroll: (scroll + 1).min(max) };
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.screen = Screen::Log { text, scroll: scroll.saturating_sub(1) };
            }
            _ => {
                self.screen = Screen::Log { text, scroll };
            }
        }
    }

    // ----- rendering -----------------------------------------------------

    fn render(&mut self, f: &mut Frame) {
        match std::mem::replace(&mut self.screen, Screen::Main) {
            Screen::Main => {
                self.render_main(f);
                self.screen = Screen::Main;
            }
            Screen::Form { editing, fields, focus, cursor } => {
                self.render_form(f, &fields, focus, cursor, editing.is_some());
                self.screen = Screen::Form { editing, fields, focus, cursor };
            }
            Screen::ConfirmDelete { index } => {
                self.render_confirm(f, index);
                self.screen = Screen::ConfirmDelete { index };
            }
            Screen::Log { text, scroll } => {
                self.render_log(f, &text, scroll);
                self.screen = Screen::Log { text, scroll };
            }
        }
    }

    fn render_main(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)])
            .split(f.area());

        let items: Vec<ListItem> = if self.config.projects.is_empty() {
            vec![ListItem::new("No projects yet — press 'a' to add one")]
        } else {
            self.config
                .projects
                .iter()
                .map(|p| {
                    let line = format!(
                        "{}  {}  [branch {}]  {}    {}",
                        p.id,
                        p.name,
                        p.branch,
                        p.command,
                        self.status_for(&p.id)
                    );
                    ListItem::new(line)
                })
                .collect()
        };

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Projects "))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("❯ ");

        f.render_stateful_widget(list, chunks[0], &mut self.list_state);

        if let Some(msg) = &self.last_msg {
            f.render_widget(Paragraph::new(msg.as_str()), chunks[1]);
        }

        f.render_widget(
            Paragraph::new(FOOTER_MAIN).style(Style::default().fg(Color::DarkGray)),
            chunks[2],
        );
    }

    fn render_form(&self, f: &mut Frame, fields: &FormFields, focus: usize, cursor: usize, editing: bool) {
        let mut constraints = vec![Constraint::Length(1)]; // title
        for i in 0..FIELD_COUNT {
            constraints.push(if i == focus {
                Constraint::Length(3)
            } else {
                Constraint::Length(1)
            });
        }
        constraints.push(Constraint::Length(1)); // status line (last_msg)
        constraints.push(Constraint::Length(1)); // footer hint

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(f.area());

        let title = if editing { "Edit project" } else { "Add project" };
        f.render_widget(
            Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
            chunks[0],
        );

        for i in 0..FIELD_COUNT {
            let value = fields.get(i);
            let text = if i == focus {
                format!("{}: {}", FIELDS[i], with_cursor(value, cursor))
            } else {
                format!("{}: {}", FIELDS[i], value)
            };
            let para = if i == focus {
                Paragraph::new(text).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                )
            } else {
                Paragraph::new(text).style(Style::default().fg(Color::DarkGray))
            };
            f.render_widget(para, chunks[1 + i]);
        }

        if let Some(msg) = &self.last_msg {
            f.render_widget(
                Paragraph::new(msg.as_str()).style(Style::default().fg(Color::Red)),
                chunks[1 + FIELD_COUNT],
            );
        }

        f.render_widget(
            Paragraph::new(FOOTER_FORM).style(Style::default().fg(Color::DarkGray)),
            chunks[2 + FIELD_COUNT],
        );
    }

    fn render_confirm(&self, f: &mut Frame, index: usize) {
        let area = centered_rect(60, 5, f.area());
        f.render_widget(Clear, area);
        let name = self.config.projects.get(index).map(|p| p.name.as_str()).unwrap_or("");
        let para = Paragraph::new(format!("Delete {name}?\n\n[y/N]"))
            .block(Block::default().borders(Borders::ALL).title(" Confirm "));
        f.render_widget(para, area);
    }

    fn render_log(&self, f: &mut Frame, text: &str, scroll: u16) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(f.area());

        let para = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" Run log "))
            .scroll((scroll, 0));
        f.render_widget(para, chunks[0]);

        f.render_widget(
            Paragraph::new("j/k or ↑/↓ scroll · q/Esc back").style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
    }
}

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

/// Insert a `▌` cursor marker into `s` at char index `cursor`.
fn with_cursor(s: &str, cursor: usize) -> String {
    let mut out = String::with_capacity(s.len() + "▌".len());
    let mut i = 0;
    for c in s.chars() {
        if i == cursor {
            out.push('▌');
        }
        out.push(c);
        i += 1;
    }
    if i == cursor {
        out.push('▌');
    }
    out
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
