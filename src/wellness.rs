mod upload;
pub use upload::FocusUpload;
use crate::{api::Snapshot, config, preview, sidebar::View};
use ratatui::{prelude::*, widgets::*};
use serde::{Deserialize, Serialize};

/// Pomodoro phases; a long break replaces every `LONG_EVERY`th short break.
#[derive(Clone, Copy, Default, PartialEq, Debug, Serialize, Deserialize)]
pub enum Phase {
    #[default]
    Focus,
    Short,
    Long,
}
const LONG_EVERY: u32 = 4;
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct FocusTask {
    pub id: String,
    pub title: String,
}
/// Wall-clock timestamps keep the timer running when the TUI is closed.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Timer {
    pomodoro: bool,
    started: Option<i64>,
    elapsed: u64,
    phase: Phase,
    /// Focus sessions finished so far; drives the long break.
    cycles: u32,
    /// Phase lengths in minutes.
    focus: u64,
    short: u64,
    long: u64,
    task: Option<FocusTask>,
    session: Option<upload::Session>,
    uploads: Vec<FocusUpload>,
    account: Option<String>,
}
impl Default for Timer {
    fn default() -> Self {
        Self { pomodoro: true, started: None, elapsed: 0, phase: Phase::Focus, cycles: 0, focus: 25, short: 5, long: 15, task: None, session: None, uploads: Vec::new(), account: None }
    }
}
impl Timer {
    pub fn is_pomodoro(&self) -> bool { self.pomodoro }
    pub fn task(&self) -> Option<&FocusTask> { self.task.as_ref() }
    pub fn set_task(&mut self, task: Option<FocusTask>) {
        if self.task == task { return; }
        if self.pomodoro && self.phase != Phase::Focus {
            self.task = task;
            return;
        }
        let running = self.started.is_some() && !self.finished();
        self.finish_session();
        self.started = None;
        self.elapsed = 0;
        self.task = task;
        if running { self.toggle(self.pomodoro); }
    }
    pub fn durations(&self) -> (u64, u64, u64) { (self.focus, self.short, self.long) }
    /// Accepts "foco pausa pausa-longa" in minutes, e.g. "50 10 20" or "50/10/20".
    pub fn set_durations(&mut self, text: &str) -> Result<(), String> {
        self.finish_if_due();
        if self.session.is_some() || self.started.is_some() || self.elapsed > 0 {
            return Err("Encerre a sessão com s antes de alterar os tempos.".into());
        }
        let values = text.split(|c: char| c.is_whitespace() || c == '/' || c == ',').filter(|s| !s.is_empty())
            .map(|s| s.parse::<u64>().ok().filter(|v| (1..=480).contains(v)))
            .collect::<Option<Vec<_>>>();
        match values.as_deref() {
            Some(&[focus, short, long]) => {
                (self.focus, self.short, self.long) = (focus, short, long);
                Ok(())
            }
            _ => Err("Use: foco pausa pausa-longa em minutos (1–480), ex.: 50 10 20".into()),
        }
    }
    pub fn phase(&self) -> Phase { self.phase }
    /// Position of the current focus inside the cycle that ends in a long break (1-based).
    pub fn cycle(&self) -> u32 {
        let done = self.cycles % LONG_EVERY;
        if self.phase == Phase::Focus { done + 1 } else if done == 0 { LONG_EVERY } else { done }
    }
    /// Pomodoro phase length in seconds; the stopwatch has none.
    pub fn target(&self) -> Option<u64> {
        self.pomodoro.then(|| 60 * match self.phase { Phase::Focus => self.focus, Phase::Short => self.short, Phase::Long => self.long })
    }
    pub fn finished(&self) -> bool { self.target().is_some_and(|t| self.seconds() >= t) }
    /// Moves to the next pomodoro phase, stopped. Only a completed focus counts towards the long break.
    pub fn advance(&mut self) {
        if !self.pomodoro { return; }
        self.finish_session();
        self.phase = match self.phase {
            Phase::Focus => {
                if self.finished() { self.cycles += 1; }
                if self.cycles > 0 && self.cycles.is_multiple_of(LONG_EVERY) { Phase::Long } else { Phase::Short }
            }
            Phase::Short | Phase::Long => Phase::Focus,
        };
        self.started = None;
        self.elapsed = 0;
    }
    pub fn select_mode(&mut self, pomodoro: bool) -> bool {
        if self.pomodoro == pomodoro { return true; }
        if self.started.is_some() && !self.finished() { return false; }
        self.finish_session();
        self.started = None;
        self.elapsed = 0;
        self.phase = Phase::Focus;
        self.pomodoro = pomodoro;
        true
    }

    fn path() -> std::path::PathBuf {
        config::cache_path().with_file_name("focus-timer.json")
    }
    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) -> anyhow::Result<()> {
        config::private_write(&Self::path(), &serde_json::to_vec(self)?)
    }
    pub fn seconds(&self) -> u64 {
        self.elapsed.saturating_add(
            self.started
                .map(|s| chrono::Utc::now().timestamp().saturating_sub(s).max(0) as u64)
                .unwrap_or(0),
        )
    }
    pub fn toggle(&mut self, pomodoro: bool) {
        if self.started.is_some() && self.pomodoro != pomodoro {
            return; // One running timer at a time.
        }
        if self.pomodoro != pomodoro {
            self.finish_session();
            self.elapsed = 0;
            self.phase = Phase::Focus;
            self.pomodoro = pomodoro;
        } else if self.finished() {
            // A finished phase starts the next one: focus → break → focus.
            self.advance();
            self.begin_session();
            self.started = Some(chrono::Utc::now().timestamp());
            return;
        }
        if self.started.is_some() {
            self.elapsed = self.seconds();
            self.started = None;
        } else {
            self.begin_session();
            self.started = Some(chrono::Utc::now().timestamp());
        }
    }
    /// Running timer for the app header: remaining time in pomodoro, elapsed in stopwatch.
    pub fn header(&self) -> Option<String> {
        self.started?;
        let label = if self.pomodoro && self.phase != Phase::Focus { "Pausa" } else { "Foco" };
        let seconds = self.seconds();
        Some(format!("  ·  ● {label} {}", clock(self.target().map_or(seconds, |t| t.saturating_sub(seconds)))))
    }
    pub fn reset(&mut self, pomodoro: bool) {
        if self.pomodoro != pomodoro {
            return;
        }
        self.finish_session();
        self.started = None;
        self.elapsed = 0;
    }
}
fn clock(seconds: u64) -> String {
    if seconds < 3600 {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!("{:02}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    }
}
use serde_json::{Value, json};
pub fn habit_goal(habit: &Value) -> f64 {
    habit["goal"].as_f64().filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0)
}
/// Whether the habit is scheduled for `day`, following TickTick's `repeatRule`
/// (e.g. `RRULE:FREQ=WEEKLY;BYDAY=MO` or `RRULE:FREQ=DAILY;INTERVAL=2`). Archived
/// habits and those starting later are hidden; unknown rules stay visible.
pub fn habit_due(habit: &Value, day: chrono::NaiveDate) -> bool {
    use chrono::Datelike;
    if habit["status"].as_i64().is_some_and(|s| s != 0) {
        return false;
    }
    let start = habit["targetStartDate"].as_i64().and_then(|v| chrono::NaiveDate::parse_from_str(&v.to_string(), "%Y%m%d").ok());
    if start.is_some_and(|s| s > day) {
        return false;
    }
    let rule = habit["repeatRule"].as_str().unwrap_or("");
    let field = |key: &str| rule.trim_start_matches("RRULE:").split(';').find_map(|p| p.strip_prefix(key)?.strip_prefix('='));
    let interval = field("INTERVAL").and_then(|v| v.parse::<i64>().ok()).filter(|v| *v > 1);
    if let Some(days) = field("BYDAY") {
        let code = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"][day.weekday().num_days_from_monday() as usize];
        // Values may carry an ordinal prefix such as "1MO"; only the weekday matters here.
        return days.split(',').any(|d| d.trim_start_matches(|c: char| c == '-' || c == '+' || c.is_ascii_digit()) == code);
    }
    match (field("FREQ"), interval, start) {
        (Some("DAILY"), Some(n), Some(start)) => (day - start).num_days().rem_euclid(n) == 0,
        _ => true,
    }
}
/// Habits scheduled for today, in TickTick order.
pub fn habits(data: &Snapshot) -> Vec<&Value> {
    let today = crate::sidebar::today(data);
    data.habits.iter().filter(|h| habit_due(h, today)).collect()
}
fn stamp(day: chrono::NaiveDate) -> i64 {
    day.format("%Y%m%d").to_string().parse().unwrap_or(0)
}
fn checkin<'a>(data: &'a Snapshot, habit: &Value) -> Option<&'a Value> {
    let today = stamp(crate::sidebar::today(data));
    data.habit_checkins.iter().filter(|r| r["habitId"] == habit["id"])
        .filter_map(|r| r["checkins"].as_array()).flatten()
        .find(|r| r["stamp"].as_i64() == Some(today))
}
pub fn habit_done(data: &Snapshot, habit: &Value) -> bool {
    checkin(data, habit).is_some_and(|r| r["value"].as_f64().unwrap_or(0.0) >= r["goal"].as_f64().filter(|v| *v > 0.0).unwrap_or_else(|| habit_goal(habit)))
}
pub fn record_checkin(data: &mut Snapshot, id: &str, day: chrono::NaiveDate, goal: f64, value: f64) {
    let stamp = stamp(day);
    // Preserve other dates and habits; the completion value is absolute, making retries idempotent.
    let index = data.habit_checkins.iter().position(|r| r["habitId"].as_str() == Some(id));
    let i = index.unwrap_or_else(|| {
        data.habit_checkins.push(json!({"habitId": id, "checkins": []}));
        data.habit_checkins.len() - 1
    });
    if !data.habit_checkins[i]["checkins"].is_array() { data.habit_checkins[i]["checkins"] = json!([]); }
    let rows = data.habit_checkins[i]["checkins"].as_array_mut().unwrap();
    rows.retain(|r| r["stamp"].as_i64() != Some(stamp));
    rows.push(json!({"stamp": stamp, "value": value, "goal": goal}));
}
fn records<'a>(data: &'a Snapshot, timer: &Timer) -> Vec<&'a Value> {
    let kind = if timer.pomodoro { 0 } else { 1 };
    let mut rows: Vec<_> = data.focus_records.iter().filter(|r| r["type"].as_i64() == Some(kind)).collect();
    rows.sort_by(|a, b| b["startTime"].as_str().cmp(&a["startTime"].as_str()));
    rows
}
pub fn row_count(view: &View, data: &Snapshot, timer: &Timer) -> usize {
    if *view == View::Habits { habits(data).len() } else { records(data, timer).len() }
}
fn shell(title: &str, theme: &config::Theme) -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded)
        .border_style(Style::default().fg(preview::color(&theme.border)))
        .title(format!(" {title} "))
        .title_style(Style::default().fg(preview::color(&theme.muted)))
}
fn center(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 2, width, height)
}
fn stat(f: &mut Frame, area: Rect, title: &str, value: usize, color: Color, theme: &config::Theme) {
    let block = shell(title, theme);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let big = inner.height >= 3 && inner.width >= value.to_string().len() as u16 * 4;
    f.render_widget(Paragraph::new(if big { crate::ui::report_number(value) } else { vec![Line::from(value.to_string())] })
        .alignment(Alignment::Center).style(Style::default().fg(color).bold()), center(inner, inner.width, if big { 3 } else { 1 }));
}
fn empty(f: &mut Frame, area: Rect, symbol: &str, title: &str, subtitle: &str, theme: &config::Theme) {
    f.render_widget(Paragraph::new(vec![
        Line::styled(symbol.to_string(), Style::default().fg(preview::color(&theme.accent)).bold()),
        Line::default(),
        Line::styled(title.to_string(), Style::default().fg(preview::color(&theme.foreground)).bold()),
        Line::default(),
        Line::styled(subtitle.to_string(), Style::default().fg(preview::color(&theme.muted))),
    ]).alignment(Alignment::Center).wrap(Wrap { trim: true }), center(area, 68, 5));
}
fn goal_label(habit: &Value) -> String {
    let goal = habit_goal(habit);
    let unit = habit["unit"].as_str().filter(|u| !u.is_empty()).unwrap_or(if goal == 1.0 { "vez" } else { "vezes" });
    format!("{goal} {unit}")
}
fn heading(f: &mut Frame, area: Rect, data: &Snapshot, title: &str, theme: &config::Theme) {
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(title.to_owned(), Style::default().fg(preview::color(&theme.foreground)).bold()),
        Span::styled(format!("   ·   {}", crate::sidebar::today(data).format("%d / %m / %Y")), Style::default().fg(preview::color(&theme.muted))),
    ])).alignment(Alignment::Center), area);
}
pub fn draw(f: &mut Frame, area: Rect, view: &View, data: &Snapshot, timer: &Timer, selected: usize, theme: &config::Theme) {
    if *view == View::Habits { draw_habits(f, area, data, selected, theme); }
    else { draw_focus(f, area, data, timer, selected, theme); }
}
fn draw_habits(f: &mut Frame, area: Rect, data: &Snapshot, selected: usize, theme: &config::Theme) {
    let accent = preview::color(&theme.accent);
    let success = preview::color(&theme.success);
    let muted = preview::color(&theme.muted);
    let fg = preview::color(&theme.foreground);
    let sections = Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)]).split(area);
    heading(f, sections[0], data, "Seu ritmo, todos os dias", theme);
    f.render_widget(Paragraph::new("j/k selecionar   Space / x concluir ou desmarcar   r atualizar").alignment(Alignment::Center).style(Style::default().fg(muted)), sections[2]);
    let list = habits(data);
    if data.habit_error.is_some() || list.is_empty() {
        let (title, subtitle) = if data.habit_error.is_some() { ("Hábitos indisponíveis", "Não foi possível consultar seus hábitos. Use r para tentar novamente.") }
            else if data.habits.is_empty() { ("Uma pequena ação por dia", "Seus hábitos do TickTick aparecem aqui.") }
            else { ("Dia livre", "Nenhum hábito programado para hoje.") };
        empty(f, sections[1], "◌", title, subtitle, theme);
        return;
    }
    let done = list.iter().filter(|h| habit_done(data, h)).count();
    let total = list.len();
    let dashboard = Layout::vertical([
        Constraint::Length(if area.height >= 19 { 6 } else { 3 }),
        Constraint::Length(2), Constraint::Min(0),
        Constraint::Length(if area.height >= 19 { 4 } else { 0 }),
    ]).spacing(1).split(sections[1]);
    let stats = Layout::horizontal([Constraint::Ratio(1, 2); 2]).spacing(1).split(dashboard[0]);
    stat(f, stats[0], "CONCLUÍDOS HOJE", done, success, theme);
    stat(f, stats[1], "HÁBITOS HOJE", total, accent, theme);
    let unknown = data.habit_checkin_error.is_some();
    f.render_widget(Gauge::default().ratio(done as f64 / total as f64)
        .gauge_style(Style::default().fg(success).bg(preview::color(&theme.code_background)))
        .label(Span::styled(if unknown { "Histórico parcial · r tentar novamente".into() } else { format!("{done} de {total} concluídos hoje") }, Style::default().fg(fg).bold())), dashboard[1]);
    let columns = Layout::horizontal([Constraint::Percentage(if area.width >= 82 { 67 } else { 100 }), Constraint::Min(0)]).spacing(if area.width >= 82 { 1 } else { 0 }).split(dashboard[2]);
    let selected = selected.min(total - 1);
    let table_rows = list.iter().map(|habit| {
        let done = habit_done(data, habit);
        Row::new(vec![
            Cell::from(if done { "✓" } else if unknown && checkin(data, habit).is_none() { "?" } else { "○" }).style(Style::default().fg(if done { success } else { muted })),
            Cell::from(habit["name"].as_str().unwrap_or("Hábito").replace(['\n', '\r'], " ")),
            Cell::from(goal_label(habit)).style(Style::default().fg(muted)),
            Cell::from(if done { "Feito" } else if unknown { "—" } else { "Pendente" }).style(Style::default().fg(if done { success } else { muted })),
        ])
    });
    let compact = columns[0].width < 48;
    f.render_stateful_widget(Table::new(table_rows, [Constraint::Length(2), Constraint::Min(1), Constraint::Length(if compact { 0 } else { 13 }), Constraint::Length(if compact { 0 } else { 8 })])
        .column_spacing(1).header(Row::new(["", "HÁBITO", if compact { "" } else { "META" }, if compact { "" } else { "HOJE" }]).style(Style::default().fg(muted)).bottom_margin(1))
        .block(shell("Seus hábitos", theme))
        .row_highlight_style(Style::default().bg(preview::color(&theme.selection)).fg(accent).bold())
        .highlight_symbol("▎"), columns[0], &mut TableState::default().with_selected(Some(selected)));
    let habit = list[selected];
    if columns[1].width > 0 {
        let block = shell("Selecionado", theme).padding(Padding::horizontal(1));
        let inner = block.inner(columns[1]);
        f.render_widget(block, columns[1]);
        let done = habit_done(data, habit);
        f.render_widget(Paragraph::new(vec![
            Line::default(),
            Line::styled(habit["name"].as_str().unwrap_or("Hábito").to_string(), Style::default().fg(fg).bold()),
            Line::default(),
            Line::styled("META", Style::default().fg(muted)),
            Line::styled(goal_label(habit), Style::default().fg(accent).bold()),
            Line::default(),
            Line::styled(if done { "✓ Concluído hoje · Space desmarca" } else { "Space / x · concluir" }, Style::default().fg(if done { success } else { accent })),
        ]).wrap(Wrap { trim: true }), inner);
    }
    let block = shell("Selecionado · Space / x concluir ou desmarcar", theme);
    f.render_widget(Paragraph::new(habit["name"].as_str().unwrap_or("Hábito")).style(Style::default().fg(fg)).wrap(Wrap { trim: true }).block(block), dashboard[3]);
}
fn record_seconds(record: &Value) -> u64 {
    record["duration"].as_u64().unwrap_or(0) / 1000
}
fn record_time(record: &Value, data: &Snapshot) -> String {
    let start = record["startTime"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).or_else(|_| chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z")).ok());
    start.map(|t| {
        data.time_zone.as_deref().and_then(|s| s.parse::<chrono_tz::Tz>().ok())
            .map(|tz| t.with_timezone(&tz).format("%H:%M").to_string())
            .unwrap_or_else(|| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
    }).unwrap_or_else(|| "—".into())
}
fn record_label(record: &Value, data: &Snapshot) -> String {
    let task_id = record["taskId"].as_str().or_else(|| record["tasks"].as_array()?.first()?["taskId"].as_str());
    task_id.and_then(|id| data.tasks.iter().chain(&data.completed).find(|t| t.id == id)).map(|t| t.title.clone())
        .or_else(|| record["note"].as_str().filter(|s| !s.is_empty()).map(str::to_owned)).unwrap_or_else(|| "Foco livre".into())
}
fn big_clock(value: &str) -> Vec<Line<'static>> {
    let digits: Vec<_> = value.chars().map(|ch| {
        if ch == ':' { vec![" ".into(), "•".into(), "•".into()] }
        else { crate::ui::report_number(ch.to_digit(10).unwrap_or(0) as usize).iter().map(|line| line.to_string()).collect::<Vec<String>>() }
    }).collect();
    (0..3).map(|row| Line::from(digits.iter().map(|digit| digit[row].as_str()).collect::<Vec<_>>().join(" "))).collect()
}
fn draw_focus(f: &mut Frame, area: Rect, data: &Snapshot, timer: &Timer, selected: usize, theme: &config::Theme) {
    let accent = preview::color(&theme.accent);
    let muted = preview::color(&theme.muted);
    let success = preview::color(&theme.success);
    let fg = preview::color(&theme.foreground);
    let parts = Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)]).split(area);
    let tab = |label: &str, active: bool| Span::styled(label.to_owned(), if active { Style::default().fg(accent).bg(preview::color(&theme.selection)).bold() } else { Style::default().fg(muted) });
    f.render_widget(Paragraph::new(Line::from(vec![
        tab("  p  Pomodoro  ", timer.pomodoro), Span::raw("  "), tab("  w  Stopwatch  ", !timer.pomodoro),
    ])).alignment(Alignment::Center), parts[0]);
    f.render_widget(Paragraph::new("Space iniciar / pausar   t atividade   c tempos   n próxima fase   s encerrar/salvar   p / w modo").alignment(Alignment::Center).style(Style::default().fg(muted)), parts[2]);
    let dashboard = Layout::vertical([
        Constraint::Length(if area.height >= 24 { 12 } else if area.height >= 15 { 9 } else { 5 }),
        Constraint::Length(if area.height >= 24 { 6 } else { 0 }), Constraint::Min(0),
    ]).spacing(1).split(parts[1]);
    let phase = timer.phase();
    let title = match (timer.pomodoro, timer.target()) {
        (true, Some(target)) => format!("{} · {} MIN · CICLO {}/{LONG_EVERY}", match phase { Phase::Focus => "FOCO", Phase::Short => "PAUSA CURTA", Phase::Long => "PAUSA LONGA" }, target / 60, timer.cycle()),
        _ => "STOPWATCH · TEMPO LIVRE".into(),
    };
    let hero = shell(&title, theme);
    let inner = hero.inner(dashboard[0]);
    f.render_widget(hero, dashboard[0]);
    let seconds = timer.seconds();
    let finished = timer.finished();
    let resting = timer.pomodoro && phase != Phase::Focus;
    let color = if finished || resting { success } else { accent };
    let time = clock(timer.target().map_or(seconds, |t| t.saturating_sub(seconds)));
    let large = inner.height >= 7 && inner.width >= time.len() as u16 * 4;
    let clock_height = if large { 3 } else { 1 };
    let content = center(inner, inner.width.saturating_sub(4), if large { 8 } else { 5 });
    let rows = Layout::vertical([Constraint::Length(clock_height), Constraint::Length(if large { 1 } else { 0 }), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).split(content);
    f.render_widget(Paragraph::new(if large { big_clock(&time) } else { vec![Line::from(time)] }).alignment(Alignment::Center).style(Style::default().fg(color).bold()), rows[0]);
    let status = if finished {
        if phase == Phase::Focus { "✓ Foco concluído · Space inicia a pausa" } else { "✓ Pausa concluída · Space volta ao foco" }
    } else if timer.started.is_some() { if resting { "● Descansando" } else { "● Em andamento" } }
    else if seconds > 0 { "◐ Pausado" } else { "○ Pronto para começar" };
    f.render_widget(Paragraph::new(status).alignment(Alignment::Center).style(Style::default().fg(if finished { success } else { fg })), rows[2]);
    let activity = match timer.task() {
        Some(task) => Line::from(vec![Span::styled("◎ ", Style::default().fg(accent)), Span::styled(task.title.replace(['\n', '\r'], " "), Style::default().fg(fg).bold())]),
        None => Line::styled("Foco livre · t escolher atividade", Style::default().fg(muted)),
    };
    f.render_widget(Paragraph::new(activity).alignment(Alignment::Center), rows[3]);
    if let Some(target) = timer.target() {
        f.render_widget(Gauge::default().ratio((seconds as f64 / target as f64).min(1.0)).gauge_style(Style::default().fg(color).bg(preview::color(&theme.code_background)))
            .label(Span::styled(format!("{} / {}", crate::focus::duration(seconds.min(target)), crate::focus::duration(target)), Style::default().fg(fg))), rows[5]);
    }
    if inner.height >= 10 {
        let (focus, short, long) = timer.durations();
        f.render_widget(Paragraph::new(format!("Foco {focus}min · pausa {short}min · pausa longa {long}min · {}", timer.sync_status())).alignment(Alignment::Center).style(Style::default().fg(muted)), Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    }
    let records = records(data, timer);
    let stats = Layout::horizontal([Constraint::Ratio(1, 2); 2]).spacing(1).split(dashboard[1]);
    stat(f, stats[0], "SESSÕES HOJE", records.len(), success, theme);
    let total_seconds: u64 = records.iter().map(|r| record_seconds(r)).sum();
    let duration_block = shell("TEMPO FOCADO HOJE", theme);
    let duration_inner = duration_block.inner(stats[1]);
    f.render_widget(duration_block, stats[1]);
    f.render_widget(Paragraph::new(crate::focus::duration(total_seconds)).alignment(Alignment::Center).style(Style::default().fg(accent).bold()), center(duration_inner, duration_inner.width, 1));
    let block = shell("Sessões de hoje · TickTick", theme);
    let history_inner = block.inner(dashboard[2]);
    if data.wellness_error.is_some() || records.is_empty() {
        f.render_widget(block, dashboard[2]);
        f.render_widget(Paragraph::new(if data.wellness_error.is_some() { "Histórico indisponível · r tentar novamente" } else { "Nenhuma sessão registrada hoje" }).alignment(Alignment::Center).style(Style::default().fg(muted)), center(history_inner, history_inner.width, 1));
    } else {
        let rows = records.iter().map(|r| Row::new(vec![
            Cell::from(record_time(r, data)).style(Style::default().fg(muted)), Cell::from(record_label(r, data)),
            Cell::from(crate::focus::duration(record_seconds(r))).style(Style::default().fg(success)),
        ]));
        f.render_stateful_widget(Table::new(rows, [Constraint::Length(6), Constraint::Min(1), Constraint::Length(11)])
            .column_spacing(1).header(Row::new(["INÍCIO", "FOCO", "DURAÇÃO"]).style(Style::default().fg(muted)).bottom_margin(1))
            .block(block).highlight_symbol("▎").row_highlight_style(Style::default().bg(preview::color(&theme.selection)).fg(accent).bold()), dashboard[2], &mut TableState::default().with_selected(Some(selected.min(records.len() - 1))));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timer_pauses_resumes_and_survives_serialization() {
        let mut timer = Timer {
            started: Some(chrono::Utc::now().timestamp() - 60),
            elapsed: 30,
            ..Default::default()
        };
        assert!((90..=91).contains(&timer.seconds()));
        timer.toggle(false);
        assert!(timer.pomodoro); // Cannot replace a running pomodoro.
        timer.reset(false);
        assert!(timer.started.is_some());
        timer.toggle(true);
        assert!(timer.started.is_none());
        assert!((90..=91).contains(&timer.seconds()));
        let restored: Timer = serde_json::from_slice(&serde_json::to_vec(&timer).unwrap()).unwrap();
        assert_eq!(restored.seconds(), timer.seconds());
        timer.reset(true);
        assert_eq!(timer.seconds(), 0);
        timer.toggle(true);
        assert!(timer.started.is_some());
    }
    #[test]
    fn pomodoro_cycles_through_custom_breaks() {
        let mut timer = Timer::default();
        assert!(timer.set_durations("50/10/20").is_ok());
        assert!(timer.set_durations("50 10").is_err());
        assert!(timer.set_durations("0 10 20").is_err());
        assert_eq!(timer.target(), Some(3000));
        for round in 1..=4 {
            assert_eq!((timer.phase(), timer.cycle()), (Phase::Focus, round));
            timer.elapsed = 3000;
            assert!(timer.finished());
            timer.toggle(true); // Starts the break right away.
            assert!(timer.started.is_some());
            let expected = if round == 4 { Phase::Long } else { Phase::Short };
            assert_eq!(timer.phase(), expected);
            assert_eq!(timer.target(), Some(if round == 4 { 1200 } else { 600 }));
            timer.advance();
        }
        assert_eq!((timer.phase(), timer.cycle()), (Phase::Focus, 1));
        // Skipping an unfinished focus does not count towards the long break.
        timer.advance();
        assert_eq!(timer.cycles, 4);
        let restored: Timer = serde_json::from_slice(br#"{"pomodoro":false,"started":null,"elapsed":0}"#).unwrap();
        assert_eq!(restored.durations(), (25, 5, 15));
    }
    #[test]
    fn habits_follow_their_weekdays_and_intervals() {
        let wednesday = chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let monday = chrono::NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let weekly = json!({"status":0,"repeatRule":"RRULE:FREQ=WEEKLY;BYDAY=MO","targetStartDate":20260910});
        assert!(habit_due(&weekly, monday));
        assert!(!habit_due(&weekly, wednesday));
        let weekdays = json!({"repeatRule":"RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"});
        assert!(habit_due(&weekdays, wednesday));
        assert!(!habit_due(&weekdays, chrono::NaiveDate::from_ymd_opt(2026, 11, 8).unwrap()));
        let every_other = json!({"repeatRule":"RRULE:FREQ=DAILY;INTERVAL=2","targetStartDate":20261102});
        assert!(habit_due(&every_other, wednesday));
        assert!(!habit_due(&every_other, wednesday.succ_opt().unwrap()));
        assert!(!habit_due(&json!({"repeatRule":"RRULE:FREQ=DAILY;INTERVAL=1","targetStartDate":20261105}), wednesday));
        assert!(!habit_due(&json!({"status":1,"repeatRule":"RRULE:FREQ=DAILY"}), wednesday));
        assert!(habit_due(&json!({"repeatRule":"RRULE:FREQ=WEEKLY;TT_TIMES=3"}), wednesday));
    }
}

mod status;
