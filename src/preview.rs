use crate::{
    api::{Snapshot, Task},
    config::Theme,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::{prelude::*, widgets::*};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    Content,
    Description,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Markdown {
        field: Field,
        offset: usize,
        checked: bool,
    },
    Checklist(usize),
    Subtask(String),
}
#[derive(Clone)]
pub struct Row {
    pub line: Line<'static>,
    pub action: Option<Action>,
    /// First Markdown link destination on the row, opened with `gx`.
    pub link: Option<String>,
}
impl Row {
    fn text(line: impl Into<Line<'static>>) -> Self {
        Self {
            line: line.into(),
            action: None,
            link: None,
        }
    }
}
pub fn color(s: &str) -> Color {
    s.parse().unwrap_or(Color::Reset)
}

struct Markdown<'a> {
    theme: &'a Theme,
    rows: Vec<Row>,
    spans: Vec<Span<'static>>,
    action: Option<Action>,
    link: Option<String>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    /// Per open list item: whether it pushed the "done" style.
    items: Vec<bool>,
    quote: usize,
    code: bool,
}
impl<'a> Markdown<'a> {
    fn style(&self) -> Style {
        self.styles.iter().fold(
            Style::default().fg(color(&self.theme.foreground)),
            |a, b| a.patch(*b),
        )
    }
    fn push(&mut self, value: impl Into<String>) {
        let style = self.style();
        self.spans.push(Span::styled(value.into(), style));
    }
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        self.rows.push(Row {
            line: Line::from(std::mem::take(&mut self.spans)),
            action: self.action.take(),
            link: self.link.take(),
        });
    }
    fn blank(&mut self) {
        self.flush();
        if self.rows.last().is_some_and(|r| r.line.width() > 0) {
            self.rows.push(Row::text(""));
        }
    }
    fn prefix(&mut self) {
        if self.spans.is_empty() && self.quote > 0 {
            self.spans.push(Span::styled(
                "│ ".repeat(self.quote),
                Style::default().fg(color(&self.theme.muted)),
            ));
        }
    }
}
fn markdown(source: &str, field: Field, theme: &Theme) -> Vec<Row> {
    let mut m = Markdown {
        theme,
        rows: vec![],
        spans: vec![],
        action: None,
        link: None,
        styles: vec![],
        lists: vec![],
        items: vec![],
        quote: 0,
        code: false,
    };
    for (event, range) in Parser::new_ext(
        source,
        Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    )
    .into_offset_iter()
    {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    m.blank();
                    m.styles
                        .push(Style::default().fg(color(&theme.heading)).bold());
                }
                Tag::Paragraph => {
                    if m.lists.is_empty() {
                        m.flush();
                    }
                    m.prefix();
                }
                Tag::Strong => m.styles.push(Style::default().bold()),
                Tag::Emphasis => m.styles.push(Style::default().italic()),
                Tag::Strikethrough => m.styles.push(Style::default().crossed_out()),
                Tag::Link { dest_url, .. } => {
                    m.link.get_or_insert(dest_url.to_string());
                    m.styles
                        .push(Style::default().fg(color(&theme.accent)).underlined());
                }
                Tag::Image { .. } => m
                    .styles
                    .push(Style::default().fg(color(&theme.accent)).underlined()),
                Tag::List(start) => {
                    m.flush();
                    m.lists.push(start);
                }
                Tag::Item => {
                    m.items.push(false);
                    m.flush();
                    m.prefix();
                    let indent = "  ".repeat(m.lists.len().saturating_sub(1));
                    let marker = match m.lists.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            marker
                        }
                        _ => "• ".into(),
                    };
                    m.push(format!("{indent}{marker}"));
                }
                Tag::BlockQuote(_) => {
                    m.blank();
                    m.quote += 1;
                    m.styles.push(Style::default().fg(color(&theme.muted)));
                }
                Tag::CodeBlock(kind) => {
                    m.blank();
                    m.code = true;
                    let language = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(s) => s.to_string(),
                        _ => String::new(),
                    };
                    m.rows.push(Row::text(Span::styled(
                        format!("╭─ {language}"),
                        Style::default().fg(color(&theme.muted)),
                    )));
                    m.styles.push(
                        Style::default()
                            .bg(color(&theme.code_background))
                            .fg(color(&theme.code)),
                    );
                }
                Tag::Table(_) | Tag::TableHead | Tag::TableRow => m.flush(),
                _ => (),
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    m.flush();
                    m.styles.pop();
                    m.blank();
                }
                TagEnd::Paragraph => {
                    if m.lists.is_empty() {
                        m.blank();
                    } else {
                        m.flush();
                    }
                }
                TagEnd::Strong
                | TagEnd::Emphasis
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image => {
                    m.styles.pop();
                }
                TagEnd::Item => {
                    m.flush();
                    if m.items.pop() == Some(true) {
                        m.styles.pop();
                    }
                }
                TagEnd::List(_) => {
                    m.flush();
                    m.lists.pop();
                    if m.lists.is_empty() {
                        m.blank();
                    }
                }
                TagEnd::BlockQuote(_) => {
                    m.flush();
                    m.quote = m.quote.saturating_sub(1);
                    m.styles.pop();
                    m.blank();
                }
                TagEnd::CodeBlock => {
                    m.flush();
                    m.styles.pop();
                    m.code = false;
                    m.rows.push(Row::text(Span::styled(
                        "╰─",
                        Style::default().fg(color(&theme.muted)),
                    )));
                    m.blank();
                }
                TagEnd::TableCell => m.push("   │   "),
                TagEnd::TableHead | TagEnd::TableRow => m.flush(),
                TagEnd::Table => m.blank(),
                _ => (),
            },
            Event::Text(text) => {
                if m.code {
                    for line in text.lines() {
                        m.push(format!("│ {line}"));
                        m.flush();
                    }
                } else {
                    m.prefix();
                    m.push(text.to_string());
                }
            }
            Event::Code(text) => {
                m.spans.push(Span::styled(
                    text.to_string(),
                    m.style()
                        .fg(color(&theme.code))
                        .bg(color(&theme.code_background)),
                ));
            }
            Event::TaskListMarker(checked) => {
                // The parser's source range points to the real task marker, excluding code blocks.
                if let Some(local) = source[range.clone()].find('[') {
                    let offset = range.start + local + 1;
                    m.action = Some(Action::Markdown {
                        field: field.clone(),
                        offset,
                        checked,
                    });
                }
                if checked && let Some(done) = m.items.last_mut() {
                    *done = true;
                    m.styles
                        .push(Style::default().fg(color(&theme.muted)).crossed_out());
                }
                m.spans.push(Span::styled(
                    if checked { "● " } else { "○ " },
                    Style::default()
                        .fg(color(if checked {
                            &theme.success
                        } else {
                            &theme.accent
                        }))
                        .bold(),
                ));
            }
            Event::SoftBreak => {
                m.push(" ");
            }
            Event::HardBreak => {
                m.flush();
                m.prefix();
            }
            Event::Rule => {
                m.blank();
                m.rows.push(Row::text(Span::styled(
                    "────────────────────────",
                    Style::default().fg(color(&theme.border)),
                )));
                m.blank();
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                m.push(text.to_string());
            }
            _ => (),
        }
    }
    m.flush();
    m.rows
}
/// Wrap graphemes before scrolling so cursor and checkbox actions use the same visual rows.
pub fn wrap(rows: Vec<Row>, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut result = Vec::new();
    for row in rows {
        let mut tokens: Vec<Vec<(String, Style)>> = vec![];
        let mut whitespace = None;
        for span in row.line.spans {
            for g in span.content.graphemes(true) {
                let space = g.chars().all(char::is_whitespace);
                if whitespace != Some(space) {
                    tokens.push(Vec::new());
                    whitespace = Some(space);
                }
                tokens.last_mut().unwrap().push((g.to_owned(), span.style));
            }
        }
        let mut spans = vec![];
        let mut used = 0;
        let mut wrapped = false;
        for token in tokens {
            let size = token
                .iter()
                .map(|(s, _)| UnicodeWidthStr::width(s.as_str()))
                .sum::<usize>();
            let space = token
                .first()
                .is_some_and(|(s, _)| s.chars().all(char::is_whitespace));
            if used > 0 && used + size > width && !space && size <= width {
                result.push(Row {
                    line: Line::from(std::mem::take(&mut spans)),
                    action: row.action.clone(),
                    link: row.link.clone(),
                });
                used = 0;
                wrapped = true;
            }
            if wrapped && used == 0 && space {
                continue;
            }
            for (g, style) in token {
                let size = UnicodeWidthStr::width(g.as_str());
                if used + size > width && used > 0 {
                    result.push(Row {
                        line: Line::from(std::mem::take(&mut spans)),
                        action: row.action.clone(),
                        link: row.link.clone(),
                    });
                    used = 0;
                    wrapped = true;
                    if space {
                        continue;
                    }
                }
                spans.push(Span::styled(g, style));
                used += size;
            }
        }
        result.push(Row {
            line: Line::from(spans),
            action: row.action,
            link: row.link,
        });
    }
    result
}
pub fn document(
    task: &Task,
    data: &Snapshot,
    theme: &Theme,
    width: usize,
    hide_done: bool,
) -> Vec<Row> {
    let mut rows = vec![
        Row::text(Span::styled(
            task.title.clone(),
            Style::default().fg(color(&theme.heading)).bold(),
        )),
        Row::text(""),
    ];
    let project = data
        .projects
        .iter()
        .find(|p| p.id == task.project_id)
        .map(|p| p.name.as_str())
        .unwrap_or("");
    let field = |label: &str, value: String, fg: &str| {
        Row::text(Line::from(vec![
            Span::styled(
                format!("{label:<14}"),
                Style::default().fg(color(&theme.muted)),
            ),
            Span::styled(value, Style::default().fg(color(fg))),
        ]))
    };
    let (priority, priority_color) = match task.priority {
        5 => ("● Alta", &theme.danger),
        3 => ("● Média", &theme.accent),
        1 => ("● Baixa", &theme.muted),
        _ => ("Nenhuma", &theme.muted),
    };
    rows.push(field("Lista", project.to_owned(), &theme.foreground));
    rows.push(field("Prioridade", priority.into(), priority_color));
    if let Some(due) = &task.due_date {
        let all_day = task.extra.get("isAllDay").and_then(|v| v.as_bool()) == Some(true);
        let tz = data.time_zone.as_ref().and_then(|s| s.parse().ok());
        rows.push(field(
            "Vencimento",
            crate::fields::format_due(due, all_day, tz),
            &theme.accent,
        ));
    }
    if let Some(reminders) = task
        .extra
        .get("reminders")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .filter(|r| !r.is_empty())
    {
        rows.push(field(
            "Lembretes",
            crate::fields::format_reminders(&reminders),
            &theme.foreground,
        ));
    }
    if let Some(parent) = task
        .parent_id
        .as_ref()
        .and_then(|id| data.tasks.iter().find(|t| &t.id == id))
    {
        rows.push(field(
            "Subtarefa de",
            parent.title.clone(),
            &theme.foreground,
        ));
    }
    rows.push(Row::text(""));
    rows.push(Row::text(Span::styled(
        "─".repeat(width),
        Style::default().fg(color(&theme.border)),
    )));
    rows.push(Row::text(""));
    rows.extend(markdown(&task.content, Field::Content, theme));
    if let Some(desc) = task.desc.as_ref().filter(|s| !s.is_empty()) {
        rows.extend(markdown(desc, Field::Description, theme));
    }
    if !task.items.is_empty() {
        if rows.last().is_some_and(|r| r.line.width() > 0) {
            rows.push(Row::text(""));
        }
        rows.push(Row::text(Line::from(vec![
            Span::styled(
                "Checklist  ",
                Style::default().fg(color(&theme.heading)).bold(),
            ),
            Span::styled(
                format!(
                    "{}/{}",
                    task.items.iter().filter(|i| i.status == 1).count(),
                    task.items.len()
                ),
                Style::default().fg(color(&theme.muted)),
            ),
        ])));
        rows.push(Row::text(""));
        for (i, item) in task.items.iter().enumerate() {
            if hide_done && item.status == 1 {
                continue;
            }
            rows.push(Row {
                line: Line::from(vec![
                    Span::styled(
                        if item.status == 1 { "● " } else { "○ " },
                        Style::default()
                            .fg(color(if item.status == 1 {
                                &theme.success
                            } else {
                                &theme.accent
                            }))
                            .bold(),
                    ),
                    Span::styled(
                        item.title.clone(),
                        if item.status == 1 {
                            Style::default().fg(color(&theme.muted)).crossed_out()
                        } else {
                            Style::default().fg(color(&theme.foreground))
                        },
                    ),
                ]),
                action: Some(Action::Checklist(i)),
                link: None,
            });
        }
        let hidden = task.items.iter().filter(|i| i.status == 1).count();
        if hide_done && hidden > 0 {
            rows.push(Row::text(Span::styled(
                format!("  {hidden} concluído(s) oculto(s) · zh mostra"),
                Style::default().fg(color(&theme.muted)).italic(),
            )));
        }
    }
    let children = data
        .tasks
        .iter()
        .filter(|t| t.parent_id.as_deref() == Some(&task.id))
        .collect::<Vec<_>>();
    if !children.is_empty() {
        rows.push(Row::text(""));
        rows.push(Row::text(Span::styled(
            format!("SUBTAREFAS   {}", children.len()),
            Style::default().fg(color(&theme.muted)).bold(),
        )));
        rows.push(Row::text(""));
        for t in children {
            rows.push(Row {
                line: Line::from(vec![
                    Span::styled("○  ", Style::default().fg(color(&theme.accent))),
                    Span::raw(t.title.clone()),
                    Span::styled("  ↵", Style::default().fg(color(&theme.muted))),
                ]),
                action: Some(Action::Subtask(t.id.clone())),
                link: None,
            });
        }
    }
    wrap(rows, width)
}
fn source<'a>(task: &'a mut Task, field: &Field) -> Option<&'a mut String> {
    match field {
        Field::Content => Some(&mut task.content),
        Field::Description => task.desc.as_mut(),
    }
}
/// Byte ranges of a Markdown task marker's line: (line start, text start, line end).
fn task_line(source: &str, offset: usize) -> Option<(usize, usize, usize)> {
    let bytes = source.as_bytes();
    if !matches!(bytes.get(offset), Some(b' ' | b'x' | b'X'))
        || bytes.get(offset + 1) != Some(&b']')
    {
        return None;
    }
    let start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |i| offset + i);
    let rest = &source[offset + 2..end];
    Some((start, end - rest.trim_start().len(), end))
}
/// Title of a checklist item or Markdown task. Subtasks live in other tasks.
pub fn label(task: &Task, action: &Action) -> Option<String> {
    match action {
        Action::Checklist(i) => task.items.get(*i).map(|i| i.title.clone()),
        Action::Markdown { field, offset, .. } => {
            let source = match field {
                Field::Content => &task.content,
                Field::Description => task.desc.as_ref()?,
            };
            let (_, text, end) = task_line(source, *offset)?;
            Some(source[text..end].to_owned())
        }
        Action::Subtask(_) => None,
    }
}
pub fn rename(task: &mut Task, action: &Action, title: &str) -> bool {
    match action {
        Action::Checklist(i) => task
            .items
            .get_mut(*i)
            .map(|i| i.title = title.into())
            .is_some(),
        Action::Markdown { field, offset, .. } => source(task, field)
            .and_then(|s| {
                task_line(s, *offset).map(|(_, text, end)| s.replace_range(text..end, title))
            })
            .is_some(),
        Action::Subtask(_) => false,
    }
}
pub fn remove(task: &mut Task, action: &Action) -> bool {
    match action {
        Action::Checklist(i) if *i < task.items.len() => {
            task.items.remove(*i);
            true
        }
        Action::Markdown { field, offset, .. } => source(task, field)
            .and_then(|s| {
                let (start, _, end) = task_line(s, *offset)?;
                // Drop the line together with one of its newlines.
                let range = if end < s.len() {
                    start..end + 1
                } else {
                    start.saturating_sub(1)..end
                };
                s.replace_range(range, "");
                Some(())
            })
            .is_some(),
        _ => false,
    }
}
pub fn toggle(task: &mut Task, action: &Action) -> bool {
    match action {
        Action::Markdown {
            field,
            offset,
            checked,
        } => {
            let Some(source) = source(task, field) else {
                return false;
            };
            if !matches!(source.as_bytes().get(*offset), Some(b' ' | b'x' | b'X')) {
                return false;
            }
            source.replace_range(*offset..*offset + 1, if *checked { " " } else { "x" });
            true
        }
        Action::Checklist(index) => {
            if let Some(item) = task.items.get_mut(*index) {
                item.status = if item.status == 1 { 0 } else { 1 };
                item.extra.remove("completedTime");
                true
            } else {
                false
            }
        }
        Action::Subtask(_) => false,
    }
}
pub fn render(
    f: &mut Frame,
    area: Rect,
    rows: &[Row],
    cursor: usize,
    focused: bool,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }
    let scroll = cursor.saturating_sub(area.height.saturating_sub(3) as usize);
    let lines = rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(area.height as usize)
        .map(|(i, row)| {
            let mut line = row.line.clone();
            if focused && i == cursor {
                line = line.style(Style::default().bg(color(&theme.selection)));
            }
            line
        })
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines), area);
    if rows.len() > area.height as usize {
        let mut state = ScrollbarState::new(rows.len())
            .position(cursor)
            .viewport_content_length(area.height as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(color(&theme.accent)))
                .track_style(Style::default().fg(color(&theme.border))),
            area,
            &mut state,
        );
    }
}
