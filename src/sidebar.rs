use crate::api::{Snapshot, Task};
use chrono::{DateTime, Local, NaiveDate, Utc};
#[derive(Clone, Debug, PartialEq)]
pub enum View {
    All,
    Today,
    Inbox,
    Upcoming,
    Group(String),
    Project(String),
}
pub struct Entry {
    pub view: View,
    pub name: String,
    pub depth: usize,
    pub count: usize,
}
pub fn today(data: &Snapshot) -> NaiveDate {
    data.time_zone
        .as_ref()
        .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
        .map(|tz| Utc::now().with_timezone(&tz).date_naive())
        .unwrap_or_else(|| Local::now().date_naive())
}
fn date(value: &str, task: &Task, data: &Snapshot) -> Option<NaiveDate> {
    if task.extra.get("isAllDay").and_then(|v| v.as_bool()) == Some(true) {
        return NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok();
    }
    let parsed = DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f%z"))
        .ok()?;
    Some(
        data.time_zone
            .as_ref()
            .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
            .map(|tz| parsed.with_timezone(&tz).date_naive())
            .unwrap_or_else(|| parsed.with_timezone(&Local).date_naive()),
    )
}
pub fn matches(view: &View, t: &Task, data: &Snapshot) -> bool {
    matches_on(view, t, data, today(data))
}
pub fn matches_on(view: &View, t: &Task, data: &Snapshot, today: NaiveDate) -> bool {
    match view {
        View::All => true,
        View::Project(id) => &t.project_id == id,
        View::Inbox => {
            data.inbox_id.as_ref().is_some_and(|id| id == &t.project_id) || t.project_id == "inbox"
        }
        View::Group(id) => data
            .projects
            .iter()
            .any(|p| p.id == t.project_id && p.group_id.as_ref() == Some(id)),
        View::Today | View::Upcoming => {
            let due = t.due_date.as_deref().and_then(|s| date(s, t, data));
            let start = t
                .extra
                .get("startDate")
                .and_then(|v| v.as_str())
                .and_then(|s| date(s, t, data));
            if *view == View::Today {
                due.is_some_and(|d| d <= today) || start.is_some_and(|d| d <= today)
            } else {
                [due, start]
                    .into_iter()
                    .flatten()
                    .any(|d| d > today && d <= today + chrono::Duration::days(7))
            }
        }
    }
}
pub fn entries(data: &Snapshot, collapsed: &std::collections::HashSet<String>) -> Vec<Entry> {
    let mut rows = Vec::new();
    let mut add = |view: View, name: String, depth| {
        let count = data
            .tasks
            .iter()
            .filter(|t| matches(&view, t, data))
            .count();
        rows.push(Entry {
            view,
            name,
            depth,
            count,
        });
    };
    add(View::All, "Todas as tarefas".into(), 0);
    add(View::Today, "Today".into(), 0);
    add(View::Inbox, "Inbox".into(), 0);
    add(View::Upcoming, "Próximos 7 dias".into(), 0);
    // Groups and root lists keep TickTick's sortOrder. Child lists retain their own order.
    let mut roots = data
        .groups
        .iter()
        .map(|g| (g.sort_order, true, g.id.as_str()))
        .chain(
            data.projects
                .iter()
                .filter(|p| {
                    p.group_id
                        .as_ref()
                        .is_none_or(|id| !data.groups.iter().any(|g| &g.id == id))
                })
                .filter(|p| data.inbox_id.as_ref() != Some(&p.id))
                .map(|p| (p.sort_order, false, p.id.as_str())),
        )
        .collect::<Vec<_>>();
    roots.sort_by_key(|v| v.0);
    for (_, group, id) in roots {
        if group {
            let g = data.groups.iter().find(|g| g.id == id).unwrap();
            add(View::Group(g.id.clone()), g.name.clone(), 0);
            if !collapsed.contains(&format!("group:{id}")) {
                for p in data
                    .projects
                    .iter()
                    .filter(|p| p.group_id.as_deref() == Some(id))
                {
                    add(View::Project(p.id.clone()), p.name.clone(), 1);
                }
            }
        } else {
            let p = data.projects.iter().find(|p| p.id == id).unwrap();
            add(View::Project(p.id.clone()), p.name.clone(), 0);
        }
    }
    rows
}
