use crate::{
    api::{Api, Project, Snapshot, Task},
    config::{self, Config},
    fields,
    preview::{self, Action},
    sidebar::{self, View},
    sync::{self, Job},
    tree,
};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode as K, KeyEvent, KeyModifiers as M},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{prelude::*, widgets::*};
use unicode_width::UnicodeWidthStr;
use std::{
    collections::{HashSet, VecDeque},
    io,
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

#[derive(PartialEq)]
enum Mode {
    Normal,
    Search,
    Command,
    Add,
    Subtask,
    Rename,
    Item,
    ItemRename,
    Move,
    Due,
    Remind,
    Delete,
    Help,
    Token,
}
struct App {
    data: Snapshot,
    confirmed: Snapshot,
    pending_jobs: VecDeque<Job>,
    in_flight: usize,
    project: usize,
    selected: usize,
    projects_focus: bool,
    preview_focus: bool,
    preview_cursor: usize,
    preview_width: usize,
    /// Task column width (%) when the preview is visible.
    split: u16,
    zoom: bool,
    hide_lists: bool,
    collapsed: HashSet<String>,
    /// Highlighted option in the due/reminder pickers, or the checklist item being renamed.
    pick: usize,
    theme_request: Option<String>,
    mode: Mode,
    input: String,
    input_cursor: usize,
    search_original: String,
    target: Option<Task>,
    query: String,
    message: String,
    busy: bool,
    demo: bool,
    pending: char,
    count: String,
    details: bool,
    last_sync: Instant,
    tx: Option<Sender<Job>>,
    rx: Option<Receiver<sync::Event>>,
}
impl App {
    fn sidebar(&self) -> Vec<sidebar::Entry> {
        sidebar::entries(&self.data, &self.collapsed)
    }
    fn view(&self) -> View {
        self.sidebar()
            .get(self.project)
            .map(|e| e.view.clone())
            .unwrap_or(View::All)
    }
    fn replay(&mut self) {
        self.data = self.confirmed.clone();
        for job in &self.pending_jobs {
            sync::apply(&mut self.data, job);
        }
        self.clamp();
    }
    fn update(&mut self, task: Task) {
        if let Some(before) = self.data.tasks.iter().find(|t| t.id == task.id) {
            let old = serde_json::to_value(before).unwrap();
            let new = serde_json::to_value(&task).unwrap();
            let fields = new
                .as_object()
                .unwrap()
                .iter()
                .filter(|(k, v)| {
                    old.get(*k) != Some(*v) && k.as_str() != "id" && k.as_str() != "projectId"
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.send(Job::Patch {
                id: task.id,
                project: task.project_id,
                fields,
            });
        }
    }
    fn create(&mut self, project: String, title: String, parent: Option<String>) {
        let task = Task {
            id: format!("local:{}", uuid::Uuid::new_v4()),
            project_id: project.clone(),
            title,
            parent_id: parent,
            ..Default::default()
        };
        self.query.clear();
        if let Some(group) = self
            .data
            .projects
            .iter()
            .find(|p| p.id == project)
            .and_then(|p| p.group_id.as_ref())
        {
            self.collapsed.remove(&format!("group:{group}"));
        }
        if let Some(id) = task.parent_id.as_ref() {
            self.collapsed.remove(id);
        }
        self.project = self
            .sidebar()
            .iter()
            .position(|e| e.view == View::Project(project.clone()))
            .unwrap_or_else(|| {
                self.sidebar()
                    .iter()
                    .position(|e| e.view == View::Inbox)
                    .unwrap_or(0)
            });
        let id = task.id.clone();
        self.send(Job::Create(task));
        self.selected = self.visible().iter().position(|t| t.id == id).unwrap_or(0);
        self.preview_cursor = 0;
    }
    fn tree(&self) -> Vec<tree::Row<'_>> {
        let view = self.view();
        let tasks = self
            .data
            .tasks
            .iter()
            .filter(|t| sidebar::matches(&view, t, &self.data))
            .collect::<Vec<_>>();
        tree::rows(&tasks, &self.collapsed, &self.query)
    }
    fn visible(&self) -> Vec<&Task> {
        self.tree().into_iter().map(|r| r.task).collect()
    }
    fn preview(&self, theme: &config::Theme) -> Vec<preview::Row> {
        self.task()
            .map(|t| preview::document(&t, &self.data, theme, self.preview_width))
            .unwrap_or_default()
    }
    fn focus(&mut self, pane: u8) {
        self.projects_focus = pane == 0;
        self.preview_focus = pane == 2;
        self.zoom &= self.preview_focus;
        self.hide_lists &= !self.projects_focus;
        if self.preview_focus {
            self.details = true;
        }
    }
    fn fold(&mut self, open: Option<bool>) {
        if self.projects_focus {
            if let View::Group(id) = self.view() {
                let id = format!("group:{id}");
                let folded = self.collapsed.contains(&id);
                if open.unwrap_or(folded) {
                    self.collapsed.remove(&id);
                } else {
                    self.collapsed.insert(id);
                }
            }
            return;
        }
        if let Some(task) = self.task() {
            let folded = self.collapsed.contains(&task.id);
            if open.unwrap_or(folded) {
                self.collapsed.remove(&task.id);
            } else {
                self.collapsed.insert(task.id);
            }
        }
    }
    fn preview_action(&mut self, theme: &config::Theme, open: bool) {
        if self.busy {
            self.message = "Salvando alterações anteriores…".into();
        }
        let action = self
            .preview(theme)
            .get(self.preview_cursor)
            .and_then(|r| r.action.clone());
        if let Some(action) = action {
            if let Action::Subtask(id) = action {
                if open {
                    // Reveal ancestors, then select the child without imposing a depth limit.
                    self.collapsed.clear();
                    if let Some(i) = self.visible().iter().position(|t| t.id == id) {
                        self.selected = i;
                        self.preview_cursor = 0;
                    }
                } else if let Some(t) = self.data.tasks.iter().find(|t| t.id == id) {
                    self.send(Job::Complete(t.clone()));
                }
            } else if let Some(mut task) = self.task()
                && preview::toggle(&mut task, &action)
            {
                self.update(task);
            }
        }
    }
    fn tz(&self) -> Option<chrono_tz::Tz> {
        self.data.time_zone.as_ref().and_then(|s| s.parse().ok())
    }
    /// Inbox and lists whose name contains `query`, in sidebar order.
    fn lists(&self, query: &str) -> Vec<(String, String)> {
        let query = query.to_lowercase();
        let inbox = self.data.inbox_id.clone();
        inbox
            .iter()
            .map(|id| (id.clone(), "Inbox".to_owned()))
            .chain(
                self.data
                    .projects
                    .iter()
                    .filter(|p| Some(&p.id) != inbox.as_ref())
                    .map(|p| (p.id.clone(), p.name.clone())),
            )
            .filter(|(_, name)| name.to_lowercase().contains(&query))
            .collect()
    }
    fn options(&self) -> &'static [(&'static str, &'static str)] {
        match self.mode {
            Mode::Due => &DUE,
            Mode::Remind => &REMIND,
            _ => &[],
        }
    }
    fn edit(&mut self, mode: Mode, input: String) {
        self.target = self.task();
        self.pick = 0;
        if self.target.is_some() {
            self.mode = mode;
            self.input_cursor = input.chars().count();
            self.input = input;
        }
    }
    /// Native checklist item under the preview cursor. Row layout does not depend on colors.
    fn checklist_item(&self) -> Option<usize> {
        match self.preview(&config::Theme::default()).get(self.preview_cursor)?.action {
            Some(Action::Checklist(i)) => Some(i),
            _ => None,
        }
    }
    fn task(&self) -> Option<Task> {
        self.visible().get(self.selected).map(|t| (*t).clone())
    }
    fn replace_snapshot(&mut self, data: Snapshot) {
        let view = self.view();
        let task_id = self.task().map(|t| t.id);
        self.confirmed = data;
        self.replay();
        self.project = self
            .sidebar()
            .iter()
            .position(|e| e.view == view)
            .unwrap_or(0);
        if let Some(index) = task_id.and_then(|id| self.visible().iter().position(|t| t.id == id)) {
            self.selected = index;
        }
        self.clamp();
    }
    fn clamp(&mut self) {
        self.project = self.project.min(self.sidebar().len().saturating_sub(1));
        self.selected = self.selected.min(self.visible().len().saturating_sub(1));
    }
    fn send(&mut self, job: Job) {
        if self.demo {
            sync::apply(&mut self.data, &job);
            self.confirmed = self.data.clone();
            self.message = "Demonstração · alteração apenas local".into();
            self.clamp();
            return;
        }
        if matches!(job, Job::Refresh) && self.busy {
            return;
        }
        if self
            .tx
            .as_ref()
            .is_some_and(|tx| tx.send(job.clone()).is_ok())
        {
            if !matches!(job, Job::Refresh) {
                self.pending_jobs.push_back(job.clone());
                sync::apply(&mut self.data, &job);
            }
            self.in_flight += 1;
            self.busy = true;
            self.message = format!("Salvando… {} operação(ões)", self.in_flight);
            self.clamp();
        } else {
            self.message = "Sincronização indisponível; alteração não enviada.".into();
        }
    }
    fn movement(&mut self, delta: isize) {
        if self.preview_focus {
            self.preview_cursor = self.preview_cursor.saturating_add_signed(delta);
            return;
        }
        self.preview_cursor = 0;
        let max = if self.projects_focus {
            self.sidebar().len().saturating_sub(1)
        } else {
            self.visible().len().saturating_sub(1)
        };
        let index = if self.projects_focus {
            &mut self.project
        } else {
            &mut self.selected
        };
        *index = index.saturating_add_signed(delta).min(max);
        if self.projects_focus {
            self.selected = 0;
        }
    }
    fn submit(&mut self) -> bool {
        let text = self.input.trim().to_owned();
        match self.mode {
            Mode::Search => {
                self.query = text;
                self.selected = 0;
            }
            Mode::Add if !text.is_empty() => {
                let project = match self.view() {
                    View::Project(id) => Some(id),
                    View::Inbox => Some(self.data.inbox_id.clone().unwrap_or("inbox".into())),
                    _ => None,
                };
                if let Some(id) = project {
                    self.create(id, text, None);
                } else {
                    self.message = "Selecione uma lista ou Inbox para criar a tarefa.".into();
                }
            }
            Mode::Subtask if !text.is_empty() => {
                if let Some(t) = self.target.take() {
                    self.create(t.project_id, text, Some(t.id));
                }
            }
            Mode::Item if !text.is_empty() => {
                if let Some(mut t) = self.target.take() {
                    t.items.push(crate::api::ChecklistItem {
                        // TickTick ids are 24 hex chars, generated by the client.
                        id: uuid::Uuid::new_v4().simple().to_string()[..24].into(),
                        title: text,
                        ..Default::default()
                    });
                    self.update(t);
                    // Stay open for the next item; Enter on an empty line or Esc finishes.
                    self.target = self.task();
                    self.input.clear();
                    self.input_cursor = 0;
                    return false;
                }
            }
            Mode::ItemRename if !text.is_empty() => {
                if let Some(mut t) = self.target.take()
                    && let Some(item) = t.items.get_mut(self.pick)
                {
                    item.title = text;
                    self.update(t);
                }
            }
            Mode::Rename if !text.is_empty() => {
                if let Some(mut t) = self.target.take() {
                    t.title = text;
                    self.update(t);
                }
            }
            Mode::Move if !text.is_empty() => {
                if let Some(t) = self.target.take() {
                    match self.lists(&text).into_iter().next() {
                        Some((id, _)) if id == t.project_id => {
                            self.message = "A tarefa já está nessa lista.".into()
                        }
                        Some((to, _)) => self.send(Job::Move { task: t, to }),
                        None => self.message = format!("Lista não encontrada: {text}"),
                    }
                }
            }
            Mode::Due => {
                if let Some(t) = self.target.take() {
                    match fields::due_fields(&text, sidebar::today(&self.data), self.tz()) {
                        Ok(fields) => self.send(Job::Patch {
                            id: t.id,
                            project: t.project_id,
                            fields,
                        }),
                        Err(e) => self.message = e,
                    }
                }
            }
            Mode::Remind => {
                if let Some(t) = self.target.take() {
                    match fields::parse_reminders(&text) {
                        Ok(r) => self.send(Job::Patch {
                            id: t.id,
                            project: t.project_id,
                            fields: [("reminders".to_owned(), serde_json::json!(r))]
                                .into_iter()
                                .collect(),
                        }),
                        Err(e) => self.message = e,
                    }
                }
            }
            Mode::Command => match text.as_str() {
                value if value.starts_with("theme ") => {
                    self.theme_request = Some(value[6..].trim().into())
                }
                "subtask" => {
                    self.target = self.task();
                    self.mode = Mode::Subtask;
                    self.input.clear();
                    return false;
                }
                "q" | "quit" | "q!" => return true,
                "w" | "sync" | "wq" => {
                    if text == "wq" {
                        self.message =
                            "Writes save immediately; use :q after sync finishes.".into();
                    } else {
                        self.send(Job::Refresh);
                    }
                }
                "help" | "h" => {
                    self.mode = Mode::Help;
                    return false;
                }
                "noh" | "nohlsearch" => self.query.clear(),
                "config" => {
                    self.message = config::config_dir()
                        .join("config.toml")
                        .display()
                        .to_string()
                }
                "token" => {
                    self.mode = Mode::Token;
                    self.input.clear();
                    return false;
                }
                "delete" => {
                    self.target = self.task();
                    self.mode = Mode::Delete;
                    return false;
                }
                "complete" => {
                    if let Some(t) = self.task() {
                        self.send(Job::Complete(t));
                    }
                }
                _ => self.message = format!("Unknown command: {text}"),
            },
            _ => (),
        }
        self.mode = Mode::Normal;
        self.input.clear();
        false
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.code == K::Char('c') && key.modifiers.contains(M::CONTROL) {
            return true;
        }
        if self.mode == Mode::Help {
            self.mode = Mode::Normal;
            return false;
        }
        if self.mode == Mode::Delete {
            if key.code == K::Char('y')
                && let Some(t) = self.target.take()
            {
                self.send(Job::Delete(t));
            }
            self.target = None;
            self.mode = Mode::Normal;
            return false;
        }
        if matches!(self.mode, Mode::Due | Mode::Remind) {
            let options = self.options();
            // j/k navigate unless the user is typing a custom date ("hoje" contains a j).
            let typing = self.mode == Mode::Due && !self.input.is_empty();
            let step = match key.code {
                K::Down => 1,
                K::Up => -1,
                K::Char('j') if !typing => 1,
                K::Char('k') if !typing => -1,
                _ => 0,
            };
            if step != 0 {
                self.pick = (self.pick as isize + step).rem_euclid(options.len() as isize) as usize;
                return false;
            }
            let value = options[self.pick].1;
            match key.code {
                K::Tab if self.mode == Mode::Due => {
                    self.input = format!("{value} ");
                    self.input_cursor = self.input.chars().count();
                    return false;
                }
                K::Enter if self.mode == Mode::Due && self.input.trim().is_empty() => {
                    self.input = value.into();
                    return self.submit();
                }
                K::Char(' ') if self.mode == Mode::Remind => {
                    let mut tokens = self
                        .input
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    match tokens.iter().position(|t| t == value) {
                        Some(i) => drop(tokens.remove(i)),
                        None => tokens.push(value.into()),
                    }
                    self.input = tokens.join(", ");
                    self.input_cursor = self.input.chars().count();
                    return false;
                }
                _ => (),
            }
        }
        if self.mode != Mode::Normal {
            match key.code {
                K::Esc => {
                    if self.mode == Mode::Search {
                        self.query = self.search_original.clone();
                    }
                    self.mode = Mode::Normal;
                    self.input.clear();
                    self.target = None;
                }
                K::Enter => return self.submit(),
                _ => {
                    edit_input(&mut self.input, &mut self.input_cursor, key);
                    if self.mode == Mode::Search {
                        self.query = self.input.clone();
                        self.selected = 0;
                        self.preview_cursor = 0;
                    }
                }
            }
            return false;
        }
        if let K::Char(c @ '1'..='9') = key.code {
            self.count.push(c);
            return false;
        }
        if key.code == K::Char('0') && !self.count.is_empty() {
            self.count.push('0');
            return false;
        }
        let n = self.count.parse::<usize>().unwrap_or(1).min(10000);
        self.count.clear();
        let pending = self.pending;
        self.pending = '\0';
        match key.code {
            K::Char('q') => return true,
            K::Char('j') | K::Down => self.movement(n as isize),
            K::Char('k') | K::Up => self.movement(-(n as isize)),
            K::Char('h') | K::Left => self.focus(if self.preview_focus { 1 } else { 0 }),
            K::Char('l') | K::Right => self.focus(if self.projects_focus { 1 } else { 2 }),
            K::Tab => self.focus(if self.projects_focus {
                1
            } else if self.preview_focus {
                0
            } else {
                2
            }),
            K::BackTab => self.focus(if self.projects_focus {
                2
            } else if self.preview_focus {
                1
            } else {
                0
            }),
            K::Char('g') if pending == 'g' => {
                if self.preview_focus {
                    self.preview_cursor = 0;
                } else if self.projects_focus {
                    self.project = 0;
                } else {
                    self.selected = 0;
                }
            }
            K::Char('g') => self.pending = 'g',
            K::Char('z') => self.pending = 'z',
            K::Char('a') if pending == 'z' => self.fold(None),
            K::Char('o') if pending == 'z' => self.fold(Some(true)),
            K::Char('c') if pending == 'z' => self.fold(Some(false)),
            K::Char('A') => {
                self.target = self.task();
                self.input.clear();
                self.mode = Mode::Subtask;
            }
            K::Char('G') => self.movement(100000),
            K::Char('d') if key.modifiers.contains(M::CONTROL) => self.movement(10),
            K::Char('u') if key.modifiers.contains(M::CONTROL) => self.movement(-10),
            K::Char('d') if pending == 'd' && self.preview_focus => {
                if let (Some(mut t), Some(i)) = (self.task(), self.checklist_item()) {
                    t.items.remove(i);
                    self.update(t);
                    self.preview_cursor = self.preview_cursor.saturating_sub(1);
                }
            }
            K::Char('d') if pending == 'd' => {
                if let Some(t) = self.task() {
                    self.target = Some(t);
                    self.mode = Mode::Delete;
                }
            }
            K::Char('d') => self.pending = 'd',
            K::Char('m') => self.edit(Mode::Move, String::new()),
            K::Char('D') => self.edit(Mode::Due, String::new()),
            K::Char('R') if self.task().is_some_and(|t| t.due_date.is_none()) => {
                self.message = "Defina um prazo (D) antes do lembrete.".into()
            }
            K::Char('R') => {
                let reminders = self
                    .task()
                    .and_then(|t| serde_json::from_value(t.extra.get("reminders")?.clone()).ok())
                    .map(|r: Vec<String>| fields::format_reminders(&r));
                self.edit(Mode::Remind, reminders.unwrap_or_default());
            }
            K::Char('/') => {
                self.mode = Mode::Search;
                self.search_original = self.query.clone();
                self.input = self.query.clone();
                self.input_cursor = self.input.chars().count();
            }
            K::Char(':') => {
                self.mode = Mode::Command;
                self.input.clear();
            }
            K::Char('a') | K::Char('o') if self.preview_focus => self.edit(Mode::Item, String::new()),
            K::Char('a') | K::Char('i') | K::Char('o') => {
                self.mode = Mode::Add;
                self.input.clear();
            }
            K::Char('c') if pending == 'c' && self.preview_focus => {
                if let (Some(t), Some(i)) = (self.task(), self.checklist_item()) {
                    self.edit(Mode::ItemRename, t.items[i].title.clone());
                    self.pick = i;
                }
            }
            K::Char('c') if pending == 'c' => {
                if let Some(t) = self.task() {
                    self.mode = Mode::Rename;
                    self.input = t.title.clone();
                    self.input_cursor = self.input.chars().count();
                    self.target = Some(t);
                }
            }
            K::Char('c') => self.pending = 'c',
            K::Char('x') | K::Char(' ') => {
                if let Some(t) = self.task() {
                    self.send(Job::Complete(t));
                }
            }
            K::Char('r') => self.send(Job::Refresh),
            K::Char('?') => self.mode = Mode::Help,
            K::Char('f') => {
                let zoom = !self.zoom;
                self.focus(2);
                self.zoom = zoom;
            }
            K::Char('b') => {
                self.hide_lists = !self.hide_lists;
                if self.projects_focus {
                    self.focus(1);
                }
            }
            K::Char('<') => self.split = self.split.saturating_sub(5).max(15),
            K::Char('>') => self.split = (self.split + 5).min(70),
            K::Enter => self.focus(2),
            K::Char('v') => {
                self.details = !self.details;
                if !self.details {
                    self.focus(1);
                }
            }
            K::Char('p') => {
                if let Some(mut t) = self.task() {
                    t.priority = match t.priority {
                        0 => 1,
                        1 => 3,
                        3 => 5,
                        _ => 0,
                    };
                    self.update(t);
                }
            }
            K::Esc => {
                if self.preview_focus {
                    self.focus(1);
                } else {
                    self.query.clear();
                }
                self.pending = '\0';
            }
            _ => (),
        }
        self.clamp();
        false
    }
}
struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
pub fn run(mut config: Config, api: Option<Api>, demo: bool) -> Result<()> {
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let api = if !demo && api.is_none() {
        match crate::token_ui::configure(&mut terminal, &config)? {
            Some(api) => Some(api),
            None => return Ok(()),
        }
    } else {
        api
    };
    let data = if demo {
        demo_data()
    } else {
        std::fs::read(config::cache_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    };
    let (tx, rx) = if let Some(api) = api {
        let (t, r) = sync::worker(api, data.clone());
        (Some(t), Some(r))
    } else {
        (None, None)
    };
    let mut app = App {
        confirmed: data.clone(),
        pending_jobs: VecDeque::new(),
        in_flight: 0,
        data,
        project: 0,
        selected: 0,
        projects_focus: false,
        preview_focus: false,
        preview_cursor: 0,
        preview_width: 60,
        split: 34,
        zoom: false,
        hide_lists: false,
        collapsed: HashSet::new(),
        pick: 0,
        theme_request: None,
        mode: Mode::Normal,
        input: String::new(),
        input_cursor: 0,
        search_original: String::new(),
        target: None,
        query: String::new(),
        message: if demo {
            "DEMO · read-only · ? help".into()
        } else {
            "Cached tasks · connecting…".into()
        },
        busy: false,
        demo,
        pending: '\0',
        count: String::new(),
        details: true,
        last_sync: Instant::now(),
        tx,
        rx,
    };
    if !demo {
        app.send(Job::Refresh);
    }
    loop {
        loop {
            let event = app.rx.as_ref().and_then(|rx| rx.try_recv().ok());
            if let Some(event) = event {
                app.in_flight = app.in_flight.saturating_sub(1);
                app.busy = app.in_flight > 0;
                app.last_sync = Instant::now();
                match event {
                    sync::Event::Data(d, m) => {
                        app.replace_snapshot(d);
                        app.message = m;
                    }
                    sync::Event::Applied {
                        original,
                        actual,
                        data,
                        message,
                    } => {
                        let selected = app.task().map(|t| t.id);
                        app.pending_jobs.pop_front();
                        let mut selected = selected;
                        if let (Job::Create(local), Job::Create(remote)) =
                            (original.as_ref(), actual.as_ref())
                        {
                            for job in &mut app.pending_jobs {
                                sync::remap(job, &local.id, &remote.id);
                            }
                            if selected.as_deref() == Some(&local.id) {
                                selected = Some(remote.id.clone());
                            }
                        }
                        app.replace_snapshot(data);
                        if let Some(index) =
                            selected.and_then(|id| app.visible().iter().position(|t| t.id == id))
                        {
                            app.selected = index;
                        }
                        app.message = message;
                    }
                    sync::Event::Error { message, mutation } => {
                        if mutation {
                            app.pending_jobs.pop_front();
                            app.replay();
                        }
                        app.message = message;
                    }
                }
            } else {
                break;
            }
        }
        if app.mode == Mode::Token {
            app.mode = Mode::Normal;
            if app.demo {
                app.message = "Run without demo to configure your account.".into();
            } else if app.busy {
                app.message = "Wait for synchronization, then use :token again.".into();
            } else if let Some(api) = crate::token_ui::configure(&mut terminal, &config)? {
                let (tx, rx) = sync::worker(api, Snapshot::default());
                app.tx = Some(tx);
                app.rx = Some(rx);
                app.data = Snapshot::default();
                app.confirmed = Snapshot::default();
                app.pending_jobs.clear();
                app.project = 0;
                app.selected = 0;
                app.query.clear();
                app.target = None;
                // Replace the prior account's cached tasks before fetching the new account.
                config::private_write(&config::cache_path(), &serde_json::to_vec(&app.data)?)?;
                app.send(Job::Refresh);
            }
            terminal.clear()?;
        }
        if let Some(name) = app.theme_request.take() {
            match config::named_theme(&name) {
                Ok(t) => {
                    config.theme = t;
                    app.message = format!("Tema: {name}");
                }
                Err(e) => app.message = e.to_string(),
            }
        }
        app.clamp();
        let area = terminal.size()?;
        let preview_area = preview_area(Rect::new(0, 0, area.width, area.height), &app);
        app.preview_width = preview_area.width.saturating_sub(6).max(1) as usize;
        app.preview_cursor = app
            .preview_cursor
            .min(app.preview(&config.theme).len().saturating_sub(1));
        if !demo
            && !app.busy
            && app.last_sync.elapsed() >= Duration::from_secs(config.sync_seconds.max(10))
        {
            app.send(Job::Refresh);
        }
        terminal.draw(|f| draw(f, &app, &config))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != event::KeyEventKind::Press {
                continue;
            }
            if app.mode == Mode::Normal
                && app.preview_focus
                && matches!(key.code, K::Char(' ') | K::Char('x') | K::Enter)
            {
                app.preview_action(&config.theme, key.code == K::Enter);
            } else if app.mode == Mode::Normal
                && app.preview_focus
                && matches!(key.code, K::Char(']') | K::Char('['))
            {
                let rows = app.preview(&config.theme);
                let index = if key.code == K::Char(']') {
                    rows.iter()
                        .enumerate()
                        .skip(app.preview_cursor + 1)
                        .find(|(_, r)| r.action.is_some())
                        .map(|(i, _)| i)
                } else {
                    rows.iter()
                        .enumerate()
                        .take(app.preview_cursor)
                        .rev()
                        .find(|(_, r)| r.action.is_some())
                        .map(|(i, _)| i)
                };
                if let Some(i) = index {
                    app.preview_cursor = i;
                }
            } else if app.mode == Mode::Normal && key.code == K::Char('e') {
                if let Some(mut t) = app.task() {
                    disable_raw_mode()?;
                    execute!(io::stdout(), LeaveAlternateScreen)?;
                    let edited = edit(&t);
                    execute!(io::stdout(), EnterAlternateScreen)?;
                    enable_raw_mode()?;
                    terminal.clear()?;
                    match edited {
                        Ok(Some((title, content))) => {
                            t.title = title;
                            t.content = content;
                            app.update(t);
                        }
                        Ok(None) => (),
                        Err(e) => app.message = e.to_string(),
                    }
                }
            } else if app.key(key) {
                break;
            }
        }
    }
    Ok(())
}
fn input_view(input: &str, cursor: usize, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut start = 0;
    let chars = input.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    while start < cursor
        && UnicodeWidthStr::width(chars[start..cursor].iter().collect::<String>().as_str())
            >= width.saturating_sub(1)
    {
        start += 1;
    }
    let tail = chars[start..].iter().collect::<String>();
    input_prompt(&tail, cursor - start)
}
fn input_prompt(input: &str, cursor: usize) -> String {
    let index = input
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(input.len());
    format!("{}█{}", &input[..index], &input[index..])
}
fn edit_input(input: &mut String, cursor: &mut usize, key: KeyEvent) {
    let mut chars = input.chars().collect::<Vec<_>>();
    *cursor = (*cursor).min(chars.len());
    match key.code {
        K::Left => *cursor = cursor.saturating_sub(1),
        K::Right => *cursor = (*cursor + 1).min(chars.len()),
        K::Home => *cursor = 0,
        K::End => *cursor = chars.len(),
        K::Backspace if *cursor > 0 => {
            *cursor -= 1;
            chars.remove(*cursor);
        }
        K::Delete if *cursor < chars.len() => {
            chars.remove(*cursor);
        }
        K::Char('u') if key.modifiers.contains(M::CONTROL) => {
            chars.drain(..*cursor);
            *cursor = 0;
        }
        K::Char('w') if key.modifiers.contains(M::CONTROL) => {
            let end = *cursor;
            while *cursor > 0 && chars[*cursor - 1].is_whitespace() {
                *cursor -= 1;
            }
            while *cursor > 0 && !chars[*cursor - 1].is_whitespace() {
                *cursor -= 1;
            }
            chars.drain(*cursor..end);
        }
        K::Char(c) if !key.modifiers.contains(M::CONTROL) => {
            chars.insert(*cursor, c);
            *cursor += 1;
        }
        _ => (),
    }
    *input = chars.into_iter().collect();
}
fn edit(task: &Task) -> Result<Option<(String, String)>> {
    use std::io::Write;
    let mut file = tempfile::Builder::new().suffix(".md").tempfile()?;
    write!(file, "{}\n\n{}", task.title, task.content)?;
    file.flush()?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or("vi".into());
    // Execute the configured editor as a program; arguments can be supplied through an editor wrapper.
    let status = std::process::Command::new(&editor)
        .arg(file.path())
        .status()
        .context("Cannot launch editor; set EDITOR to an executable or wrapper script")?;
    if !status.success() {
        anyhow::bail!("Editor exited unsuccessfully; task unchanged");
    }
    let text = std::fs::read_to_string(file.path())?;
    let (title, content) = text.split_once('\n').unwrap_or((&text, ""));
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("Title cannot be empty");
    }
    let content = content
        .strip_prefix('\n')
        .unwrap_or(content)
        .trim_end()
        .to_owned();
    if title == task.title && content == task.content {
        Ok(None)
    } else {
        Ok(Some((title.into(), content)))
    }
}
fn demo_data() -> Snapshot {
    use crate::api::ChecklistItem;
    Snapshot {
        projects:vec![Project{id:"work".into(),name:"Projetos".into(),closed:false,group_id:Some("work-group".into()),..Default::default()},Project{id:"personal".into(),name:"Leitura".into(),closed:false,group_id:Some("personal-group".into()),..Default::default()}],
        tasks:vec![
            Task{id:"launch".into(),project_id:"work".into(),title:"Preparar lançamento do produto".into(),priority:5,content:"## Um espaço para pensar\n\nOrganize o lançamento com **clareza**, sem sair do terminal. Cada detalhe fica junto da tarefa.\n\n### Antes de publicar\n- [x] Revisar a proposta com o time\n- [ ] Validar a experiência no celular\n- [ ] Escrever as notas de lançamento\n\n> Pequenas entregas. Progresso visível.\n\n### Referências\nLeia o [guia do projeto](https://example.com/guia) e use `cargo test` antes de publicar.\n\n```rust\nfn main() {\n    println!(\"Pronto para lançar\");\n}\n```".into(),items:vec![ChecklistItem{id:"c1".into(),title:"Aprovar identidade visual".into(),status:1,..Default::default()},ChecklistItem{id:"c2".into(),title:"Conferir métricas".into(),..Default::default()}],..Default::default()},
            Task{id:"design".into(),project_id:"work".into(),parent_id:Some("launch".into()),title:"Revisar interface".into(),content:"### Interface\n- [ ] Conferir contraste\n- [ ] Navegar só com o teclado".into(),..Default::default()},
            Task{id:"mobile".into(),project_id:"work".into(),parent_id:Some("design".into()),title:"Conferir versão mobile".into(),..Default::default()},
            Task{id:"small".into(),project_id:"work".into(),parent_id:Some("mobile".into()),title:"Testar em uma tela pequena".into(),..Default::default()},
            Task{id:"notes".into(),project_id:"work".into(),title:"Escrever notas da próxima versão".into(),content:"## Notas\nConte o que mudou e como isso ajuda quem usa o produto.".into(),priority:1,..Default::default()},
            Task{id:"read".into(),project_id:"personal".into(),title:"Ler um capítulo".into(),content:"### Leitura da semana\n- [ ] Separar 20 minutos\n- [ ] Fazer anotações".into(),..Default::default()}
        ],
        groups:vec![crate::api::ProjectGroup{id:"work-group".into(),name:"Trabalho".into(),sort_order:0},crate::api::ProjectGroup{id:"personal-group".into(),name:"Pessoal".into(),sort_order:1}],
        inbox_id:Some("inbox".into()), time_zone:Some("America/Recife".into()),..Default::default()
    }
}
fn body_area(area: Rect) -> Rect {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area)[1]
}
fn preview_area(area: Rect, app: &App) -> Rect {
    let body = body_area(area);
    if app.preview_focus && (app.zoom || body.width < 110) {
        return body;
    }
    if app.details && body.width >= 110 {
        let sidebar = if app.hide_lists { 0 } else if body.width >= 150 { 25 } else { 21 };
        return Layout::horizontal([
            Constraint::Length(sidebar),
            Constraint::Percentage(app.split),
            Constraint::Min(40),
        ])
        .split(body)[2];
    }
    Rect::default()
}
fn draw(f: &mut Frame, app: &App, c: &Config) {
    let t = &c.theme;
    let fg = preview::color(&t.foreground);
    let muted = preview::color(&t.muted);
    let accent = preview::color(&t.accent);
    let panel = preview::color(&t.panel);
    let selection = preview::color(&t.selection);
    let bg = preview::color(&t.background);
    let border = preview::color(&t.border);
    f.render_widget(
        Block::default().style(Style::default().bg(bg).fg(fg)),
        f.area(),
    );
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(f.area());
    let header = Layout::horizontal([
        Constraint::Length(23),
        Constraint::Min(0),
        Constraint::Length(24),
    ])
    .split(rows[0].inner(Margin::new(1, 1)));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ◉ ", Style::default().fg(accent)),
            Span::styled(
                "TickTick TUI",
                Style::default().fg(preview::color(&t.heading)).bold(),
            ),
        ])),
        header[0],
    );
    let current = app.sidebar();
    let project = current
        .get(app.project)
        .map(|e| e.name.as_str())
        .unwrap_or("Todas as tarefas");
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(project, Style::default().fg(muted)),
            Span::styled(
                format!("  /  {} pendentes", app.visible().len()),
                Style::default().fg(muted),
            ),
        ])),
        header[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            if app.demo {
                "DEMO · local"
            } else if app.busy {
                "◌ Sincronizando"
            } else {
                "● Sincronizado"
            },
            Style::default().fg(if app.busy {
                accent
            } else {
                preview::color(&t.success)
            }),
        ))
        .alignment(Alignment::Right),
        header[2],
    );
    let block = |title: String, active: bool| {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(title)
            .title_style(
                Style::default()
                    .fg(if active { accent } else { muted })
                    .bold(),
            )
            .border_style(Style::default().fg(if active { accent } else { border }))
            .style(Style::default().bg(panel))
    };
    let preview = preview_area(f.area(), app);
    let fullscreen = app.preview_focus && (app.zoom || rows[1].width < 110);
    if !fullscreen {
        let cols = if preview.width > 0 {
            Layout::horizontal([
                Constraint::Length(if app.hide_lists { 0 } else if rows[1].width >= 150 { 25 } else { 21 }),
                Constraint::Percentage(app.split),
                Constraint::Min(40),
            ])
            .split(rows[1])
        } else {
            Layout::horizontal([
                Constraint::Length(if app.hide_lists { 0 } else if rows[1].width < 55 { 17 } else { 25 }),
                Constraint::Min(0),
            ])
            .split(rows[1])
        };
        let entries = app.sidebar();
        let projects = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let marker = match &e.view {
                    View::All => "≡ ",
                    View::Today => "◷ ",
                    View::Inbox => "▣ ",
                    View::Upcoming => "▦ ",
                    View::Group(id) => {
                        if app.collapsed.contains(&format!("group:{id}")) {
                            "▸ "
                        } else {
                            "▾ "
                        }
                    }
                    View::Project(_) => "  ",
                };
                let mut lines = vec![Line::from(vec![
                    Span::styled(
                        format!(" {}{marker}", "  ".repeat(e.depth)),
                        Style::default().fg(if i < 4 { accent } else { muted }),
                    ),
                    Span::raw(e.name.clone()),
                    Span::styled(format!("  {}", e.count), Style::default().fg(muted)),
                ])];
                if i == 3 {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled(
                        "  LISTAS",
                        Style::default().fg(muted).bold(),
                    )));
                    lines.push(Line::default());
                }
                ListItem::new(lines)
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(app.project));
        f.render_stateful_widget(
            List::new(projects)
                .block(
                    block(" 1 · Listas ".into(), app.projects_focus).padding(Padding::vertical(1)),
                )
                .highlight_style(Style::default().bg(selection).fg(accent))
                .highlight_symbol("▎")
                .repeat_highlight_symbol(true),
            cols[0],
            &mut state,
        );
        let tree = app.tree();
        let (tz, today) = (app.tz(), sidebar::today(&app.data));
        let danger = preview::color(&t.danger);
        // Borders, padding and the highlight symbol take 5 columns.
        let row_width = cols[1].width.saturating_sub(5) as usize;
        let items = tree
            .iter()
            .map(|row| {
                let task = row.task;
                // Cap visual indentation to available space, never the actual tree depth.
                let indent =
                    "  ".repeat(row.depth.min(cols[1].width.saturating_sub(20) as usize / 2));
                let branch = if row.children > 0 {
                    if app.collapsed.contains(&task.id) {
                        "▸ "
                    } else {
                        "▾ "
                    }
                } else if row.depth > 0 {
                    "└ "
                } else {
                    ""
                };
                let pill = |text: String, color: Color| {
                    Span::styled(format!("  {text}"), Style::default().fg(color))
                };
                let mut badges = Vec::new();
                match task.priority {
                    5 => badges.push(pill("● alta".into(), danger)),
                    3 => badges.push(pill("● média".into(), accent)),
                    1 => badges.push(pill("● baixa".into(), muted)),
                    _ => (),
                }
                if !task.content.is_empty() {
                    badges.push(pill("≡".into(), muted));
                }
                if !task.items.is_empty() {
                    let done = task.items.iter().filter(|i| i.status == 1).count();
                    badges.push(pill(format!("✓ {done}/{}", task.items.len()), muted));
                }
                if row.children > 0 && app.collapsed.contains(&task.id) {
                    badges.push(pill(format!("+{}", row.children), muted));
                }
                if task.extra.get("reminders").and_then(|v| v.as_array()).is_some_and(|r| !r.is_empty()) {
                    badges.push(pill("⏰".into(), muted));
                }
                let all_day = task.extra.get("isAllDay").and_then(|v| v.as_bool()) == Some(true);
                if let Some((label, overdue)) = task
                    .due_date
                    .as_deref()
                    .and_then(|d| fields::relative_due(d, all_day, tz, today))
                {
                    badges.push(pill(label, if overdue { danger } else { accent }));
                }
                let left = format!(" {indent}{branch}○ ");
                let right: usize = badges.iter().map(Span::width).sum();
                let title = truncate(&task.title, row_width.saturating_sub(left.width() + right + 1));
                let pad = row_width.saturating_sub(left.width() + title.width() + right);
                let mut line = vec![
                    Span::styled(left, Style::default().fg(muted)),
                    Span::raw(title),
                    Span::raw(" ".repeat(pad)),
                ];
                line.extend(badges);
                ListItem::new(Line::from(line))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(if tree.is_empty() {
            None
        } else {
            Some(app.selected)
        });
        let title = if app.query.is_empty() {
            format!(" 2 · Tarefas  {} ", tree.len())
        } else {
            format!(" 2 · Busca /{} ", app.query)
        };
        let taskblock = block(title, !app.projects_focus && !app.preview_focus)
            .padding(Padding::new(1, 1, 1, 0));
        let inner = taskblock.inner(cols[1]);
        f.render_stateful_widget(
            List::new(items)
                .block(taskblock)
                .highlight_style(Style::default().bg(selection))
                .highlight_symbol("▎")
                .repeat_highlight_symbol(true),
            cols[1],
            &mut state,
        );
        if tree.is_empty() {
            f.render_widget(
                Paragraph::new(if app.busy {
                    "Buscando suas tarefas…"
                } else {
                    "Tudo em dia.\n\nSelecione uma lista e pressione a\npara adicionar uma tarefa."
                })
                .style(Style::default().fg(muted))
                .wrap(Wrap { trim: false }),
                inner.inner(Margin::new(1, 1)),
            );
        }
    }
    if preview.width > 0 {
        let document = app.preview(t);
        let preview_block = block(
            format!(
                " 3 · Preview{} ",
                if app.preview_focus {
                    " · j/k navegar"
                } else {
                    ""
                }
            ),
            app.preview_focus,
        )
        .padding(Padding::new(2, 2, 1, 1));
        let inner = preview_block.inner(preview);
        f.render_widget(preview_block, preview);
        if document.is_empty() {
            f.render_widget(
                Paragraph::new("Selecione uma tarefa para visualizar.")
                    .style(Style::default().fg(muted)),
                inner,
            );
        } else {
            preview::render(
                f,
                inner,
                &document,
                app.preview_cursor,
                app.preview_focus,
                t,
            );
        }
    }
    f.render_widget(
        Paragraph::new(format!(" {}", app.message)).style(Style::default().fg(muted)),
        rows[2],
    );
    let prompt = match app.mode {
        Mode::Search => format!(
            " /{}",
            input_view(
                &app.input,
                app.input_cursor,
                f.area().width.saturating_sub(4) as usize
            )
        ),
        Mode::Command => format!(
            " :{}",
            input_view(
                &app.input,
                app.input_cursor,
                f.area().width.saturating_sub(4) as usize
            )
        ),
        Mode::Add => format!(
            " INSERIR  Nova tarefa: {}",
            input_prompt(&app.input, app.input_cursor)
        ),
        Mode::Subtask => format!(
            " INSERIR  Subtarefa: {}",
            input_prompt(&app.input, app.input_cursor)
        ),
        Mode::Item => format!(
            " CHECKLIST  Novo item: {}   Enter adiciona outro · Esc termina",
            input_prompt(&app.input, app.input_cursor)
        ),
        Mode::ItemRename => format!(
            " CHECKLIST  Item: {}",
            input_prompt(&app.input, app.input_cursor)
        ),
        Mode::Rename => format!(
            " INSERIR  Título: {}",
            input_prompt(&app.input, app.input_cursor)
        ),
        Mode::Move => {
            let names = app
                .lists(&app.input)
                .into_iter()
                .take(4)
                .map(|(_, name)| name)
                .collect::<Vec<_>>();
            format!(
                " MOVER  Lista: {}   → {}",
                input_prompt(&app.input, app.input_cursor),
                if names.is_empty() { "nenhuma lista".into() } else { names.join(" · ") }
            )
        }
        Mode::Due => " PRAZO  j/k escolher · Enter salvar · Tab completar · Esc cancelar".into(),
        Mode::Remind => " LEMBRETES  j/k escolher · Space marcar · Enter salvar · Esc cancelar".into(),
        Mode::Delete => {
            " EXCLUIR  Confirmar exclusão permanente? y / qualquer tecla cancela".into()
        }
        _ => {
            if app.preview_focus {
                " PREVIEW  j/k navegar · f zoom · a novo item · cc renomear · dd apagar · Space marcar · h voltar".into()
            } else {
                " NORMAL  h/j/k/l navegar · a nova · m mover · D prazo · R lembrete · za recolher · ? ajuda".into()
            }
        }
    };
    f.render_widget(
        Paragraph::new(prompt).style(Style::default().bg(selection).fg(accent)),
        rows[3],
    );
    if matches!(app.mode, Mode::Due | Mode::Remind) {
        let due = app.mode == Mode::Due;
        let chosen = app.input.split(',').map(str::trim).collect::<Vec<_>>();
        let mut lines = app
            .options()
            .iter()
            .enumerate()
            .map(|(i, (label, value))| {
                let mark = match (due, chosen.contains(value)) {
                    (true, _) => "",
                    (false, true) => "● ",
                    (false, false) => "○ ",
                };
                let style = if i == app.pick {
                    Style::default().bg(selection).fg(accent)
                } else {
                    Style::default()
                };
                Line::styled(format!(" {mark}{label:<width$}", width = 36), style)
            })
            .collect::<Vec<_>>();
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(
                if due { " Outra data: " } else { " Selecionados: " },
                Style::default().fg(muted),
            ),
            Span::raw(input_prompt(&app.input, app.input_cursor)),
        ]));
        lines.push(Line::styled(
            if due {
                " ex.: 25/12 · sex 14:30 · +3 · amanhã 09:00"
            } else {
                " ou digite: 2h, 3d"
            },
            Style::default().fg(muted),
        ));
        let current = app.target.as_ref().and_then(|t| {
            let all_day = t.extra.get("isAllDay").and_then(|v| v.as_bool()) == Some(true);
            t.due_date.as_deref().map(|d| fields::format_due(d, all_day, app.tz()))
        });
        let title = match (due, current) {
            (true, Some(c)) => format!(" Prazo · atual {c} "),
            (true, None) => " Prazo ".into(),
            (false, _) => " Lembretes ".into(),
        };
        let area = centered(f.area(), 44, lines.len() as u16 + 4);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(panel).fg(fg))
                .block(block(title, true).padding(Padding::new(1, 1, 1, 0))),
            area,
        );
    }
    if app.mode == Mode::Help {
        let area = centered(f.area(), 84, 31);
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new("\nNAVEGAÇÃO\nh / l, Tab     Listas → tarefas → preview\nj / k, 5j      Navegar com contagem       gg / G    Início / fim\nCtrl-d / u     Descer / subir 10 linhas   Enter     Focar preview\nv              Mostrar / ocultar preview  Esc / h   Voltar às tarefas\nf              Zoom no preview            < / >     Redimensionar colunas\nb              Ocultar / mostrar listas\n\nTAREFAS\na / i / o      Nova tarefa na lista       A         Nova subtarefa\nza / zo / zc   Alternar / abrir / recolher árvore\ncc             Renomear                   e         Editar no Vim/$EDITOR\nm              Mover para outra lista     D / R     Prazo / lembretes\nx / Space      Concluir tarefa            dd        Excluir (confirmar y)\np              Alternar prioridade        /         Buscar com ancestrais\n\nPREVIEW\nj / k          Navegar e rolar o Markdown formatado\n] / [          Próximo / anterior checkbox ou subtarefa\nSpace / x      Marcar checkbox, checklist ou concluir subtarefa\nEnter          Marcar checkbox / abrir subtarefa\na / cc / dd    Novo item / renomear / apagar item do checklist\n\nCOMANDOS\n:token         Configurar API token       :subtask  Nova subtarefa\n:theme dark    Tema escuro                :theme light  Tema claro\n:sync / :w     Sincronizar                :config   Caminho da configuração\n:noh           Limpar busca               :q / q    Sair\n\nQualquer tecla fecha a ajuda.").style(Style::default().bg(panel).fg(fg)).wrap(Wrap{trim:false}).block(block(" Atalhos ".into(),true).padding(Padding::horizontal(2))),area);
    }
}
const DUE: [(&str, &str); 4] = [
    ("Hoje", "hoje"),
    ("Amanhã", "amanhã"),
    ("Próxima semana", "+7"),
    ("Sem prazo", ""),
];
const REMIND: [(&str, &str); 6] = [
    ("No horário", "0"),
    ("5 min antes", "5m"),
    ("15 min antes", "15m"),
    ("30 min antes", "30m"),
    ("1 hora antes", "1h"),
    ("1 dia antes", "1d"),
];
fn truncate(text: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= width {
        return text.into();
    }
    let mut out = String::new();
    let mut used = 1; // reserve the ellipsis
    for c in text.chars() {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        out.push(c);
    }
    out + "…"
}
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        let mut a = App {
            confirmed: Snapshot::default(),
            pending_jobs: VecDeque::new(),
            in_flight: 0,
            data: Snapshot {
                projects: vec![Project {
                    id: "p".into(),
                    name: "Work".into(),
                    closed: false,
                    ..Default::default()
                }],
                tasks: (0..20)
                    .map(|i| Task {
                        id: i.to_string(),
                        project_id: "p".into(),
                        title: format!("Task {i}"),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            project: 0,
            selected: 0,
            projects_focus: false,
            preview_focus: false,
            preview_cursor: 0,
            preview_width: 60,
            split: 34,
            zoom: false,
            hide_lists: false,
            collapsed: HashSet::new(),
            pick: 0,
            theme_request: None,
            mode: Mode::Normal,
            input: String::new(),
            input_cursor: 0,
            search_original: String::new(),
            target: None,
            query: String::new(),
            message: String::new(),
            busy: false,
            demo: true,
            pending: '\0',
            count: String::new(),
            details: true,
            last_sync: Instant::now(),
            tx: None,
            rx: None,
        };
        a.confirmed = a.data.clone();
        a
    }
    fn press(a: &mut App, c: char) {
        a.key(KeyEvent::new(K::Char(c), M::NONE));
    }
    #[test]
    fn vim_counts_boundaries_and_project_focus() {
        let mut a = app();
        press(&mut a, '5');
        press(&mut a, 'j');
        assert_eq!(a.selected, 5);
        press(&mut a, 'G');
        assert_eq!(a.selected, 19);
        press(&mut a, 'g');
        press(&mut a, 'g');
        assert_eq!(a.selected, 0);
        press(&mut a, 'h');
        press(&mut a, 'j');
        assert_eq!(a.project, 1);
        assert_eq!(a.selected, 0);
        press(&mut a, 'l');
        press(&mut a, 'k');
        assert_eq!(a.selected, 0);
    }
    #[test]
    fn delete_requires_confirmation_and_escape_cancels_input() {
        let mut a = app();
        press(&mut a, 'd');
        press(&mut a, 'd');
        assert!(a.mode == Mode::Delete);
        a.key(KeyEvent::new(K::Esc, M::NONE));
        assert!(a.mode == Mode::Normal);
        assert!(a.message.is_empty());
        press(&mut a, 'a');
        press(&mut a, 'X');
        a.key(KeyEvent::new(K::Esc, M::NONE));
        assert!(a.input.is_empty());
        assert_eq!(a.data.tasks.len(), 20);
    }
    #[test]
    fn search_is_case_insensitive_and_clamps_selection() {
        let mut a = app();
        a.selected = 19;
        press(&mut a, '/');
        a.input = "TASK 3".into();
        a.key(KeyEvent::new(K::Enter, M::NONE));
        assert_eq!(a.visible().len(), 1);
        assert_eq!(a.selected, 0);
        assert_eq!(a.task().unwrap().title, "Task 3");
    }
    #[test]
    fn preview_adds_renames_and_deletes_checklist_items() {
        let mut a = app();
        a.focus(2);
        for c in "aum\rdois\r\r".chars() {
            let code = if c == '\r' { K::Enter } else { K::Char(c) };
            a.key(KeyEvent::new(code, M::NONE));
        }
        assert!(a.mode == Mode::Normal);
        let titles = |a: &App| a.task().unwrap().items.iter().map(|i| i.title.clone()).collect::<Vec<_>>();
        assert_eq!(titles(&a), ["um", "dois"]);
        a.preview_cursor = a
            .preview(&config::Theme::default())
            .iter()
            .position(|r| matches!(r.action, Some(Action::Checklist(1))))
            .unwrap();
        press(&mut a, 'c');
        press(&mut a, 'c');
        a.key(KeyEvent::new(K::Char('!'), M::NONE));
        a.key(KeyEvent::new(K::Enter, M::NONE));
        assert_eq!(titles(&a), ["um", "dois!"]);
        press(&mut a, 'd');
        press(&mut a, 'd');
        assert_eq!(titles(&a), ["um"]);
    }
    #[test]
    fn small_terminal_and_help_render() {
        let mut a = app();
        a.mode = Mode::Help;
        for (w, h) in [(120, 32), (40, 12), (10, 4)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
        }
    }
    #[test]
    fn refresh_preserves_selection_and_delete_target() {
        let mut a = app();
        a.selected = 3;
        press(&mut a, 'd');
        press(&mut a, 'd');
        let mut data = a.data.clone();
        data.tasks.reverse();
        a.replace_snapshot(data);
        assert_eq!(a.task().unwrap().id, "3");
        // Even if the task vanishes in the refresh, confirmation keeps its original ID.
        let mut data = a.data.clone();
        data.tasks.retain(|t| t.id != "3");
        a.replace_snapshot(data);
        let (tx, rx) = std::sync::mpsc::channel();
        a.tx = Some(tx);
        a.demo = false;
        press(&mut a, 'y');
        match rx.try_recv().unwrap() {
            Job::Delete(t) => assert_eq!(t.id, "3"),
            _ => panic!("Expected delete"),
        }
    }
}
