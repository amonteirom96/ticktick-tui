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
use std::{
    collections::{HashSet, VecDeque},
    io,
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

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
    Visual,
    /// Picks the task tracked by the focus timer.
    FocusTask,
    /// Edits the pomodoro focus / break / long break minutes.
    FocusConfig,
}
struct App {
    timer: crate::wellness::Timer,
    data: Snapshot,
    confirmed: Snapshot,
    pending_jobs: VecDeque<Job>,
    in_flight: usize,
    project: usize,
    report_day: chrono::NaiveDate,
    report_loading: std::collections::HashMap<chrono::NaiveDate, usize>,
    selected: usize,
    /// Fixed end of the visual selection; `selected` is the moving end.
    anchor: usize,
    projects_focus: bool,
    preview_focus: bool,
    preview_cursor: usize,
    preview_width: usize,
    /// Task column width (%) when the preview is visible.
    split: u16,
    /// Extra sidebar width (columns), changed with `<`/`>` while the list is focused.
    lists: i16,
    zoom: bool,
    hide_lists: bool,
    collapsed: HashSet<String>,
    /// Highlighted option in the due/reminder pickers, or where the next checklist item goes.
    pick: usize,
    /// Preview entry being renamed with `cc`.
    item: Option<Action>,
    hide_done: bool,
    show_completed: bool,
    /// Inverse patches of `update`, newest last.
    undo: Vec<Job>,
    theme_request: Option<String>,
    mode: Mode,
    input: String,
    input_cursor: usize,
    search_original: String,
    target: Option<Task>,
    query: String,
    message: String,
    busy: bool,
    /// Quit requested while syncing: exit once `busy` clears.
    quitting: bool,
    demo: bool,
    /// Open audio output; `None` when sounds are off or no device exists.
    audio: Option<rodio::MixerDeviceSink>,
    pending: char,
    count: String,
    details: bool,
    last_sync: Instant,
    tx: Option<Sender<Job>>,
    rx: Option<Receiver<sync::Event>>,
}
impl App {
    fn persist_timer(&mut self) {
        if self.demo {
            return;
        }
        if let Err(error) = self.timer.save() {
            self.message = format!("Timer em memória; falha ao salvar estado local: {error}");
            return;
        }
        for upload in self.timer.ready_uploads() {
            self.timer.mark_attempted(&upload.id);
            if let Err(error) = self.timer.save() {
                self.message = format!("Falha ao salvar envio pendente de foco: {error}");
                return;
            }
            if self
                .tx
                .as_ref()
                .is_some_and(|tx| tx.send(Job::SaveFocus(upload)).is_ok())
            {
                self.in_flight += 1;
                self.busy = true;
                self.message = "Salvando sessão de foco no TickTick…".into();
            } else {
                self.message =
                    "Foco pendente · reconecte e use :focus-retry após conferir o histórico".into();
            }
        }
    }

    fn sidebar(&self) -> Vec<sidebar::Entry> {
        sidebar::entries(&self.data, &self.collapsed)
    }
    fn view(&self) -> View {
        self.sidebar()
            .get(self.project)
            .map(|e| e.view.clone())
            .unwrap_or(View::Today)
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
            let fields: serde_json::Map<_, _> = new
                .as_object()
                .unwrap()
                .iter()
                .filter(|(k, v)| {
                    old.get(*k) != Some(*v) && k.as_str() != "id" && k.as_str() != "projectId"
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if fields.is_empty() {
                return;
            }
            let inverse = fields
                .keys()
                .map(|k| (k.clone(), old.get(k).cloned().unwrap_or_default()))
                .collect();
            self.undo.push(Job::Patch {
                id: task.id.clone(),
                project: task.project_id.clone(),
                fields: inverse,
            });
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
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
        if let Some(id) = task.parent_id.as_ref() {
            self.collapsed.remove(id);
        }
        self.open_list(&project);
        let id = task.id.clone();
        self.send(Job::Create(task));
        self.selected = self.visible().iter().position(|t| t.id == id).unwrap_or(0);
        self.preview_cursor = 0;
    }
    /// Selects the sidebar list `project` (Inbox if it has no entry), expanding its group.
    fn open_list(&mut self, project: &str) {
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
        self.project = self
            .sidebar()
            .iter()
            .position(|e| e.view == View::Project(project.to_owned()))
            .unwrap_or_else(|| {
                self.sidebar()
                    .iter()
                    .position(|e| e.view == View::Inbox)
                    .unwrap_or(0)
            });
    }
    /// Tasks of the current view in TickTick's manual order.
    fn view_tasks(&self) -> Vec<&Task> {
        let view = self.view();
        let mut tasks = self
            .data
            .tasks
            .iter()
            .filter(|t| sidebar::matches(&view, t, &self.data))
            .collect::<Vec<_>>();
        if self.show_completed {
            let mut seen: HashSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
            for task in self.data.completed.iter().rev() {
                if sidebar::matches(&view, task, &self.data) && seen.insert(task.id.clone()) {
                    tasks.push(task);
                }
            }
        }
        tasks.sort_by_key(|t| (t.status != 0, t.sort_order));
        tasks
    }
    fn tree(&self) -> Vec<tree::Row<'_>> {
        tree::rows(&self.view_tasks(), &self.collapsed, &self.query)
    }
    /// Visual selection as an inclusive row range.
    fn range(&self) -> (usize, usize) {
        let last = self.visible().len().saturating_sub(1);
        (
            self.anchor.min(self.selected).min(last),
            self.anchor.max(self.selected).min(last),
        )
    }
    /// Moves the selected siblings `delta` places, reusing the siblings' own sortOrder values.
    fn shift(&mut self, delta: isize) {
        let (lo, hi) = self.range();
        let rows = self.tree();
        let Some(depth) = rows
            .get(lo..=hi)
            .and_then(|r| r.iter().map(|r| r.depth).min())
        else {
            return;
        };
        // Shallowest rows of a contiguous range are siblings; their subtasks follow them.
        let block = rows[lo..=hi]
            .iter()
            .filter(|r| r.depth == depth)
            .map(|r| r.task.clone())
            .collect::<Vec<_>>();
        drop(rows);
        let mut siblings = self
            .view_tasks()
            .into_iter()
            .filter(|t| t.parent_id == block[0].parent_id)
            .cloned()
            .collect::<Vec<_>>();
        let start = siblings
            .iter()
            .position(|t| t.id == block[0].id)
            .unwrap_or(0);
        if siblings[start..]
            .iter()
            .take(block.len())
            .map(|t| &t.id)
            .ne(block.iter().map(|t| &t.id))
        {
            self.message = "Selecione tarefas do mesmo nível para mover.".into();
            return;
        }
        let mut orders = siblings.iter().map(|t| t.sort_order).collect::<Vec<_>>();
        for i in 1..orders.len() {
            orders[i] = orders[i].max(orders[i - 1].saturating_add(1));
        }
        let moved = siblings
            .drain(start..start + block.len())
            .collect::<Vec<_>>();
        let at = start.saturating_add_signed(delta).min(siblings.len());
        siblings.splice(at..at, moved);
        for (mut t, order) in siblings.into_iter().zip(orders) {
            if t.sort_order != order {
                t.sort_order = order;
                self.update(t);
            }
        }
        let visible = self.visible();
        let pos = |id: &str| visible.iter().position(|t| t.id == id);
        if let (Some(first), Some(last)) = (pos(&block[0].id), pos(&block[block.len() - 1].id)) {
            (self.anchor, self.selected) = if self.selected >= self.anchor {
                (first, last)
            } else {
                (last, first)
            };
        }
    }
    /// `L`: subtask of the sibling above; `H`: up one level. "" parentId = root.
    fn reparent(&mut self, indent: bool) {
        let rows = self.tree();
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        let (mut task, depth) = (row.task.clone(), row.depth);
        let mut above = rows[..self.selected].iter().rev();
        let parent = if indent {
            above
                .take_while(|r| r.depth >= depth)
                .find(|r| r.depth == depth)
                .map(|r| r.task.id.clone())
        } else {
            above.find(|r| r.depth < depth).map(|r| {
                r.task
                    .parent_id
                    .clone()
                    .filter(|_| depth > 1)
                    .unwrap_or_default()
            })
        };
        drop(rows);
        let Some(parent) = parent else {
            self.message = if indent {
                "Nenhuma tarefa acima no mesmo nível."
            } else {
                "Já está no primeiro nível."
            }
            .into();
            return;
        };
        self.collapsed.remove(&parent);
        task.parent_id = Some(parent);
        let id = task.id.clone();
        self.update(task);
        self.selected = self
            .visible()
            .iter()
            .position(|t| t.id == id)
            .unwrap_or(self.selected);
    }
    fn visible(&self) -> Vec<&Task> {
        self.tree().into_iter().map(|r| r.task).collect()
    }
    fn preview(&self, theme: &config::Theme) -> Vec<preview::Row> {
        self.task()
            .map(|t| {
                if self.view() == View::Report {
                    let mut data = self.data.clone();
                    data.tasks = self.report_tasks().into_iter().cloned().collect();
                    preview::document(&t, &data, theme, self.preview_width, false)
                } else {
                    preview::document(&t, &self.data, theme, self.preview_width, self.hide_done)
                }
            })
            .unwrap_or_default()
    }
    fn focus(&mut self, pane: u8) {
        if pane == 2
            && self.view() == View::Report
            && (self.task().is_none() || self.report_loading.contains_key(&self.report_day))
        {
            return;
        }
        let entering = pane == 2 && !self.preview_focus;
        self.projects_focus = pane == 0;
        self.preview_focus = pane == 2;
        self.zoom &= self.preview_focus;
        self.hide_lists &= !self.projects_focus;
        if self.preview_focus {
            self.details = true;
        }
        if entering
            && self.view() != View::Report
            && let Some(&i) = self.open_boxes().first()
        {
            self.preview_cursor = i;
        }
    }
    /// Preview rows whose checkbox (Markdown task or checklist item) is still open.
    fn open_boxes(&self) -> Vec<usize> {
        let Some(task) = self.task() else {
            return vec![];
        };
        self.preview(&config::Theme::default())
            .iter()
            .enumerate()
            .filter(|(_, r)| match &r.action {
                Some(Action::Markdown { checked, .. }) => !checked,
                Some(Action::Checklist(i)) => task.items.get(*i).is_some_and(|i| i.status != 1),
                _ => false,
            })
            .map(|(i, _)| i)
            .collect()
    }
    /// Preview j/k: next/previous checkbox or subtask; scrolls a line when none is left that way.
    fn jump(&mut self, delta: isize) {
        let rows = self.preview(&config::Theme::default());
        for _ in 0..delta.unsigned_abs() {
            let here = rows
                .get(self.preview_cursor)
                .and_then(|r| r.action.as_ref());
            // Wrapped rows repeat their action, so skip rows of the current entry.
            let other = |r: &&preview::Row| r.action.is_some() && r.action.as_ref() != here;
            let found = if delta > 0 {
                rows.iter()
                    .skip(self.preview_cursor + 1)
                    .position(|r| other(&r))
                    .map(|i| self.preview_cursor + 1 + i)
            } else {
                rows[..self.preview_cursor.min(rows.len())]
                    .iter()
                    .rposition(|r| other(&r))
                    .map(|mut i| {
                        while i > 0 && rows[i - 1].action == rows[i].action {
                            i -= 1;
                        }
                        i
                    })
            };
            self.preview_cursor = found.unwrap_or_else(|| {
                self.preview_cursor
                    .saturating_add_signed(delta.signum())
                    .min(rows.len().saturating_sub(1))
            });
        }
    }
    fn cursor_to(&mut self, action: Action) {
        if let Some(i) = self
            .preview(&config::Theme::default())
            .iter()
            .position(|r| r.action.as_ref() == Some(&action))
        {
            self.preview_cursor = i;
        }
    }
    /// Inserts checklist items at `pick` into `target` and keeps the cursor on the last one.
    fn insert_items(&mut self, items: &[(String, bool)]) {
        let Some(mut t) = self.target.take() else {
            return;
        };
        for (title, done) in items {
            self.pick = self.pick.min(t.items.len());
            t.items.insert(self.pick, new_item(title, *done));
            self.pick += 1;
        }
        renumber(&mut t.items);
        self.update(t);
        self.target = self.task();
        self.cursor_to(Action::Checklist(self.pick - 1));
    }
    fn undo(&mut self) {
        match self.undo.pop() {
            Some(job) => {
                self.send(job);
                self.message = format!("Desfeito · {} restante(s)", self.undo.len());
            }
            None => self.message = "Nada para desfazer.".into(),
        }
    }
    /// `gs`: checklist item / Markdown task → subtask, or a plain subtask → checklist item.
    fn convert(&mut self) {
        let (Some(mut t), Some(action)) = (self.task(), self.cursor_action()) else {
            return;
        };
        if let Action::Subtask(id) = action {
            let Some(sub) = self.data.tasks.iter().find(|s| s.id == id).cloned() else {
                return;
            };
            let has_children = self
                .data
                .tasks
                .iter()
                .any(|c| c.parent_id.as_deref() == Some(&id));
            if !sub.content.trim().is_empty() || !sub.items.is_empty() || has_children {
                self.message =
                    "Subtarefa com conteúdo, checklist ou filhas; não convertida.".into();
                return;
            }
            t.items.push(new_item(&sub.title, false));
            renumber(&mut t.items);
            self.update(t);
            self.send(Job::Delete(sub));
            self.cursor_to(Action::Checklist(
                self.task().map_or(0, |t| t.items.len() - 1),
            ));
            return;
        }
        let Some(title) = preview::label(&t, &action) else {
            return;
        };
        if preview::remove(&mut t, &action) {
            let parent = t.id.clone();
            let project = t.project_id.clone();
            self.update(t);
            self.create(project, title, Some(parent.clone()));
            // create() selects the new subtask; stay on the parent so the preview keeps context.
            if let Some(i) = self.visible().iter().position(|v| v.id == parent) {
                self.selected = i;
            }
            self.message = "Convertido em subtarefa.".into();
        }
    }
    /// `gx`: link on the cursor row (preview) or the task's first link.
    fn open_link(&mut self) {
        let rows = self.preview(&config::Theme::default());
        let rows = if self.preview_focus {
            rows.get(self.preview_cursor..=self.preview_cursor)
                .unwrap_or_default()
        } else {
            &rows[..]
        };
        let url = rows.iter().find_map(|r| {
            r.link.clone().or_else(|| {
                let text = r
                    .line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                text.split_whitespace()
                    .find(|w| w.starts_with("http"))
                    .map(str::to_owned)
            })
        });
        // Only web/mail links: `open file:///…` would launch arbitrary local apps.
        self.message = match url.filter(|u| {
            ["http://", "https://", "mailto:"]
                .iter()
                .any(|p| u.starts_with(p))
        }) {
            Some(url) => match std::process::Command::new(if cfg!(target_os = "macos") {
                "open"
            } else if cfg!(windows) {
                "explorer.exe"
            } else {
                "xdg-open"
            })
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            {
                Ok(_) => format!("Abrindo {url}"),
                Err(e) => format!("Não foi possível abrir o link: {e}"),
            },
            None => "Nenhum link aqui.".into(),
        };
    }
    /// Bracketed paste: several lines in checklist mode become several items.
    fn paste(&mut self, text: &str) {
        if self.mode == Mode::Item && self.target.is_some() {
            let items = text
                .lines()
                .filter_map(|l| {
                    let l = l
                        .trim()
                        .trim_start_matches(['-', '*', '+', '•'])
                        .trim_start();
                    let (done, l) = match l.get(..3) {
                        Some("[ ]") => (false, &l[3..]),
                        Some("[x]" | "[X]") => (true, &l[3..]),
                        _ => (false, l),
                    };
                    let l = l.trim();
                    (!l.is_empty()).then(|| (l.to_owned(), done))
                })
                .collect::<Vec<_>>();
            if items.len() > 1 {
                self.insert_items(&items);
                self.message = format!("{} itens adicionados", items.len());
                return;
            }
        }
        if matches!(
            self.mode,
            Mode::Normal | Mode::Visual | Mode::Help | Mode::Delete | Mode::Token
        ) {
            return;
        }
        for c in text.trim().chars() {
            let c = if c.is_control() { ' ' } else { c };
            self.key(KeyEvent::new(K::Char(c), M::NONE));
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
                    // Not in this view (e.g. undated child under Today): jump to its own list.
                    if !self.visible().iter().any(|t| t.id == id)
                        && let Some(p) = self
                            .data
                            .tasks
                            .iter()
                            .find(|t| t.id == id)
                            .map(|t| t.project_id.clone())
                    {
                        self.open_list(&p);
                    }
                    if let Some(i) = self.visible().iter().position(|t| t.id == id) {
                        self.selected = i;
                        self.preview_cursor = 0;
                    }
                } else if let Some(t) = self.data.tasks.iter().find(|t| t.id == id) {
                    self.send(Job::Complete(t.clone()));
                }
            } else if let Some(mut task) = self.task() {
                let checking = match &action {
                    Action::Markdown { checked, .. } => !checked,
                    Action::Checklist(i) => task.items.get(*i).is_some_and(|i| i.status != 1),
                    Action::Subtask(_) => false,
                };
                if preview::toggle(&mut task, &action) {
                    if checking {
                        self.play(include_bytes!("../assets/check.wav"), 0.7);
                    }
                    self.update(task);
                    if checking && self.open_boxes().is_empty() {
                        self.message = "Tudo marcado ✓ · h depois x conclui a tarefa".into();
                    }
                }
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
    /// Open tasks matching `query` for the focus picker; `None` is free focus.
    fn focus_choices(&self, query: &str) -> Vec<Option<crate::wellness::FocusTask>> {
        let query = query.to_lowercase();
        let today = sidebar::today(&self.data);
        let mut tasks = self
            .data
            .tasks
            .iter()
            .filter(|t| t.status == 0 && t.title.to_lowercase().contains(&query))
            .collect::<Vec<_>>();
        // Overdue and today's tasks first, then the rest in list order.
        tasks.sort_by_key(|t| {
            !t.due_date
                .as_deref()
                .and_then(|d| sidebar::date(d, t, &self.data))
                .is_some_and(|d| d <= today)
        });
        query
            .is_empty()
            .then_some(None)
            .into_iter()
            .chain(tasks.into_iter().map(|t| {
                Some(crate::wellness::FocusTask {
                    id: t.id.clone(),
                    title: t.title.clone(),
                })
            }))
            .collect()
    }
    fn project_name(&self, id: &str) -> String {
        if self.data.inbox_id.as_deref() == Some(id) {
            return "Inbox".into();
        }
        self.data
            .projects
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_default()
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
    /// Preview entry under the cursor. Row layout does not depend on colors.
    fn cursor_action(&self) -> Option<Action> {
        self.preview(&config::Theme::default())
            .get(self.preview_cursor)?
            .action
            .clone()
    }
    fn report_tasks(&self) -> Vec<&Task> {
        let mut tasks = self
            .data
            .completed
            .iter()
            .filter(|t| {
                !t.is_note()
                    && t.extra
                        .get("completedTime")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|v| sidebar::date(v, &Task::default(), &self.data))
                        == Some(self.report_day)
            })
            .collect::<Vec<_>>();
        tasks.sort_by_key(|t| {
            t.extra
                .get("completedTime")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        });
        // Preserve completion order between families; keep siblings together.
        let mut families = std::collections::HashMap::new();
        let mut family_order = std::collections::HashMap::new();
        for task in &tasks {
            let mut family = *task;
            let mut seen = HashSet::from([task.id.as_str()]);
            while let Some(parent) = family.parent_id.as_deref().and_then(|id| {
                self.data
                    .tasks
                    .iter()
                    .chain(self.data.completed.iter())
                    .find(|t| t.id == id)
            }) {
                if !seen.insert(parent.id.as_str()) {
                    break;
                }
                family = parent;
            }
            let family = family.id.clone();
            let key = (task.project_id.clone(), family);
            let next = families.len();
            let rank = *families.entry(key).or_insert(next);
            family_order.insert(task.id.as_str(), rank);
        }
        tasks.sort_by_key(|task| family_order[task.id.as_str()]);
        tree::rows(&tasks, &HashSet::new(), "")
            .into_iter()
            .map(|row| row.task)
            .collect()
    }
    fn task(&self) -> Option<Task> {
        if self.view() == View::Report {
            let tasks = self.report_tasks();
            return tasks
                .get(self.selected.min(tasks.len().saturating_sub(1)))
                .map(|t| (*t).clone());
        }
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
        let count = if matches!(self.view(), View::Habits | View::Focus) {
            crate::wellness::row_count(&self.view(), &self.data, &self.timer)
        } else if self.view() == View::Report {
            self.report_tasks().len()
        } else {
            self.visible().len()
        };
        self.selected = self.selected.min(count.saturating_sub(1));
    }
    fn finish_report(&mut self, day: chrono::NaiveDate) {
        if let Some(count) = self.report_loading.get_mut(&day) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.report_loading.remove(&day);
            }
        }
    }
    fn play(&self, sound: &'static [u8], volume: f32) {
        if let Some(audio) = &self.audio
            && let Ok(source) = rodio::Decoder::new(std::io::Cursor::new(sound))
        {
            audio.mixer().add(rodio::Source::amplify(source, volume));
        }
    }
    fn send(&mut self, job: Job) {
        if matches!(&job, Job::Complete(t) if t.is_note()) {
            self.message = "Anotação · use e para editar; anotações não são concluídas".into();
            return;
        }
        if matches!(self.view(), View::Report | View::Habits | View::Focus)
            && !matches!(job, Job::Refresh | Job::Report(_) | Job::CheckHabit { .. })
        {
            self.message = "Relatório · visualização das atividades concluídas".into();
            return;
        }
        if matches!(job, Job::Complete(_))
            || matches!(job, Job::CheckHabit { value, .. } if value > 0.0)
        {
            self.play(include_bytes!("../assets/check.wav"), 0.7);
        }
        if self.demo {
            if let Job::CheckHabit {
                ref id,
                day,
                goal,
                value,
            } = job
            {
                crate::wellness::record_checkin(&mut self.data, id, day, goal, value);
            }
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
            if !matches!(job, Job::Refresh | Job::Report(_) | Job::CheckHabit { .. }) {
                self.pending_jobs.push_back(job.clone());
                sync::apply(&mut self.data, &job);
            }
            if let Job::Report(day) = &job {
                *self.report_loading.entry(*day).or_default() += 1;
            }
            self.in_flight += 1;
            self.busy = true;
            self.message = match &job {
                Job::Report(day) => format!("Consultando relatório de {}…", day.format("%d/%m/%Y")),
                Job::Refresh => "Sincronizando com TickTick…".into(),
                Job::CheckHabit { .. } => "Registrando hábito no TickTick…".into(),
                _ => format!("Salvando… {} operação(ões)", self.pending_jobs.len()),
            };
            self.clamp();
        } else {
            self.message = if matches!(job, Job::Refresh | Job::Report(_) | Job::CheckHabit { .. })
            {
                "Consulta indisponível; verifique sua conexão com o TickTick."
            } else {
                "Sincronização indisponível; alteração não enviada."
            }
            .into();
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
            if matches!(self.view(), View::Habits | View::Focus) {
                crate::wellness::row_count(&self.view(), &self.data, &self.timer).saturating_sub(1)
            } else if self.view() == View::Report {
                self.report_tasks().len().saturating_sub(1)
            } else {
                self.visible().len().saturating_sub(1)
            }
        };
        let index = if self.projects_focus {
            &mut self.project
        } else {
            &mut self.selected
        };
        *index = index.saturating_add_signed(delta).min(max);
        if self.projects_focus {
            self.selected = 0;
            if self.view() == View::Report {
                self.send(Job::Report(self.report_day));
            }
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
            Mode::Item if !text.is_empty() && self.target.is_some() => {
                self.insert_items(&[(text, false)]);
            }
            Mode::ItemRename if !text.is_empty() => match (self.target.take(), self.item.take()) {
                (Some(_), Some(Action::Subtask(id))) => {
                    if let Some(mut sub) = self.data.tasks.iter().find(|t| t.id == id).cloned() {
                        sub.title = text;
                        self.update(sub);
                    }
                }
                (Some(mut t), Some(action)) => {
                    if preview::rename(&mut t, &action, &text) {
                        self.update(t);
                    }
                }
                _ => (),
            },
            Mode::Rename if !text.is_empty() => {
                if let Some(mut t) = self.target.take() {
                    t.title = text;
                    self.update(t);
                }
            }
            Mode::FocusTask => {
                let choices = self.focus_choices(&text);
                match choices.get(self.pick.min(choices.len().saturating_sub(1))) {
                    Some(task) => {
                        self.message = match task {
                            Some(t) => format!("Atividade do foco: {}", t.title),
                            None => "Foco livre".into(),
                        };
                        self.timer.set_task(task.clone());
                        self.persist_timer();
                    }
                    None => self.message = format!("Nenhuma tarefa encontrada: {text}"),
                }
            }
            Mode::FocusConfig if !text.is_empty() => match self.timer.set_durations(&text) {
                Ok(()) => {
                    let (focus, short, long) = self.timer.durations();
                    self.message =
                        format!("Foco {focus}min · pausa {short}min · pausa longa {long}min");
                    self.persist_timer();
                }
                Err(e) => self.message = e,
            },
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
                value if value.starts_with("report ") => {
                    match chrono::NaiveDate::parse_from_str(value[7..].trim(), "%Y-%m-%d") {
                        Ok(day) if day <= sidebar::today(&self.data) => {
                            self.report_day = day;
                            self.project = self
                                .sidebar()
                                .iter()
                                .position(|e| e.view == View::Report)
                                .unwrap_or(3);
                            self.selected = 0;
                            self.send(Job::Report(day));
                        }
                        _ => self.message = "Use :report AAAA-MM-DD com uma data até hoje.".into(),
                    }
                }

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
                "focus-retry" => {
                    if self.busy {
                        self.message =
                            "Aguarde o envio em andamento antes de tentar novamente.".into();
                    } else {
                        self.timer.retry_uploads();
                        self.persist_timer();
                    }
                }
                "complete" => {
                    if let Some(t) = self.task() {
                        self.send(if t.status == 2 {
                            Job::Reopen(t)
                        } else {
                            Job::Complete(t)
                        });
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
        if self.mode == Mode::FocusTask {
            let ctrl = key.modifiers.contains(M::CONTROL);
            let step = match key.code {
                K::Down | K::Tab => 1,
                K::Up | K::BackTab => -1,
                K::Char('n') if ctrl => 1,
                K::Char('p') if ctrl => -1,
                _ => 0,
            };
            let len = self.focus_choices(&self.input).len().max(1) as isize;
            if step != 0 {
                self.pick = (self.pick as isize + step).rem_euclid(len) as usize;
                return false;
            }
            if !matches!(key.code, K::Enter | K::Esc) {
                self.pick = 0; // The filter changed, so restart from the best match.
            }
        }
        if self.mode == Mode::Visual
            && !matches!(
                key.code,
                K::Char('v' | 'j' | 'k' | 'J' | 'K' | 'g' | 'G' | '0'..='9')
                    | K::Up
                    | K::Down
                    | K::Esc
            )
        {
            return false;
        }
        if !matches!(self.mode, Mode::Normal | Mode::Visual) {
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
        if matches!(self.view(), View::Habits | View::Focus) && !self.projects_focus {
            if self.view() == View::Habits && matches!(key.code, K::Char(' ' | 'x') | K::Enter) {
                if self.busy {
                    self.message = "Aguarde a confirmação da sincronização.".into();
                } else if let Some(habit) = crate::wellness::habits(&self.data)
                    .get(self.selected)
                    .map(|h| (*h).clone())
                    && let Some(id) = habit["id"].as_str()
                {
                    // Space toggles: a completed habit is reset to zero for today.
                    let goal = crate::wellness::habit_goal(&habit);
                    let value = if crate::wellness::habit_done(&self.data, &habit) {
                        0.0
                    } else {
                        goal
                    };
                    self.send(Job::CheckHabit {
                        id: id.into(),
                        day: sidebar::today(&self.data),
                        goal,
                        value,
                    });
                }
                return false;
            }
            if matches!(self.view(), View::Focus) {
                match key.code {
                    K::Char(' ') | K::Enter => {
                        self.timer.toggle(self.timer.is_pomodoro());
                        self.persist_timer();
                        return false;
                    }
                    K::Char('p' | 'w') => {
                        if self.timer.select_mode(key.code == K::Char('p')) {
                            self.selected = 0;
                            self.persist_timer();
                        } else {
                            self.message = "Pause o timer antes de trocar de modo.".into();
                        }
                        return false;
                    }
                    K::Char('s') => {
                        self.timer.reset(self.timer.is_pomodoro());
                        self.persist_timer();
                        return false;
                    }
                    K::Char('n') if self.timer.is_pomodoro() => {
                        self.timer.advance();
                        self.persist_timer();
                        return false;
                    }
                    K::Char('t') => {
                        self.mode = Mode::FocusTask;
                        self.pick = 0;
                        self.input.clear();
                        self.input_cursor = 0;
                        return false;
                    }
                    K::Char('c') => {
                        let (focus, short, long) = self.timer.durations();
                        self.mode = Mode::FocusConfig;
                        self.input = format!("{focus} {short} {long}");
                        self.input_cursor = self.input.chars().count();
                        return false;
                    }
                    _ => (),
                }
            }
            if matches!(
                key.code,
                K::Char(
                    'a' | 'i'
                        | 'o'
                        | 'A'
                        | 'x'
                        | 'd'
                        | 'c'
                        | 'p'
                        | 'm'
                        | 'D'
                        | 'R'
                        | 'e'
                        | 'v'
                        | 'J'
                        | 'K'
                        | 'H'
                        | 'L'
                        | 'f'
                        | 'z'
                ) | K::Enter
                    | K::Char(' ')
            ) {
                return false;
            }
        }
        if self.view() == View::Report {
            if self.preview_focus {
                match key.code {
                    K::Char('h') | K::Left | K::Esc => {
                        self.focus(1);
                        return false;
                    }
                    K::Char('j') | K::Down => {
                        self.preview_cursor = self.preview_cursor.saturating_add(1);
                        return false;
                    }
                    K::Char('k') | K::Up => {
                        self.preview_cursor = self.preview_cursor.saturating_sub(1);
                        return false;
                    }
                    _ => (),
                }
            }
            if matches!(key.code, K::Char('l') | K::Right | K::Enter) && !self.projects_focus {
                if self.task().is_some() && !self.report_loading.contains_key(&self.report_day) {
                    self.focus(2);
                    self.preview_cursor = 0;
                }
                return false;
            }
            let day = match key.code {
                K::Char('[') => self.report_day.pred_opt(),
                K::Char(']') => self
                    .report_day
                    .succ_opt()
                    .filter(|d| *d <= sidebar::today(&self.data)),
                K::Char('t') => Some(sidebar::today(&self.data)),
                _ => None,
            };
            if let Some(day) = day {
                self.report_day = day;
                self.selected = 0;
                self.preview_cursor = 0;
                self.focus(1);
                self.send(Job::Report(day));
                return false;
            }
            if !self.projects_focus
                && !self.preview_focus
                && matches!(key.code, K::Char('j' | 'k') | K::Down | K::Up)
            {
                self.selected = if matches!(key.code, K::Char('j') | K::Down) {
                    self.selected.saturating_add(1)
                } else {
                    self.selected.saturating_sub(1)
                };
                self.clamp();
                self.preview_cursor = 0;
                return false;
            }
            if matches!(key.code, K::Char('O' | 'u')) && !key.modifiers.contains(M::CONTROL) {
                return false;
            }
            if matches!(
                key.code,
                K::Char(
                    'a' | 'i'
                        | 'o'
                        | 'A'
                        | 'x'
                        | ' '
                        | 'd'
                        | 'c'
                        | 'p'
                        | 'm'
                        | 'D'
                        | 'R'
                        | 'e'
                        | 'v'
                        | 'J'
                        | 'K'
                        | 'H'
                        | 'L'
                ) | K::Enter
            ) {
                return false;
            }
        }
        if let K::Char(c @ '1'..='3') = key.code
            && self.count.is_empty()
            && self.mode != Mode::Visual
        {
            self.focus(c as u8 - b'1');
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
            K::Char('v') | K::Esc if self.mode == Mode::Visual => self.mode = Mode::Normal,
            K::Char('J') if self.mode == Mode::Visual => self.shift(n as isize),
            K::Char('K') if self.mode == Mode::Visual => self.shift(-(n as isize)),
            K::Char('J' | 'K') if self.preview_focus => {
                if let (Some(mut t), Some(Action::Checklist(i))) =
                    (self.task(), self.cursor_action())
                {
                    let j = if key.code == K::Char('J') {
                        i + 1
                    } else {
                        i.wrapping_sub(1)
                    };
                    if j < t.items.len() {
                        t.items.swap(i, j);
                        renumber(&mut t.items);
                        self.update(t);
                        self.cursor_to(Action::Checklist(j));
                    }
                }
            }
            K::Char('H' | 'L') if !self.projects_focus && !self.preview_focus => {
                self.reparent(key.code == K::Char('L'))
            }
            K::Char('J' | 'K') if !self.projects_focus && self.mode == Mode::Normal => {
                self.anchor = self.selected;
                self.shift(if key.code == K::Char('J') {
                    n as isize
                } else {
                    -(n as isize)
                });
            }
            K::Char('h') if pending == 'z' && !self.preview_focus => {
                self.show_completed = !self.show_completed;
                self.clamp();
                self.preview_cursor = 0;
                self.message = if self.show_completed {
                    "Concluídos disponíveis no histórico local visíveis · x / Space reabre"
                } else {
                    "Concluídos ocultos"
                }
                .into();
            }
            K::Char('h') if pending == 'z' => {
                self.hide_done = !self.hide_done;
                self.message = if self.hide_done {
                    "Concluídos ocultos"
                } else {
                    "Concluídos visíveis"
                }
                .into();
            }
            K::Char('s') if pending == 'g' && self.preview_focus => self.convert(),
            K::Char('x') if pending == 'g' => self.open_link(),
            K::Char('q') => return true,
            K::Char('j' | ']') | K::Down if self.preview_focus => self.jump(n as isize),
            K::Char('k' | '[') | K::Up if self.preview_focus => self.jump(-(n as isize)),
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
            K::Char('u') => self.undo(),
            K::Char('d') if pending == 'd' && self.preview_focus => match self.cursor_action() {
                Some(Action::Subtask(id)) => {
                    self.target = self.data.tasks.iter().find(|t| t.id == id).cloned();
                    if self.target.is_some() {
                        self.mode = Mode::Delete;
                    }
                }
                Some(action) => {
                    if let Some(mut t) = self.task()
                        && preview::remove(&mut t, &action)
                    {
                        self.update(t);
                        self.preview_cursor = self.preview_cursor.saturating_sub(1);
                        self.message = "Item apagado · u desfaz".into();
                    }
                }
                None => (),
            },
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
            K::Char(c @ ('a' | 'o' | 'O')) if self.preview_focus => {
                let len = self.task().map_or(0, |t| t.items.len());
                let at = match (c, self.cursor_action()) {
                    ('o', Some(Action::Checklist(i))) => i + 1,
                    ('O', Some(Action::Checklist(i))) => i,
                    ('O', _) => 0,
                    _ => len,
                };
                self.edit(Mode::Item, String::new());
                self.pick = at;
            }
            K::Char('a') | K::Char('i') | K::Char('o') => {
                self.mode = Mode::Add;
                self.input.clear();
            }
            K::Char('c') if pending == 'c' && self.preview_focus => {
                if let (Some(t), Some(action)) = (self.task(), self.cursor_action()) {
                    let title = match &action {
                        Action::Subtask(id) => self
                            .data
                            .tasks
                            .iter()
                            .find(|s| &s.id == id)
                            .map(|s| s.title.clone()),
                        _ => preview::label(&t, &action),
                    };
                    if let Some(title) = title {
                        self.edit(Mode::ItemRename, title);
                        self.item = Some(action);
                    }
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
                    self.send(if t.status == 2 {
                        Job::Reopen(t)
                    } else {
                        Job::Complete(t)
                    });
                }
            }
            K::Char('r') => self.send(if self.view() == View::Report {
                Job::Report(self.report_day)
            } else {
                Job::Refresh
            }),
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
            K::Char('<') if self.projects_focus => self.lists = (self.lists - 2).max(-6),
            K::Char('>') if self.projects_focus => self.lists = (self.lists + 2).min(40),
            K::Char('<') => self.split = self.split.saturating_sub(5).max(15),
            K::Char('>') => self.split = (self.split + 5).min(70),
            K::Enter => self.focus(2),
            K::Char('v')
                if !self.projects_focus && !self.preview_focus && self.task().is_some() =>
            {
                self.mode = Mode::Visual;
                self.anchor = self.selected;
            }
            K::Char('p') if pending == 'z' => {
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
        let _ = execute!(
            io::stdout(),
            event::DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}
pub fn run(mut config: Config, api: Option<Api>, demo: bool) -> Result<()> {
    if !demo {
        crate::wellness::Timer::save_status_theme(&config.theme)?;
    }
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        event::EnableBracketedPaste
    )?;
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
    let account = api.as_ref().map(Api::account_key);
    let (tx, rx) = if let Some(api) = api {
        let (t, r) = sync::worker(api, data.clone());
        (Some(t), Some(r))
    } else {
        (None, None)
    };
    let mut app = App {
        timer: if demo {
            Default::default()
        } else {
            crate::wellness::Timer::load()
        },
        confirmed: data.clone(),
        pending_jobs: VecDeque::new(),
        in_flight: 0,
        report_loading: Default::default(),
        report_day: sidebar::today(&data),
        data,
        project: 0,
        selected: 0,
        anchor: 0,
        projects_focus: true,
        preview_focus: false,
        preview_cursor: 0,
        preview_width: 60,
        split: 34,
        lists: 0,
        zoom: false,
        hide_lists: false,
        collapsed: HashSet::new(),
        pick: 0,
        item: None,
        hide_done: false,
        show_completed: false,
        undo: Vec::new(),
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
        quitting: false,
        demo,
        // Opened once so each sound plays without startup latency.
        audio: config
            .sounds
            .then(|| {
                rodio::DeviceSinkBuilder::from_default_device()
                    .and_then(|b| {
                        // rodio's 2048-frame default is rejected by macOS (max 1024).
                        b.with_buffer_size(rodio::cpal::BufferSize::Default)
                            .with_error_callback(|_| {})
                            .open_stream()
                    })
                    .ok()
                    .map(|mut sink| {
                        sink.log_on_drop(false);
                        sink
                    })
            })
            .flatten(),
        pending: '\0',
        count: String::new(),
        details: true,
        last_sync: Instant::now(),
        tx,
        rx,
    };
    if let Some(account) = account {
        app.timer.bind_account(account);
        app.persist_timer();
        if app.timer.uncertain_uploads() > 0 {
            app.message =
                "Há envios de foco sem confirmação · confira o histórico antes de :focus-retry"
                    .into();
        }
    }
    if !demo {
        app.send(Job::Refresh);
    }
    let mut was_finished = app.timer.finished();
    loop {
        // Plays a sound once when a pomodoro phase ends.
        let finished = app.timer.finished();
        if finished && !was_finished {
            app.play(include_bytes!("../assets/notification.wav"), 1.0);
        }
        was_finished = finished;
        if app.timer.finish_if_due() || !app.timer.ready_uploads().is_empty() {
            app.persist_timer();
        }
        loop {
            let event = app.rx.as_ref().and_then(|rx| rx.try_recv().ok());
            if let Some(event) = event {
                app.in_flight = app.in_flight.saturating_sub(1);
                app.busy = app.in_flight > 0;
                app.last_sync = Instant::now();
                match event {
                    sync::Event::FocusSaved { id, result } => {
                        match result {
                            Ok(data) => {
                                app.timer.acknowledge(&id);
                                app.replace_snapshot(data);
                                app.message = "Sessão de foco salva no TickTick".into();
                            }
                            Err(error) => {
                                app.message = format!(
                                    "Envio de foco sem confirmação: {error}. Registro local preservado; confira o histórico antes de :focus-retry."
                                )
                            }
                        }
                        if let Err(error) = app.timer.save() {
                            app.message = format!("Falha ao salvar confirmação do foco: {error}");
                        }
                    }

                    sync::Event::Report(day, d, m) => {
                        app.finish_report(day);
                        app.replace_snapshot(d);
                        app.message = m;
                    }
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
                            app.timer.remap_task(&local.id, &remote.id);
                            app.persist_timer();
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
        if app.quitting && !app.busy {
            break;
        }
        if app.mode == Mode::Token {
            app.mode = Mode::Normal;
            if app.demo {
                app.message = "Run without demo to configure your account.".into();
            } else if app.busy {
                app.message = "Wait for synchronization, then use :token again.".into();
            } else if let Some(api) = crate::token_ui::configure(&mut terminal, &config)? {
                app.timer.bind_account(api.account_key());
                app.timer.save()?;
                let (tx, rx) = sync::worker(api, Snapshot::default());
                app.tx = Some(tx);
                app.rx = Some(rx);
                app.data = Snapshot::default();
                app.confirmed = Snapshot::default();
                app.pending_jobs.clear();
                app.report_loading.clear();
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
                    if !demo {
                        crate::wellness::Timer::save_status_theme(&t)?;
                    }
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
        let event = if event::poll(Duration::from_millis(100))? {
            Some(event::read()?)
        } else {
            None
        };
        if let Some(Event::Paste(text)) = &event {
            app.paste(text);
        }
        if let Some(Event::Key(key)) = event {
            if key.kind != event::KeyEventKind::Press {
                continue;
            }
            if app.quitting {
                // Ctrl-C forces exit without waiting.
                if key.code == K::Char('c') && key.modifiers.contains(M::CONTROL) {
                    break;
                }
            } else if app.mode == Mode::Normal
                && app.preview_focus
                && app.pending != 'g'
                && (matches!(key.code, K::Char(' ') | K::Char('x') | K::Enter)
                    || key.code == K::Char('l')
                        && matches!(app.cursor_action(), Some(Action::Subtask(_))))
            {
                app.preview_action(&config.theme, matches!(key.code, K::Enter | K::Char('l')));
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
                if !app.busy {
                    break;
                }
                app.quitting = true;
            }
        }
    }
    Ok(())
}
fn new_item(title: &str, done: bool) -> crate::api::ChecklistItem {
    crate::api::ChecklistItem {
        // TickTick ids are 24 hex chars, generated by the client.
        id: uuid::Uuid::new_v4().simple().to_string()[..24].into(),
        title: title.into(),
        status: done.into(),
        ..Default::default()
    }
}
/// TickTick orders checklist items by `sortOrder`, so keep it in sync with the vector order.
fn renumber(items: &mut [crate::api::ChecklistItem]) {
    for (i, item) in items.iter_mut().enumerate() {
        item.extra.insert("sortOrder".into(), serde_json::json!(i));
    }
}
// Keep one cell available for the terminal cursor, including at end of input.
fn input_view(input: &str, cursor: usize, width: usize) -> (String, u16) {
    let chars = input.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let mut start = 0;
    while start < cursor
        && chars[start..cursor].iter().collect::<String>().width() >= width.max(1)
    {
        start += 1;
    }
    let offset = chars[start..cursor].iter().collect::<String>().width() as u16;
    (chars[start..].iter().collect(), offset)
}

fn input_line(f: &mut Frame, area: Rect, input: &str, cursor: usize) -> String {
    let (text, offset) = input_view(input, cursor, area.width as usize);
    if area.width > 0 && area.height > 0 {
        f.set_cursor_position((area.x + offset, area.y));
    }
    text
}

// Wrap explicitly so the displayed text and terminal cursor use identical rows.
fn wrapped_input(input: &str, cursor: usize, width: usize) -> (String, (u16, u16)) {
    use unicode_segmentation::UnicodeSegmentation;
    let width = width.max(1);
    let mut text = String::new();
    let (mut x, mut y, mut count) = (0, 0, 0);
    let mut position = (0, 0);
    for g in input.graphemes(true) {
        let w = g.width();
        if g != "\n" && x + w >= width && x > 0 {
            text.push('\n');
            x = 0;
            y += 1;
        }
        if count <= cursor {
            position = (x as u16, y as u16);
        }
        text.push_str(g);
        if g == "\n" {
            x = 0;
            y += 1;
        } else {
            x += w;
        }
        count += g.chars().count();
        if count <= cursor {
            position = (x as u16, y as u16);
        }
    }
    (text, position)
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
        .unwrap_or_else(|_| if cfg!(windows) { "notepad.exe" } else { "vi" }.into());
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
        user_name: Some("Alex".into()),
        report_days: vec![chrono::Utc::now().with_timezone(&chrono_tz::America::Recife).date_naive()],
        completed: ["Revisar proposta com o time", "Publicar correção", "Organizar a semana"].iter().enumerate().map(|(i, title)| Task {
            id: format!("done-{i}"), project_id: "work".into(), title: (*title).into(), status: 2,
            extra: serde_json::from_value(serde_json::json!({"completedTime": chrono::Utc::now().to_rfc3339()})).unwrap(),
            ..Default::default()
        }).collect(),
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
    if matches!(app.view(), View::Habits | View::Focus) {
        return Rect::default();
    }
    if app.view() == View::Report {
        return if app.preview_focus {
            body
        } else {
            Rect::default()
        };
    }
    if app.preview_focus && (app.zoom || body.width < 110) {
        return body;
    }
    if app.details && body.width >= 110 {
        let sidebar = if app.hide_lists {
            0
        } else if body.width >= 150 {
            25u16
        } else {
            21
        }
        .saturating_add_signed(if app.hide_lists { 0 } else { app.lists });
        return Layout::horizontal([
            Constraint::Length(sidebar),
            Constraint::Percentage(app.split),
            Constraint::Min(40),
        ])
        .split(body)[2];
    }
    Rect::default()
}
pub(crate) fn report_number(value: usize) -> Vec<Line<'static>> {
    const DIGITS: [[&str; 3]; 10] = [
        ["█▀█", "█ █", "▀▀▀"],
        [" ▄█", "  █", "  ▀"],
        ["▀▀█", "█▀▀", "▀▀▀"],
        ["▀▀█", " ▀█", "▀▀▀"],
        ["█ █", "▀▀█", "  ▀"],
        ["█▀▀", "▀▀█", "▀▀▀"],
        ["█▀▀", "█▀█", "▀▀▀"],
        ["▀▀█", "  █", "  ▀"],
        ["█▀█", "█▀█", "▀▀▀"],
        ["█▀█", "▀▀█", "▀▀▀"],
    ];
    (0..3)
        .map(|row| {
            Line::from(
                value
                    .to_string()
                    .bytes()
                    .map(|b| DIGITS[(b - b'0') as usize][row])
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        })
        .collect()
}
fn draw_report(f: &mut Frame, area: Rect, app: &App, theme: &config::Theme) {
    let success = preview::color(&theme.success);
    let muted = preview::color(&theme.muted);
    let accent = preview::color(&theme.accent);
    let fg = preview::color(&theme.foreground);
    let border = preview::color(&theme.border);
    let shell = |title: &str| {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border))
            .title(format!(" {title} "))
            .title_style(Style::default().fg(muted))
    };
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("‹  ", Style::default().fg(accent)),
            Span::styled(
                app.report_day.format("%d / %m / %Y").to_string(),
                Style::default().fg(fg).bold(),
            ),
            Span::styled("  ›", Style::default().fg(accent)),
        ]))
        .alignment(Alignment::Center),
        sections[0],
    );
    f.render_widget(
        Paragraph::new(
            if app.data.report_days.contains(&app.report_day)
                || app.report_loading.contains_key(&app.report_day)
            {
                "[ anterior   ] seguinte   t hoje   :report AAAA-MM-DD"
            } else {
                "Registros locais · histórico remoto indisponível · r tentar novamente"
            },
        )
        .style(Style::default().fg(muted))
        .alignment(Alignment::Center),
        sections[2],
    );
    let loading = app.report_loading.contains_key(&app.report_day);
    let tasks = app.report_tasks();
    let focus = app.data.focus_days.iter().find(|v| v.day == app.report_day);
    let has_focus = focus.is_some_and(|v| !v.totals.is_empty());
    if loading || (tasks.is_empty() && !has_focus) {
        let frames = ["◐", "◓", "◑", "◒"];
        let spinner = frames[(app.last_sync.elapsed().as_millis() / 120 % 4) as usize];
        let remote = app.data.report_days.contains(&app.report_day);
        let (symbol, title, subtitle) = if loading {
            (
                spinner,
                "Buscando suas conquistas…",
                "Consultando o TickTick para este dia",
            )
        } else if remote {
            (
                "☀",
                "Um dia sem conclusões registradas",
                "Seu ritmo também tem espaço para uma pausa.",
            )
        } else {
            (
                "◌",
                "Histórico indisponível",
                "Não foi possível consultar este dia no TickTick. Use r para tentar novamente.",
            )
        };
        let center = centered(sections[1], 76, 7);
        f.render_widget(
            Paragraph::new(vec![
                Line::styled(symbol, Style::default().fg(accent).bold()),
                Line::default(),
                Line::styled(title, Style::default().fg(fg).bold()),
                Line::default(),
                Line::styled(subtitle, Style::default().fg(muted)),
                Line::default(),
                Line::styled(
                    if loading {
                        ""
                    } else if focus.is_some_and(|v| v.error.is_some()) {
                        "Tempo focado indisponível"
                    } else {
                        "Foco · sem sessões registradas neste dia"
                    },
                    Style::default().fg(muted),
                ),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            center,
        );
        return;
    }
    let total = tasks.len();
    let mut groups = std::collections::BTreeMap::<String, usize>::new();
    let project_name = |t: &Task| {
        app.data
            .projects
            .iter()
            .find(|p| p.id == t.project_id)
            .map(|p| p.name.as_str())
            .unwrap_or(if app.data.inbox_id.as_ref() == Some(&t.project_id) {
                "Inbox"
            } else {
                "Lista"
            })
    };
    for task in &tasks {
        *groups.entry(project_name(task).to_owned()).or_default() += 1;
    }

    let focus_columns = if area.width >= 90 {
        3
    } else if area.width >= 50 {
        2
    } else {
        1
    };
    let focus_rows = focus.map_or(1, |v| v.totals.len().max(1).div_ceil(focus_columns));
    let dashboard = Layout::vertical([
        Constraint::Length(if area.height >= 18 { 6 } else { 3 }),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length((focus_rows + 2).min(u16::MAX as usize) as u16),
        Constraint::Length(4),
    ])
    .split(sections[1]);
    let stats = Layout::horizontal([Constraint::Ratio(1, 2); 2])
        .spacing(1)
        .split(dashboard[0]);
    for (i, (label, value, color)) in [
        ("CONCLUSÕES", total, success),
        ("LISTAS", groups.len(), accent),
    ]
    .into_iter()
    .enumerate()
    {
        let block = shell(label);
        let inner = block.inner(stats[i]);
        f.render_widget(block, stats[i]);
        if inner.height >= 3 && inner.width >= value.to_string().len() as u16 * 4 {
            f.render_widget(
                Paragraph::new(report_number(value))
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(color).bold()),
                centered(inner, inner.width, 3),
            );
        } else {
            f.render_widget(
                Paragraph::new(value.to_string())
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(color).bold()),
                inner,
            );
        }
    }
    let mut priorities = vec![Span::styled("Prioridades  ", Style::default().fg(muted))];
    for (i, (priority, label, color)) in [
        (5, "alta", preview::color(&theme.danger)),
        (3, "média", accent),
        (1, "baixa", success),
        (0, "sem prioridade", muted),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            priorities.push(Span::styled("  ·  ", Style::default().fg(muted)));
        }
        priorities.push(Span::styled(
            format!(
                "{} {label}",
                tasks.iter().filter(|t| t.priority == priority).count()
            ),
            Style::default().fg(color),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(priorities)).alignment(Alignment::Center),
        dashboard[1],
    );
    let columns = Layout::horizontal([
        Constraint::Percentage(if area.width >= 72 { 66 } else { 100 }),
        Constraint::Min(0),
    ])
    .spacing(if area.width >= 72 { 1 } else { 0 })
    .split(dashboard[2]);
    let selected = app.selected.min(total.saturating_sub(1));
    let list_block = shell(&format!("✓ Conquistas  {}/{}", selected + 1, total));
    let inner = list_block.inner(columns[0]);
    let project_width = if inner.width >= 50 { 16 } else { 0 };
    let title_width = inner.width.saturating_sub(project_width + 5) as usize;
    let hierarchy = tree::rows(&tasks, &HashSet::new(), "");
    let mut previous_context = String::new();
    let rows = hierarchy.iter().enumerate().map(|(i, row)| {
        let task = row.task;
        let context = app.data.ancestry(task);
        let show_context = row.depth == 0 && !context.is_empty() && context != previous_context;
        let mut title = Vec::new();
        if show_context {
            title.push(Line::styled(
                truncate(&context, title_width),
                Style::default().fg(muted),
            ));
        }
        let depth = row.depth + usize::from(row.depth == 0 && !context.is_empty());
        let indent = "  ".repeat(depth.min(4));
        let prefix = format!("{indent}✓ ");
        title.push(Line::from(vec![
            Span::styled(prefix.clone(), Style::default().fg(success)),
            Span::raw(truncate(
                &task.title.replace(['\n', '\r'], " "),
                title_width.saturating_sub(prefix.width()),
            )),
        ]));
        previous_context = context;
        let height = title.len() as u16;
        Row::new(vec![
            Cell::from(Text::from(title)),
            Cell::from(truncate(project_name(task), project_width as usize))
                .style(Style::default().fg(muted)),
        ])
        .height(height)
        .style(Style::default().fg(if i == selected { accent } else { fg }))
    });
    let table = Table::new(
        rows,
        [Constraint::Min(0), Constraint::Length(project_width)],
    )
    .column_spacing(1)
    .block(list_block)
    .row_highlight_style(Style::default().bg(preview::color(&theme.selection)).bold());
    f.render_stateful_widget(
        table,
        columns[0],
        &mut TableState::default().with_selected((!tasks.is_empty()).then_some(selected)),
    );
    if columns[1].width > 0 {
        let block = shell("Por lista");
        let inner = block.inner(columns[1]);
        f.render_widget(block, columns[1]);
        let mut groups = groups.into_iter().collect::<Vec<_>>();
        groups.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (i, (name, count)) in groups.iter().take(inner.height as usize / 3).enumerate() {
            let y = inner.y + i as u16 * 3;
            f.render_widget(
                Paragraph::new(truncate(name, inner.width as usize)).style(Style::default().fg(fg)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            f.render_widget(
                Gauge::default()
                    .ratio(*count as f64 / total as f64)
                    .gauge_style(
                        Style::default()
                            .fg(if i % 2 == 0 { success } else { accent })
                            .bg(preview::color(&theme.code_background)),
                    )
                    .label(Span::styled(
                        count.to_string(),
                        Style::default()
                            .fg(preview::color(&theme.panel))
                            .bg(if i % 2 == 0 { success } else { accent })
                            .bold(),
                    )),
                Rect::new(inner.x, y + 1, inner.width, 1),
            );
        }
    }
    let focus_total = focus.map_or(0, |v| v.totals.iter().map(|t| t.seconds).sum());
    let focus_title = if has_focus {
        format!(
            "Tempo focado por lista · {}",
            crate::focus::duration(focus_total)
        )
    } else {
        "Tempo focado por lista".into()
    };
    let focus_block = shell(&focus_title);
    let focus_inner = focus_block.inner(dashboard[3]);
    f.render_widget(focus_block, dashboard[3]);
    if let Some(day) = focus.filter(|d| d.error.is_none() && !d.totals.is_empty()) {
        let cols = Layout::horizontal(vec![
            Constraint::Ratio(1, focus_columns as u32);
            focus_columns
        ])
        .spacing(2)
        .split(focus_inner);
        for (col, rect) in cols.iter().enumerate() {
            let rows = day
                .totals
                .iter()
                .skip(col * focus_rows)
                .take(focus_rows)
                .map(|t| {
                    Row::new(vec![
                        Cell::from(truncate(&t.label, rect.width.saturating_sub(12) as usize))
                            .style(Style::default().fg(fg)),
                        Cell::from(crate::focus::duration(t.seconds))
                            .style(Style::default().fg(success).bold()),
                    ])
                });
            f.render_widget(
                Table::new(rows, [Constraint::Min(0), Constraint::Length(11)]),
                *rect,
            );
        }
    } else {
        f.render_widget(
            Paragraph::new(if focus.is_some_and(|d| d.error.is_some()) {
                "Não foi possível consultar o foco · r tentar novamente"
            } else {
                "Nenhuma sessão de foco registrada neste dia"
            })
            .alignment(Alignment::Center)
            .style(Style::default().fg(muted)),
            focus_inner,
        );
    }
    let detail = shell("Selecionada · l / Enter ver atividade");
    f.render_widget(
        Paragraph::new(tasks.get(selected).map_or(
            "Nenhuma tarefa concluída · o tempo focado também é progresso",
            |t| t.title.as_str(),
        ))
        .wrap(Wrap { trim: true })
        .style(Style::default().fg(fg))
        .block(detail),
        dashboard[4],
    );
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
                if matches!(app.view(), View::Habits | View::Focus) {
                    "  /  Hábitos e foco".into()
                } else if app.view() == View::Report {
                    "  /  Seu progresso".into()
                } else {
                    format!("  /  {} pendentes", app.visible().len())
                },
                Style::default().fg(muted),
            ),
            Span::styled(
                app.timer.header().unwrap_or_default(),
                Style::default().fg(accent),
            ),
        ])),
        header[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            app.data
                .user_name
                .as_deref()
                .or(c.user_name.as_deref())
                .unwrap_or(if app.demo {
                    "Pessoa de exemplo"
                } else {
                    "Minha conta"
                }),
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
    let fullscreen =
        app.preview_focus && (app.view() == View::Report || app.zoom || rows[1].width < 110);
    if !fullscreen {
        let cols = if preview.width > 0 {
            Layout::horizontal([
                Constraint::Length(
                    if app.hide_lists {
                        0
                    } else if rows[1].width >= 150 {
                        25u16
                    } else {
                        21
                    }
                    .saturating_add_signed(if app.hide_lists {
                        0
                    } else {
                        app.lists
                    }),
                ),
                Constraint::Percentage(app.split),
                Constraint::Min(40),
            ])
            .split(rows[1])
        } else {
            Layout::horizontal([
                Constraint::Length(
                    if app.hide_lists {
                        0
                    } else if rows[1].width < 55 {
                        17u16
                    } else {
                        25
                    }
                    .saturating_add_signed(if app.hide_lists {
                        0
                    } else {
                        app.lists
                    }),
                ),
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
                    View::Tomorrow => "▦ ",
                    View::Report => "★ ",
                    View::Habits => "✓ ",
                    View::Focus => "◴ ",
                    View::Group(id) => {
                        if app.collapsed.contains(&format!("group:{id}")) {
                            "▸ "
                        } else {
                            "▾ "
                        }
                    }
                    View::Project(_) => "  ",
                };
                let selected = i == app.project;
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled(
                            if selected { "▎" } else { " " },
                            Style::default().fg(accent),
                        ),
                        Span::styled(
                            format!(" {}{marker}", "  ".repeat(e.depth)),
                            Style::default().fg(if i < 6 || e.view == View::All {
                                accent
                            } else {
                                muted
                            }),
                        ),
                        Span::raw(e.name.clone()),
                        Span::styled(
                            if matches!(e.view, View::Report | View::Habits | View::Focus) {
                                String::new()
                            } else {
                                format!("  {}", e.count)
                            },
                            Style::default().fg(muted),
                        ),
                    ])
                    .style(if selected {
                        Style::default().bg(selection).fg(accent)
                    } else {
                        Style::default()
                    }),
                ];
                // The "LISTAS" header lives in item 3, so highlight only the first line by hand.
                if i == 5 {
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
            List::new(projects).block(
                block(" 1 · Listas ".into(), app.projects_focus).padding(Padding::vertical(1)),
            ),
            cols[0],
            &mut state,
        );
        let tree = app.tree();
        let (tz, today) = (app.tz(), sidebar::today(&app.data));
        let danger = preview::color(&t.danger);
        // Borders, padding and the highlight symbol take 5 columns.
        let row_width = cols[1].width.saturating_sub(5) as usize;
        let visual = (app.mode == Mode::Visual).then(|| app.range());
        let mut previous_context = String::new();
        let items = tree
            .iter()
            .enumerate()
            .map(|(i, row)| {
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
                if task
                    .extra
                    .get("reminders")
                    .and_then(|v| v.as_array())
                    .is_some_and(|r| !r.is_empty())
                {
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
                let context = app.data.ancestry(task);
                let show_context =
                    row.depth == 0 && !context.is_empty() && context != previous_context;
                let left = if row.depth == 0 && !context.is_empty() {
                    format!("   {} ", task.marker())
                } else {
                    format!(" {indent}{branch}{} ", task.marker())
                };
                let right: usize = badges.iter().map(Span::width).sum();
                let title = truncate(
                    &task.title,
                    row_width.saturating_sub(left.width() + right + 1),
                );
                let pad = row_width.saturating_sub(left.width() + title.width() + right);
                let mut line = vec![
                    Span::styled(left, Style::default().fg(muted)),
                    Span::raw(title),
                    Span::raw(" ".repeat(pad)),
                ];
                line.extend(badges);
                let mut lines = Vec::new();
                if show_context {
                    lines.push(Line::styled(
                        truncate(&format!(" {indent}{context}"), row_width),
                        Style::default().fg(muted),
                    ));
                }
                previous_context = context;
                lines.push(Line::from(line));
                let item = ListItem::new(lines);
                match visual {
                    Some((lo, hi)) if (lo..=hi).contains(&i) => {
                        item.style(Style::default().bg(selection))
                    }
                    _ => item,
                }
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(if tree.is_empty() {
            None
        } else {
            Some(app.selected)
        });
        let title = if matches!(app.view(), View::Habits | View::Focus) {
            format!(" {} ", project)
        } else if app.view() == View::Report {
            " ★ · Relatório do dia ".into()
        } else if app.query.is_empty() {
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
        if matches!(app.view(), View::Habits | View::Focus) {
            crate::wellness::draw(
                f,
                inner,
                &app.view(),
                &app.data,
                &app.timer,
                app.selected,
                t,
            );
        } else if app.view() == View::Report {
            draw_report(f, inner, app, t);
        } else if tree.is_empty() {
            f.render_widget(
                Paragraph::new(if app.busy {
                    "Buscando suas tarefas…"
                } else {
                    "Tudo em dia.\n\nSelecione uma lista e pressione a\npara adicionar uma tarefa."
                })
                .style(Style::default().fg(muted))
                .wrap(Wrap { trim: false })
                .alignment(Alignment::Center),
                centered(inner, 64, 5),
            );
        }
    }
    if preview.width > 0 {
        let document = app.preview(t);
        let preview_block = block(
            if app.view() == View::Report {
                " Atividade concluída · h / Esc voltar ".into()
            } else {
                format!(
                    " 3 · Preview{} ",
                    if app.preview_focus {
                        " · j/k navegar"
                    } else {
                        ""
                    }
                )
            },
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
        Paragraph::new(if app.busy {
            " ◌ Sincronizando com TickTick…".to_owned()
        } else {
            format!(" {}", app.message)
        })
        .style(Style::default().fg(muted)),
        rows[2],
    );
    let prompt = match app.mode {
        Mode::Search | Mode::Command => {
            let area = Rect::new(rows[3].x + 2, rows[3].y, rows[3].width.saturating_sub(2), rows[3].height);
            let text = input_line(f, area, &app.input, app.input_cursor);
            format!(" {}{text}", if app.mode == Mode::Search { "/" } else { ":" })
        }
        Mode::Add | Mode::Subtask | Mode::Rename | Mode::ItemRename => {
            " INSERIR  Enter salvar · Esc cancelar".into()
        }
        Mode::Item => " CHECKLIST  Enter adiciona · colar várias linhas · Esc cancela".into(),
        Mode::Move => {
            let names = app
                .lists(&app.input)
                .into_iter()
                .take(4)
                .map(|(_, name)| name)
                .collect::<Vec<_>>();
            let area = Rect::new(rows[3].x + 14, rows[3].y, rows[3].width.saturating_sub(14), rows[3].height);
            let text = input_line(f, area, &app.input, app.input_cursor);
            format!(
                " MOVER  Lista: {}   → {}",
                text,
                if names.is_empty() {
                    "nenhuma lista".into()
                } else {
                    names.join(" · ")
                }
            )
        }
        Mode::Visual => " VISUAL  j/k estender · J/K mover · v/Esc sair".into(),
        Mode::FocusTask => format!(
            " ATIVIDADE  Buscar: {}   ↑/↓ escolher · Enter iniciar com ela · Esc cancelar",
            app.input.clone()
        ),
        Mode::FocusConfig => {
            " POMODORO  foco pausa pausa-longa (min) · Enter salvar · Esc cancelar".into()
        }
        Mode::Due => " PRAZO  j/k escolher · Enter salvar · Tab completar · Esc cancelar".into(),
        Mode::Remind => {
            " LEMBRETES  j/k escolher · Space marcar · Enter salvar · Esc cancelar".into()
        }
        Mode::Delete => {
            " EXCLUIR  Confirmar exclusão permanente? y / qualquer tecla cancela".into()
        }
        _ => {
            if matches!(app.view(), View::Focus) {
                " FOCO  Space iniciar/pausar · t atividade · c tempos · n próxima fase · s encerrar/salvar · p/w modo · h listas".into()
            } else if app.view() == View::Habits {
                " HÁBITOS  j/k rolar · Space concluir/desmarcar · h listas · r sincronizar · ? ajuda".into()
            } else if app.preview_focus {
                " PREVIEW  x marcar · o/O novo item · J/K mover · cc renomear · dd apagar · u desfazer · gs subtarefa · zh ocultar feitos · h voltar".into()
            } else {
                " NORMAL  h/j/k/l navegar · a nova · J/K mover · H/L nível · m lista · D prazo · R lembrete · za recolher · ? ajuda".into()
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
                if due {
                    " Outra data: "
                } else {
                    " Selecionados: "
                },
                Style::default().fg(muted),
            ),
            Span::raw(app.input.clone()),
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
            t.due_date
                .as_deref()
                .map(|d| fields::format_due(d, all_day, app.tz()))
        });
        let title = match (due, current) {
            (true, Some(c)) => format!(" Prazo · atual {c} "),
            (true, None) => " Prazo ".into(),
            (false, _) => " Lembretes ".into(),
        };
        let area = centered(f.area(), 44, lines.len() as u16 + 4);
        let input_area = Rect::new(area.x + 2 + if due { 13 } else { 15 }, area.y + area.height.saturating_sub(4), area.width.saturating_sub(4 + if due { 13 } else { 15 }), 1);
        let text = input_line(f, input_area, &app.input, app.input_cursor);
        if let Some(line) = lines.iter_mut().rev().nth(1) {
            if let Some(span) = line.spans.last_mut() { span.content = text.into(); }
        }
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(panel).fg(fg))
                .block(block(title, true).padding(Padding::new(1, 1, 1, 0))),
            area,
        );
    }
    if app.mode == Mode::FocusTask {
        let choices = app.focus_choices(&app.input);
        let pick = app.pick.min(choices.len().saturating_sub(1));
        let shown = 10;
        let first = pick.saturating_sub(shown - 1);
        let width = f.area().width.saturating_sub(4).min(64);
        let mut lines = choices
            .iter()
            .enumerate()
            .skip(first)
            .take(shown)
            .map(|(i, choice)| {
                let style = if i == pick {
                    Style::default().bg(selection).fg(accent)
                } else {
                    Style::default()
                };
                let (title, list) = match choice {
                    Some(t) => (
                        t.title.replace(['\n', '\r'], " "),
                        app.data
                            .tasks
                            .iter()
                            .find(|x| x.id == t.id)
                            .map(|x| app.project_name(&x.project_id))
                            .unwrap_or_default(),
                    ),
                    None => ("○ Foco livre (sem tarefa)".into(), String::new()),
                };
                Line::from(vec![
                    Span::styled(format!(" {title} "), style),
                    Span::styled(list, Style::default().fg(muted)),
                ])
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(Line::styled(
                " Nenhuma tarefa encontrada",
                Style::default().fg(muted),
            ));
        }
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(" Buscar: ", Style::default().fg(muted)),
            Span::raw(app.input.clone()),
        ]));
        let area = centered(f.area(), width, lines.len() as u16 + 3);
        let input_area = Rect::new(area.x + 2 + 8, area.y + area.height.saturating_sub(2), area.width.saturating_sub(4 + 8), 1);
        let text = input_line(f, input_area, &app.input, app.input_cursor);
        if let Some(line) = lines.iter_mut().rev().nth(0) {
            if let Some(span) = line.spans.last_mut() { span.content = text.into(); }
        }
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(panel).fg(fg))
                .block(block(" Atividade do foco ".into(), true).padding(Padding::new(1, 1, 1, 0))),
            area,
        );
    }
    let title = match app.mode {
        Mode::Add => Some(" Nova tarefa "),
        Mode::Subtask => Some(" Subtarefa "),
        Mode::Item => Some(" Novo item "),
        Mode::ItemRename => Some(" Item "),
        Mode::Rename => Some(" Título "),
        Mode::FocusConfig => Some(" Foco · pausa · pausa longa (minutos) "),
        _ => None,
    };
    if let Some(title) = title {
        let width = f.area().width.saturating_sub(4).min(72);
        let (text, (x, y)) = wrapped_input(&app.input, app.input_cursor, width.saturating_sub(4) as usize);
        let lines = text.split('\n').count() as u16;
        let area = centered(f.area(), width, lines.saturating_add(3));
        let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
        let scroll = y.saturating_sub(inner.height.saturating_sub(1));
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(text)
                .style(Style::default().bg(panel).fg(fg))
                .scroll((scroll, 0))
                .block(block(title.into(), true).padding(Padding::horizontal(1))),
            area,
        );
        if inner.width > 0 && inner.height > 0 {
            f.set_cursor_position((inner.x + x.min(inner.width - 1), inner.y + y - scroll));
        }
    }

    if app.mode == Mode::Help {
        let area = centered(f.area(), 84, 36);
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new("\nNAVEGAÇÃO\nh / l, Tab     Listas → tarefas → preview\n1 / 2 / 3      Focar listas / tarefas / preview\nj / k, 5j      Navegar com contagem       gg / G    Início / fim\nCtrl-d / u     Descer / subir 10 linhas   Enter     Focar preview\nzp             Mostrar / ocultar preview  Esc / h   Voltar às tarefas\nf              Zoom no preview            < / >     Redimensionar colunas\nb              Ocultar / mostrar listas\n\nTAREFAS\na / i / o      Nova tarefa na lista       A         Nova subtarefa\nza / zo / zc   Alternar / abrir / recolher árvore\ncc             Renomear                   e         Editar no Vim/$EDITOR\nv + j/k        Selecionar (v/Esc sai)     J / K     Mover tarefa/seleção ↓ / ↑\nH / L          Subir nível / virar subtarefa da de cima\nm              Mover para outra lista     D / R     Prazo / lembretes\nx / Space      Concluir / reabrir tarefa            dd        Excluir (confirmar y)\nzh             Mostrar / ocultar tarefas concluídas\np              Alternar prioridade        /         Buscar com ancestrais\n\nPREVIEW\nj / k, ] / [   Próximo / anterior checkbox ou subtarefa (sem mais: rola)\nCtrl-d / u     Rolar o texto 10 linhas\nSpace / x      Marcar checkbox, checklist ou concluir subtarefa\nEnter          Marcar checkbox / abrir subtarefa (l também abre)\na / o / O      Novo item: fim / abaixo / acima (colar linhas cria vários)\nJ / K          Mover item ↓ / ↑           u         Desfazer última edição\ncc / dd        Renomear / apagar item, checkbox ou subtarefa\ngs             Item ↔ subtarefa           gx        Abrir link\nzh             Ocultar / mostrar itens concluídos\n\nRELATÓRIO\n[ / ]          Dia anterior / seguinte    t         Hoje\n:report AAAA-MM-DD  Escolher uma data\n\nHÁBITOS E FOCO\nSpace / x      Concluir / desmarcar hábito\np / w          Pomodoro / Stopwatch (na tela Foco)\nSpace / Enter  Iniciar / pausar timer local\nt              Escolher atividade         c         Tempos de foco / pausas\nn              Próxima fase do Pomodoro   s         Encerrar e salvar sessão\n:focus-retry   Reenviar foco pendente após conferir histórico\n\nCOMANDOS\n:token         Configurar API token       :subtask  Nova subtarefa\n:theme dark    Tema escuro                :theme light  Tema claro\n:sync / :w     Sincronizar                :config   Caminho da configuração\n:noh           Limpar busca               :q / q    Sair\n\nQualquer tecla fecha a ajuda.").style(Style::default().bg(panel).fg(fg)).wrap(Wrap{trim:false}).block(block(" Atalhos ".into(),true).padding(Padding::horizontal(2))),area);
    }
    if app.quitting {
        let area = centered(f.area(), 44, 5);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(
                "Sincronizando… sai ao terminar.
Ctrl-C força a saída.",
            )
            .style(Style::default().bg(panel).fg(fg))
            .block(block(" Sincronizando ".into(), true).padding(Padding::horizontal(1))),
            area,
        );
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
    #[test]
    fn native_cursor_tracks_scrolling_and_unicode_wrapping() {
        assert_eq!(super::input_view("abcdef", 6, 4), ("def".into(), 3));
        assert_eq!(super::input_view("a界b", 2, 4), ("a界b".into(), 3));
        assert_eq!(super::wrapped_input("abcd", 4, 4), ("abc\nd".into(), (1, 1)));
        assert_eq!(super::wrapped_input("a\n界b", 3, 5), ("a\n界b".into(), (2, 1)));
    }

    use super::*;
    #[test]
    fn report_groups_siblings_with_parent_first() {
        let mut a = app();
        let stamp = serde_json::json!(chrono::Utc::now().to_rfc3339());
        let make = |id: &str, parent: Option<&str>| Task {
            id: id.into(),
            project_id: "p".into(),
            title: id.into(),
            parent_id: parent.map(str::to_owned),
            status: 2,
            extra: serde_json::from_value(serde_json::json!({"completedTime": stamp})).unwrap(),
            ..Default::default()
        };
        a.data.completed = vec![
            make("child-a", Some("parent")),
            make("other", None),
            make("child-b", Some("parent")),
            make("parent", None),
        ];
        a.report_day = sidebar::today(&a.data);
        let ids: Vec<_> = a.report_tasks().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["parent", "child-a", "child-b", "other"]);
    }

    #[test]
    fn list_shows_completed_and_reopens_selected_task() {
        let mut a = app();
        a.demo = true;
        a.projects_focus = false;
        a.preview_focus = false;
        let task = a.data.tasks[0].clone();
        a.send(Job::Complete(task.clone()));
        assert!(!a.visible().iter().any(|t| t.id == task.id));
        a.key(KeyEvent::new(K::Char('z'), M::NONE));
        a.key(KeyEvent::new(K::Char('h'), M::NONE));
        a.selected = a.visible().iter().position(|t| t.id == task.id).unwrap();
        assert_eq!(a.task().unwrap().marker(), "✓");
        a.key(KeyEvent::new(K::Char('x'), M::NONE));
        assert!(
            a.data
                .tasks
                .iter()
                .any(|t| t.id == task.id && t.status == 0)
        );
        assert!(!a.data.completed.iter().any(|t| t.id == task.id));
    }

    #[test]
    fn notes_cannot_be_completed_or_queued() {
        let mut a = app();
        let (tx, rx) = std::sync::mpsc::channel();
        a.tx = Some(tx);
        a.demo = false;
        a.data.tasks[0]
            .extra
            .insert("kind".into(), serde_json::json!("NOTE"));
        let note = a.data.tasks[0].clone();
        assert_eq!(note.marker(), "≡");
        a.send(Job::Complete(note));
        assert_eq!(a.data.tasks.len(), 20);
        assert!(a.pending_jobs.is_empty());
        assert!(rx.try_recv().is_err());
    }

    fn app() -> App {
        let mut a = App {
            timer: Default::default(),
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
            report_loading: Default::default(),
            report_day: chrono::Local::now().date_naive(),
            project: 6,
            selected: 0,
            anchor: 0,
            projects_focus: false,
            preview_focus: false,
            preview_cursor: 0,
            preview_width: 60,
            split: 34,
            lists: 0,
            zoom: false,
            hide_lists: false,
            collapsed: HashSet::new(),
            pick: 0,
            item: None,
            hide_done: false,
            show_completed: false,
            undo: Vec::new(),
            theme_request: None,
            mode: Mode::Normal,
            input: String::new(),
            input_cursor: 0,
            search_original: String::new(),
            target: None,
            query: String::new(),
            message: String::new(),
            busy: false,
            quitting: false,
            demo: true,
            audio: None,
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
    fn b_hides_lists_after_widening_them() {
        let area = Rect::new(0, 0, 160, 40);
        let mut hidden = app();
        hidden.hide_lists = true;
        let mut a = app();
        a.projects_focus = true;
        press(&mut a, '>');
        press(&mut a, 'b');
        assert_eq!(preview_area(area, &a), preview_area(area, &hidden));
    }
    #[test]
    fn wellness_views_render_and_block_task_mutations() {
        let mut a = app();
        let (tx, rx) = std::sync::mpsc::channel();
        a.tx = Some(tx);
        for view in [View::Habits, View::Focus] {
            a.project = a.sidebar().iter().position(|e| e.view == view).unwrap();
            a.send(Job::Delete(a.data.tasks[0].clone()));
            assert!(rx.try_recv().is_err());
            for size in [(30, 10), (100, 30)] {
                let backend = ratatui::backend::TestBackend::new(size.0, size.1);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
            }
        }
    }
    #[test]
    fn space_toggles_habit_completion() {
        let mut a = app();
        a.data.habits = vec![
            serde_json::json!({"id":"h","name":"Ler","goal":1.0,"repeatRule":"RRULE:FREQ=DAILY"}),
        ];
        a.project = a
            .sidebar()
            .iter()
            .position(|e| e.view == View::Habits)
            .unwrap();
        a.projects_focus = false;
        let habit = a.data.habits[0].clone();
        press(&mut a, ' ');
        assert!(crate::wellness::habit_done(&a.data, &habit));
        press(&mut a, ' ');
        assert!(!crate::wellness::habit_done(&a.data, &habit));
    }
    #[test]
    fn focus_picks_activity_and_durations() {
        let mut a = app();
        a.project = a
            .sidebar()
            .iter()
            .position(|e| e.view == View::Focus)
            .unwrap();
        a.projects_focus = false;
        let title = a.data.tasks[3].title.clone();
        press(&mut a, 't');
        assert!(a.mode == Mode::FocusTask);
        for c in title.chars() {
            press(&mut a, c);
        }
        let backend = ratatui::backend::TestBackend::new(100, 30);
        Terminal::new(backend)
            .unwrap()
            .draw(|f| draw(f, &a, &Config::default()))
            .unwrap();
        a.key(KeyEvent::from(K::Enter));
        assert_eq!(a.timer.task().map(|t| t.title.clone()), Some(title));
        press(&mut a, 'c');
        assert!(a.mode == Mode::FocusConfig);
        a.input = "45 10 30".into();
        a.key(KeyEvent::from(K::Enter));
        assert_eq!(a.timer.durations(), (45, 10, 30));
        press(&mut a, 't');
        a.key(KeyEvent::from(K::Enter)); // Empty query: first choice is free focus.
        assert!(a.timer.task().is_none());
    }
    #[test]
    fn opening_subtask_hidden_in_view_jumps_to_its_list() {
        let mut a = app();
        a.data.tasks[0].due_date = Some("2000-01-01T00:00:00.000+0000".into());
        a.data.tasks[1].parent_id = Some("0".into());
        a.project = a
            .sidebar()
            .iter()
            .position(|e| e.view == View::Today)
            .unwrap();
        assert_eq!(a.visible().len(), 1);
        a.preview_cursor = a
            .preview(&config::Theme::default())
            .iter()
            .position(|r| r.action == Some(Action::Subtask("1".into())))
            .unwrap();
        a.preview_action(&config::Theme::default(), true);
        assert_eq!(a.view(), View::Project("p".into()));
        assert_eq!(a.task().map(|t| t.id), Some("1".into()));
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
        assert_eq!(a.view(), View::All);
        assert_eq!(a.selected, 0);
        press(&mut a, 'l');
        press(&mut a, 'k');
        assert_eq!(a.selected, 0);
    }
    #[test]
    fn shift_l_nests_under_sibling_above_and_shift_h_unnests() {
        let mut a = app();
        for (i, t) in a.data.tasks.iter_mut().enumerate() {
            t.sort_order = i as i64;
        }
        a.confirmed = a.data.clone();
        let parent = |a: &App, id: &str| {
            let t = a.data.tasks.iter().find(|t| t.id == id).unwrap();
            t.parent_id.clone().filter(|p| !p.is_empty())
        };
        a.selected = 2;
        press(&mut a, 'L');
        assert_eq!(parent(&a, "2"), Some("1".into()));
        a.selected = 3;
        press(&mut a, 'L');
        press(&mut a, 'L');
        assert_eq!(parent(&a, "3"), Some("2".into()));
        press(&mut a, 'H');
        assert_eq!(parent(&a, "3"), Some("1".into()));
        press(&mut a, 'H');
        assert_eq!(parent(&a, "3"), None);
        assert_eq!(a.task().unwrap().id, "3");
    }
    #[test]
    fn visual_mode_moves_sibling_block_with_subtasks() {
        let mut a = app();
        for (i, t) in a.data.tasks.iter_mut().enumerate() {
            t.sort_order = i as i64;
        }
        a.data.tasks[5].parent_id = Some("4".into());
        a.confirmed = a.data.clone();
        let ids = |a: &App| {
            a.visible()
                .iter()
                .take(8)
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        };
        a.selected = 3;
        press(&mut a, 'v');
        press(&mut a, 'j');
        press(&mut a, 'K');
        assert_eq!(ids(&a), ["0", "1", "3", "4", "5", "2", "6", "7"]);
        assert_eq!((a.anchor, a.selected), (2, 3));
        press(&mut a, '2');
        press(&mut a, 'J');
        assert_eq!(ids(&a), ["0", "1", "2", "6", "3", "4", "5", "7"]);
        press(&mut a, 'v');
        assert!(a.mode == Mode::Normal);
        press(&mut a, 'J');
        assert_eq!(ids(&a)[4], "3");
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
        for c in "aum\radois\r".chars() {
            let code = if c == '\r' { K::Enter } else { K::Char(c) };
            a.key(KeyEvent::new(code, M::NONE));
        }
        assert!(a.mode == Mode::Normal);
        let titles = |a: &App| {
            a.task()
                .unwrap()
                .items
                .iter()
                .map(|i| i.title.clone())
                .collect::<Vec<_>>()
        };
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
    fn preview_vim_editing_of_checkboxes_and_checklist() {
        let mut a = app();
        let typing = |a: &mut App, text: &str| {
            for c in text.chars() {
                let code = match c {
                    '\r' => K::Enter,
                    '\x1b' => K::Esc,
                    c => K::Char(c),
                };
                a.key(KeyEvent::new(code, M::NONE));
            }
        };
        a.data.tasks[0].content = "- [x] alfa\n- [ ] beta\nfim".into();
        a.focus(2);
        // Entering the preview lands on the first open checkbox.
        assert!(matches!(
            a.cursor_action(),
            Some(Action::Markdown { checked: false, .. })
        ));
        typing(&mut a, "cc");
        a.key(KeyEvent::new(K::Char('u'), M::CONTROL));
        typing(&mut a, "gama\r");
        assert_eq!(a.task().unwrap().content, "- [x] alfa\n- [ ] gama\nfim");
        typing(&mut a, "dd");
        assert_eq!(a.task().unwrap().content, "- [x] alfa\nfim");
        typing(&mut a, "u");
        assert_eq!(a.task().unwrap().content, "- [x] alfa\n- [ ] gama\nfim");

        let titles = |a: &App| {
            a.task()
                .unwrap()
                .items
                .iter()
                .map(|i| i.title.clone())
                .collect::<Vec<_>>()
        };
        typing(&mut a, "a");
        a.paste("- [ ] um\n- [x] três\n");
        typing(&mut a, "\x1b");
        assert_eq!(titles(&a), ["um", "três"]);
        assert_eq!(a.task().unwrap().items[1].status, 1);
        typing(&mut a, "Odois\r");
        assert_eq!(titles(&a), ["um", "dois", "três"]);
        typing(&mut a, "K");
        assert_eq!(titles(&a), ["dois", "um", "três"]);
        assert_eq!(a.task().unwrap().items[0].extra["sortOrder"], 0);
        typing(&mut a, "zh");
        assert!(
            !a.preview(&config::Theme::default())
                .iter()
                .any(|r| r.action == Some(Action::Checklist(2)))
        );
        a.cursor_to(Action::Checklist(0));
        typing(&mut a, "gs");
        assert_eq!(titles(&a), ["um", "três"]);
        assert_eq!(a.task().unwrap().id, "0");
        assert!(
            a.data
                .tasks
                .iter()
                .any(|t| t.parent_id.as_deref() == Some("0") && t.title == "dois")
        );
    }
    #[test]
    fn j_k_jump_between_checkboxes_and_shift_k_moves_task_without_visual() {
        let mut a = app();
        for (i, t) in a.data.tasks.iter_mut().enumerate() {
            t.sort_order = i as i64;
        }
        a.data.tasks[0].content = "texto\n\n- [ ] um\n\nmais texto\n\n- [ ] dois\n\nfim".into();
        a.focus(2);
        let label = |a: &App| {
            a.cursor_action()
                .and_then(|x| preview::label(&a.task().unwrap(), &x))
        };
        assert_eq!(label(&a).as_deref(), Some("um"));
        press(&mut a, 'j');
        assert_eq!(label(&a).as_deref(), Some("dois"));
        // No checkbox below: j scrolls the remaining text, k returns to the checkbox.
        press(&mut a, 'j');
        assert_eq!(label(&a), None);
        press(&mut a, 'k');
        assert_eq!(label(&a).as_deref(), Some("dois"));
        a.focus(1);
        press(&mut a, 'J');
        assert_eq!(a.task().unwrap().id, "0");
        assert_eq!(a.selected, 1);
        assert!(a.mode == Mode::Normal);
    }
    #[test]
    fn report_queries_do_not_show_save_status_or_queue_mutations() {
        let mut a = app();
        let (tx, rx) = std::sync::mpsc::channel();
        a.tx = Some(tx);
        a.demo = false;
        let before = a.data.tasks.len();
        a.send(Job::Report(a.report_day));
        assert!(matches!(rx.try_recv().unwrap(), Job::Report(_)));
        assert!(a.message.starts_with("Consultando relatório de "));
        assert!(!a.message.contains("Salvando"));
        assert!(a.pending_jobs.is_empty());
        assert_eq!(a.data.tasks.len(), before);
        assert!(a.busy);
        a.busy = false;
        a.send(Job::Refresh);
        assert!(matches!(rx.try_recv().unwrap(), Job::Refresh));
        assert_eq!(a.message, "Sincronizando com TickTick…");
        let task = a.data.tasks[0].clone();
        a.send(Job::Complete(task));
        assert!(a.message.starts_with("Salvando… 1 operação"));
        assert_eq!(a.pending_jobs.len(), 1);
    }
    #[test]
    fn report_filters_dates_and_renders_small_terminals() {
        let mut a = app();
        a.data = demo_data();
        a.confirmed = a.data.clone();
        a.project = 3;
        a.report_day = sidebar::today(&a.data);
        a.projects_focus = false;
        for (w, h) in [(120, 32), (40, 12), (10, 4)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
            if w == 120 {
                let buffer = terminal.backend().buffer();
                let text = buffer
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(text.contains("CONCLUSÕES"));
                assert!(text.contains("Revisar proposta com o time"));
                assert!(text.contains("Alex"));
                if let Ok(path) = std::env::var("TICKTICK_REPORT_PREVIEW") {
                    let cells = buffer.content.iter().map(|c| serde_json::json!({"text":c.symbol(), "fg":format!("{:?}",c.fg), "bg":format!("{:?}",c.bg)})).collect::<Vec<_>>();
                    std::fs::write(
                        path,
                        serde_json::to_vec(
                            &serde_json::json!({"width":w,"height":h,"cells":cells}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
        }
        let today = a.report_day;
        press(&mut a, '[');
        assert_eq!(a.report_day, today.pred_opt().unwrap());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Histórico indisponível"));
        assert!(!text.contains("Revisar proposta com o time"));
        press(&mut a, 't');
        assert_eq!(a.report_day, today);
        press(&mut a, ']');
        assert_eq!(a.report_day, today);
        let before = a.data.tasks.len();
        press(&mut a, 'x');
        assert_eq!(a.data.tasks.len(), before);
    }
    #[test]
    fn today_reorders_tasks_across_lists_and_keeps_the_selected_task() {
        let mut a = app();
        a.project = 0;
        a.data.tasks.truncate(3);
        let due = sidebar::today(&a.data)
            .format("%Y-%m-%dT12:00:00Z")
            .to_string();
        for (i, t) in a.data.tasks.iter_mut().enumerate() {
            t.due_date = Some(due.clone());
            t.sort_order = i as i64;
            if i == 1 {
                t.project_id = "other".into();
            }
        }
        a.confirmed = a.data.clone();
        press(&mut a, 'J');
        assert_eq!(
            a.visible()
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "0", "2"]
        );
        assert_eq!(a.selected, 1);
        assert_eq!(a.task().unwrap().id, "0");
        press(&mut a, 'K');
        assert_eq!(
            a.visible()
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            ["0", "1", "2"]
        );
        press(&mut a, 'm');
        assert!(a.mode == Mode::Move);
        assert_eq!(a.target.as_ref().unwrap().id, "0");
    }
    #[test]
    fn report_preview_opens_completed_activity_without_mutating_it() {
        let mut a = app();
        a.data = demo_data();
        a.data.completed[0].content = "Descrição da entrega\n\n- [x] Validar resultado".into();
        a.data.completed[0].desc = Some("Observações finais".into());
        a.data.completed[0].items = vec![crate::api::ChecklistItem {
            id: "step".into(),
            title: "Checklist concluído".into(),
            status: 1,
            ..Default::default()
        }];
        a.confirmed = a.data.clone();
        a.project = 3;
        a.report_day = sidebar::today(&a.data);
        a.projects_focus = false;
        let original = serde_json::to_value(&a.data).unwrap();
        press(&mut a, 'l');
        assert!(a.preview_focus);
        for (w, h) in [(140, 40), (80, 24), (40, 12)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
            if w == 140 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                for label in [
                    "Descrição da entrega",
                    "Validar resultado",
                    "Observações finais",
                    "Checklist concluído",
                ] {
                    assert!(text.contains(label), "Missing {label}");
                }
            }
        }
        press(&mut a, 'j');
        assert_eq!(a.selected, 0);
        assert_eq!(a.preview_cursor, 1);
        press(&mut a, 'x');
        assert_eq!(serde_json::to_value(&a.data).unwrap(), original);
        press(&mut a, 'h');
        assert!(!a.preview_focus);
        press(&mut a, 'j');
        assert_eq!(a.selected, 1);
        a.key(KeyEvent::new(K::Enter, M::NONE));
        assert_eq!(a.task().unwrap().title, "Publicar correção");
        a.key(KeyEvent::new(K::Esc, M::NONE));
        assert!(!a.preview_focus);
    }
    #[test]
    fn report_priorities_are_separate_and_chart_counts_contrast_over_bars() {
        let mut a = app();
        a.data = demo_data();
        for (task, priority) in a.data.completed.iter_mut().zip([5, 3, 1]) {
            task.priority = priority;
        }
        a.project = 3;
        a.report_day = sidebar::today(&a.data);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        for label in ["1 alta", "1 média", "1 baixa", "0 sem prioridade"] {
            assert!(text.contains(label));
        }
        assert!(!text.contains("PRIORIDADE ALTA"));
        let row = buffer
            .content
            .chunks(140)
            .find(|row| {
                row.iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .contains("████")
            })
            .unwrap();
        let last_bar = row.iter().rposition(|c| c.symbol() == "█").unwrap();
        let count = row.iter().rposition(|c| c.symbol() == "3").unwrap();
        assert!(count < last_bar);
        assert_eq!(
            row[count].fg,
            preview::color(&config::Theme::default().panel)
        );
        assert_ne!(row[count].fg, row[count].bg);
    }
    #[test]
    fn report_shows_focus_per_list_even_without_completed_tasks() {
        let mut a = app();
        a.data = demo_data();
        a.data.completed.clear();
        a.project = 3;
        a.report_day = sidebar::today(&a.data);
        a.data.focus_days.push(crate::focus::FocusDay {
            day: a.report_day,
            totals: vec![
                crate::focus::FocusTotal {
                    project_id: Some("work".into()),
                    label: "Expin".into(),
                    seconds: 5400,
                },
                crate::focus::FocusTotal {
                    project_id: None,
                    label: "Foco livre".into(),
                    seconds: 1800,
                },
            ],
            error: None,
        });
        for (w, h) in [(140, 40), (80, 24), (40, 12)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
            if w == 140 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                for label in [
                    "Tempo focado por lista",
                    "Expin",
                    "1h 30min",
                    "Foco livre",
                    "30min",
                ] {
                    assert!(text.contains(label), "Missing {label}");
                }
                assert!(!text.contains("Um dia sem conclusões registradas"));
            }
        }
        a.report_day = a.report_day.pred_opt().unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!text.contains("1h 30min"));
    }
    #[test]
    fn report_loading_is_scoped_to_date_and_empty_state_is_centered() {
        let mut a = app();
        a.data = demo_data();
        a.project = 3;
        a.report_day = sidebar::today(&a.data);
        let today = a.report_day;
        let yesterday = today.pred_opt().unwrap();
        a.report_loading.insert(today, 1);
        a.report_loading.insert(yesterday, 1);
        a.finish_report(yesterday);
        assert!(a.report_loading.contains_key(&today));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Buscando suas conquistas"));
        assert!(!text.contains("Revisar proposta com o time"));
        a.finish_report(today);
        a.report_day = yesterday;
        a.data.report_days.push(yesterday);
        terminal.draw(|f| draw(f, &a, &Config::default())).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Um dia sem conclusões registradas"));
        let y = (0..40)
            .find(|y| {
                buffer.content[y * 140..(y + 1) * 140]
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .contains("Um dia sem conclusões registradas")
            })
            .unwrap();
        let x = buffer.content[y * 140..(y + 1) * 140]
            .iter()
            .position(|c| c.symbol() == "U")
            .unwrap();
        assert!(y > 10); // Centered vertically, below the header.
        assert!(x > 35); // Centered horizontally, beyond the sidebar edge.
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
