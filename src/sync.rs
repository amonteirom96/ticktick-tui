use crate::{
    api::{Api, Snapshot, Task},
    config,
};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::mpsc::{self, Receiver, Sender},
};
#[derive(Clone, Debug)]
pub enum Job {
    Refresh,
    LoadTask {
        request: u64,
        project: String,
        id: String,
    },
    Search {
        id: u64,
        query: String,
    },
    SaveFocus(crate::wellness::FocusUpload),
    /// `value` equal to `goal` completes the habit; 0 undoes it.
    CheckHabit {
        id: String,
        day: chrono::NaiveDate,
        goal: f64,
        value: f64,
    },
    Report(chrono::NaiveDate),
    Create(Task),
    Patch {
        id: String,
        project: String,
        fields: Map<String, Value>,
    },
    Complete(Task),
    Reopen(Task),
    Delete(Task),
    Move {
        task: Task,
        to: String,
    },
}
pub enum Event {
    TaskLoaded {
        request: u64,
        result: Result<Task, String>,
    },
    Search {
        id: u64,
        result: Result<Vec<Task>, String>,
    },
    FocusSaved {
        id: String,
        result: Result<Snapshot, String>,
    },
    Data(Snapshot, String),
    Report(chrono::NaiveDate, Snapshot, String),
    Applied {
        original: Box<Job>,
        actual: Box<Job>,
        data: Snapshot,
        message: String,
    },
    Error {
        message: String,
        mutation: bool,
    },
}
/// Remember an API task without treating a search result as a user mutation.
pub fn remember(data: &mut Snapshot, task: &Task) {
    let tasks = if task.status == 2 {
        &mut data.completed
    } else {
        &mut data.tasks
    };
    if let Some(existing) = tasks.iter_mut().find(|existing| existing.id == task.id) {
        *existing = task.clone();
    } else {
        tasks.push(task.clone());
    }
}
/// The same operation is used for optimistic rendering, commit and rollback replay.
pub fn apply(data: &mut Snapshot, job: &Job) {
    match job {
        Job::LoadTask { .. }
        | Job::Search { .. }
        | Job::SaveFocus(_)
        | Job::Refresh
        | Job::Report(_)
        | Job::CheckHabit { .. } => (),
        Job::Reopen(task) => {
            let mut task = task.clone();
            task.status = 0;
            task.extra.remove("completedTime");
            data.completed.retain(|t| t.id != task.id);
            if !data.tasks.iter().any(|t| t.id == task.id) {
                data.tasks.push(task);
            }
        }
        Job::Create(task) => {
            if !data.tasks.iter().any(|t| t.id == task.id) {
                data.tasks.push(task.clone());
            }
        }
        Job::Patch { id, fields, .. } => {
            if let Some(t) = data
                .tasks
                .iter_mut()
                .chain(data.completed.iter_mut())
                .find(|t| &t.id == id)
            {
                let mut value = serde_json::to_value(&*t).expect("task serializes");
                for (key, v) in fields {
                    value[key] = v.clone();
                }
                if let Ok(task) = serde_json::from_value(value) {
                    *t = task;
                }
            }
        }
        Job::Move { task, to } => {
            if let Some(t) = data.tasks.iter_mut().find(|t| t.id == task.id) {
                t.project_id = to.clone();
            }
        }
        Job::Complete(task) | Job::Delete(task) => {
            let mut removed = HashSet::from([task.id.clone()]);
            loop {
                let old = removed.len();
                for t in &data.tasks {
                    if t.parent_id.as_ref().is_some_and(|p| removed.contains(p)) {
                        removed.insert(t.id.clone());
                    }
                }
                if old == removed.len() {
                    break;
                }
            }
            if matches!(job, Job::Complete(_)) {
                for mut t in data
                    .tasks
                    .iter()
                    .filter(|t| removed.contains(&t.id))
                    .cloned()
                {
                    t.status = 2;
                    t.extra.insert(
                        "completedTime".into(),
                        Value::String(chrono::Utc::now().to_rfc3339()),
                    );
                    if !data.completed.iter().any(|x| {
                        x.id == t.id
                            && x.extra
                                .get("completedTime")
                                .and_then(Value::as_str)
                                .and_then(|v| crate::sidebar::date(v, &Task::default(), data))
                                == Some(crate::sidebar::today(data))
                    }) {
                        data.completed.push(t);
                    }
                }
            }
            data.tasks.retain(|t| !removed.contains(&t.id));
        }
    }
}
pub fn remap(job: &mut Job, old: &str, new: &str) {
    let map_task = |t: &mut Task| {
        if t.id == old {
            t.id = new.into();
        }
        if t.parent_id.as_deref() == Some(old) {
            t.parent_id = Some(new.into());
        }
    };
    match job {
        Job::Create(t)
        | Job::Reopen(t)
        | Job::Complete(t)
        | Job::Delete(t)
        | Job::Move { task: t, .. } => map_task(t),
        Job::Patch { id, fields, .. } => {
            if id == old {
                *id = new.into();
            }
            if fields.get("parentId").and_then(Value::as_str) == Some(old) {
                fields.insert("parentId".into(), Value::String(new.into()));
            }
        }
        Job::LoadTask { .. }
        | Job::Search { .. }
        | Job::SaveFocus(_)
        | Job::Refresh
        | Job::Report(_)
        | Job::CheckHabit { .. } => (),
    }
}
fn reflected(data: &Snapshot, job: &Job) -> bool {
    match job {
        Job::Create(t) | Job::Reopen(t) => data.tasks.iter().any(|x| x.id == t.id),
        Job::Complete(t) | Job::Delete(t) => !data.tasks.iter().any(|x| x.id == t.id),
        Job::Move { task, to } => data
            .tasks
            .iter()
            .any(|x| x.id == task.id && &x.project_id == to),
        Job::Patch { id, fields, .. } => data
            .tasks
            .iter()
            .chain(data.completed.iter())
            .find(|t| &t.id == id)
            .is_some_and(|t| {
                let v = serde_json::to_value(t).unwrap();
                fields.iter().all(|(k, x)| v.get(k) == Some(x))
            }),
        Job::LoadTask { .. }
        | Job::Search { .. }
        | Job::SaveFocus(_)
        | Job::Refresh
        | Job::Report(_)
        | Job::CheckHabit { .. } => true,
    }
}
/// A newer patch of the same field supersedes the older one, so it is never replayed over it.
fn track(unindexed: &mut Vec<(Job, std::time::Instant)>, job: Job) {
    if let Job::Reopen(task) | Job::Complete(task) = &job {
        unindexed.retain(
            |(old, _)| !matches!(old, Job::Reopen(t) | Job::Complete(t) if t.id == task.id),
        );
    }
    if let Job::Patch { id, fields, .. } = &job {
        for (old, _) in unindexed.iter_mut() {
            if let Job::Patch {
                id: o, fields: f, ..
            } = old
                && o == id
            {
                f.retain(|k, _| !fields.contains_key(k));
            }
        }
    }
    unindexed.push((job, std::time::Instant::now()));
}
fn cache(data: &Snapshot) -> Option<String> {
    serde_json::to_vec(data)
        .map_err(anyhow::Error::from)
        .and_then(|v| config::private_write(&config::cache_path(), &v))
        .err()
        .map(|_| "Could not update local cache".into())
}
/// Recurring completions receive a new ID in TickTick's history. Match only
/// the local recurring copy, with its schedule and completion instant intact.
fn recurring_completion(local: &Task, remote: &Task) -> bool {
    let stamp = |task: &Task| {
        task.extra
            .get("completedTime")
            .and_then(Value::as_str)
            .and_then(|v| {
                chrono::DateTime::parse_from_rfc3339(v)
                    .or_else(|_| chrono::DateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f%z"))
                    .ok()
            })
    };
    local.id != remote.id
        && local
            .extra
            .get("repeatFlag")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        && !remote
            .extra
            .get("repeatFlag")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        && local.project_id == remote.project_id
        && local.title == remote.title
        && local.parent_id == remote.parent_id
        && local.due_date == remote.due_date
        && local.extra.get("startDate") == remote.extra.get("startDate")
        && stamp(local)
            .zip(stamp(remote))
            .is_some_and(|(a, b)| (a - b).num_milliseconds().unsigned_abs() <= 5_000)
}

fn merge_completed(data: &mut Snapshot, tasks: Vec<Task>, day: chrono::NaiveDate) {
    for task in tasks {
        // Remove both the optimistic copy and any stale duplicate already cached.
        let previous = std::mem::take(&mut data.completed);
        data.completed = previous
            .into_iter()
            .filter(|old| {
                let old_day = old
                    .extra
                    .get("completedTime")
                    .and_then(Value::as_str)
                    .and_then(|v| crate::sidebar::date(v, &Task::default(), data));
                !(old.id == task.id && old_day == Some(day)) && !recurring_completion(old, &task)
            })
            .collect();
        data.completed.push(task);
    }
    if !data.report_days.contains(&day) {
        data.report_days.push(day);
    }
}

fn fetch_report(api: &Api, data: &mut Snapshot, day: chrono::NaiveDate) -> String {
    let message = match api.completed_on(day, data) {
        Ok(tasks) => {
            merge_completed(data, tasks, day);
            format!(
                "Report for {} updated · Synced with TickTick",
                day.format("%d/%m/%Y")
            )
        }
        Err(e) => {
            data.report_days.retain(|d| *d != day);
            format!("History unavailable: {e}. Showing local records.")
        }
    };
    let focus = match api.focus_on(day, data) {
        Ok(totals) => crate::focus::FocusDay {
            day,
            totals,
            error: None,
        },
        Err(e) => crate::focus::FocusDay {
            day,
            totals: vec![],
            error: Some(e.to_string()),
        },
    };
    data.focus_days.retain(|f| f.day != day);
    data.focus_days.push(focus);
    message
}

pub fn worker(api: Api, initial: Snapshot) -> (Sender<Job>, Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Job>();
    let (out, events) = mpsc::channel();
    std::thread::spawn(move || {
        let mut aliases = HashMap::<String, String>::new();
        let mut known = initial;
        let mut report_day = crate::sidebar::today(&known);
        let mut profile_checked = known.user_name.is_some();
        let mut history_available = true;
        // Keep confirmed writes visible while TickTick's list index catches up.
        let mut unindexed = Vec::<(Job, std::time::Instant)>::new();
        let mut queue = std::collections::VecDeque::new();
        loop {
            queue.extend(rx.try_iter());
            let Some(mut original) = queue.pop_front().or_else(|| rx.recv().ok()) else {
                break;
            };
            if let Job::SaveFocus(upload) = &original {
                let mut body = upload.body.clone();
                if let Some(id) = body["taskId"].as_str().map(str::to_owned)
                    && let Some(remote) = aliases.get(&id)
                {
                    body["taskId"] = remote.clone().into();
                }
                let unresolved = body["taskId"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("local:"));
                let result = if unresolved {
                    Err(anyhow::anyhow!(
                        "The focus task has not been saved to TickTick yet"
                    ))
                } else {
                    api.create_focus(&body)
                }
                .map(|response| {
                    let mut record = body.clone();
                    record["id"] = response
                        .get("id")
                        .cloned()
                        .unwrap_or_else(|| Value::String(upload.id.clone()));
                    record["duration"] =
                        Value::from(body["duration"].as_u64().unwrap_or(0).saturating_mul(1000));
                    record["status"] = Value::from(1);
                    record
                })
                .map_err(|e| e.to_string());
                let result = result.map(|record| {
                    known.focus_records.retain(|r| r["id"] != record["id"]);
                    let first = record["startTime"]
                        .as_str()
                        .and_then(|v| crate::sidebar::date(v, &Task::default(), &known));
                    let last = record["endTime"]
                        .as_str()
                        .and_then(|v| crate::sidebar::date(v, &Task::default(), &known));
                    known.focus_records.push(record);
                    for day in &mut known.focus_days {
                        if first
                            .zip(last)
                            .is_some_and(|(first, last)| day.day >= first && day.day <= last)
                        {
                            day.error =
                                Some("New session saved · press r in the report to refresh".into());
                        }
                    }
                    let _ = cache(&known);
                    known.clone()
                });
                if out
                    .send(Event::FocusSaved {
                        id: upload.id.clone(),
                        result,
                    })
                    .is_err()
                {
                    break;
                }
                continue;
            }
            // Burst of edits on one task (e.g. `p` pressed repeatedly): one request carries them all.
            let mut merged = 1;
            if let Job::Patch { id, fields, .. } = &mut original {
                queue.extend(rx.try_iter());
                while let Some(Job::Patch {
                    id: next,
                    fields: more,
                    ..
                }) = queue.front()
                    && next == id
                {
                    fields.extend(more.clone());
                    queue.pop_front();
                    merged += 1;
                }
            }
            let mut job = original.clone();
            for (old, new) in &aliases {
                remap(&mut job, old, new);
            }
            if let Job::CheckHabit {
                ref id,
                day,
                goal,
                value,
            } = job
            {
                match api.check_habit(id, day, goal, value) {
                    Ok(()) => {
                        crate::wellness::record_checkin(&mut known, id, day, goal, value);
                        let mut message = if value > 0.0 {
                            "Habit completed · saved to TickTick"
                        } else {
                            "Habit reset · saved to TickTick"
                        }
                        .to_string();
                        if let Some(warning) = cache(&known) {
                            message.push_str(&format!(" · {warning}"));
                        }
                        if out.send(Event::Data(known.clone(), message)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = out.send(Event::Error {
                            message: format!(
                                "Could not {} habit: {error}. Press r to check before retrying.",
                                if value > 0.0 { "complete" } else { "reset" }
                            ),
                            mutation: false,
                        });
                    }
                }
                continue;
            }
            if let Job::LoadTask {
                request,
                ref project,
                ref id,
            } = job
            {
                let result = api.task(project, id).map_err(|error| error.to_string());
                if let Ok(task) = &result {
                    remember(&mut known, task);
                }
                if out.send(Event::TaskLoaded { request, result }).is_err() {
                    break;
                }
                continue;
            }
            if let Job::Search { id, ref query } = job {
                let result = api.search(query).map_err(|error| error.to_string());
                if let Ok(tasks) = &result {
                    for task in tasks {
                        remember(&mut known, task);
                    }
                }
                if out.send(Event::Search { id, result }).is_err() {
                    break;
                }
                continue;
            }
            if let Job::Report(day) = job {
                report_day = day;
                let mut message = fetch_report(&api, &mut known, day);
                if let Some(warning) = cache(&known) {
                    message = format!("{message} · {warning}");
                }
                if out
                    .send(Event::Report(day, known.clone(), message))
                    .is_err()
                {
                    break;
                }
                continue;
            }
            let mutation = !matches!(
                job,
                Job::LoadTask { .. }
                    | Job::Search { .. }
                    | Job::SaveFocus(_)
                    | Job::Refresh
                    | Job::Report(_)
                    | Job::CheckHabit { .. }
            );
            let unresolved = match &job {
                Job::Create(t) => t
                    .parent_id
                    .as_deref()
                    .is_some_and(|s| s.starts_with("local:")),
                Job::Patch { id, .. } => id.starts_with("local:"),
                Job::Reopen(t) | Job::Complete(t) | Job::Delete(t) | Job::Move { task: t, .. } => {
                    t.id.starts_with("local:")
                }
                Job::LoadTask { .. }
                | Job::Search { .. }
                | Job::SaveFocus(_)
                | Job::Refresh
                | Job::Report(_)
                | Job::CheckHabit { .. } => false,
            };
            let result = if unresolved {
                Err(anyhow::anyhow!("The previous task could not be created"))
            } else {
                match &job {
                    Job::LoadTask { .. }
                    | Job::Search { .. }
                    | Job::SaveFocus(_)
                    | Job::Refresh
                    | Job::Report(_)
                    | Job::CheckHabit { .. } => Ok(job.clone()),
                    Job::Create(t) => api.create(t).map(|remote| {
                        aliases.insert(t.id.clone(), remote.id.clone());
                        Job::Create(remote)
                    }),
                    Job::Patch {
                        id,
                        project,
                        fields,
                    } => api.patch(id, project, fields.clone()).map(|_| job.clone()),
                    Job::Reopen(t) => api.reopen(t).map(|_| job.clone()),
                    Job::Complete(t) => api.complete(t).map(|_| job.clone()),
                    Job::Delete(t) => api.delete(t).map(|_| job.clone()),
                    Job::Move { task, to } => api.move_task(task, to).map(|_| job.clone()),
                }
            };
            let actual = match result {
                Ok(job) => job,
                Err(e) => {
                    for _ in 0..merged {
                        let _=out.send(Event::Error{message:format!("Save failed: {e}. Local change reverted; press r to check before retrying."),mutation});
                    }
                    continue;
                }
            };
            if mutation {
                apply(&mut known, &actual);
                track(&mut unindexed, actual.clone());
            }
            let mut message = String::from("Synced with TickTick");
            // More writes queued (e.g. reorder burst): skip the full refetch until the last one.
            queue.extend(rx.try_iter());
            let busy = mutation && !queue.is_empty();
            match (!busy).then(|| api.snapshot()) {
                None => {}
                Some(Ok(mut data)) => {
                    data.user_name = known.user_name.clone();
                    if !profile_checked {
                        data.user_name = api.profile_name();
                        profile_checked = true;
                    }
                    data.completed = known.completed.clone();
                    data.report_days = known.report_days.clone();
                    data.focus_days = known.focus_days.clone();
                    if matches!(job, Job::Report(_)) || (!mutation && history_available) {
                        match api.completed_on(report_day, &data) {
                            Ok(tasks) => {
                                history_available = true;
                                merge_completed(&mut data, tasks, report_day);
                            }
                            Err(_) => {
                                history_available = false;
                                data.report_days.retain(|d| *d != report_day);
                            }
                        }
                    }
                    if !mutation && history_available {
                        let focus = match api.focus_on(report_day, &data) {
                            Ok(totals) => crate::focus::FocusDay {
                                day: report_day,
                                totals,
                                error: None,
                            },
                            Err(e) => crate::focus::FocusDay {
                                day: report_day,
                                totals: vec![],
                                error: Some(e.to_string()),
                            },
                        };
                        data.focus_days.retain(|f| f.day != report_day);
                        data.focus_days.push(focus);
                    }
                    unindexed.retain(|(j, since)| {
                        !reflected(&data, j)
                            && since.elapsed() < std::time::Duration::from_secs(120)
                    });
                    for (pending, _) in &unindexed {
                        apply(&mut data, pending);
                    }
                    known = data;
                    if !known.warnings.is_empty() {
                        message = known.warnings.join(" · ");
                    }
                }
                Some(Err(e)) if mutation => {
                    message = format!("Saved to TickTick; list refresh failed: {e}")
                }
                Some(Err(e)) => {
                    let _ = out.send(Event::Error {
                        message: format!("Sync failed: {e}"),
                        mutation: false,
                    });
                    continue;
                }
            }
            if let Some(warning) = cache(&known) {
                message = format!("{message} · {warning}");
            }
            // The UI pops one pending job per event, so a merged burst answers once per keypress.
            let sent = (0..merged).all(|_| {
                let event = if mutation {
                    Event::Applied {
                        original: Box::new(original.clone()),
                        actual: Box::new(actual.clone()),
                        data: known.clone(),
                        message: message.clone(),
                    }
                } else {
                    Event::Data(known.clone(), message.clone())
                };
                out.send(event).is_ok()
            });
            if !sent {
                break;
            }
        }
    });
    (tx, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recurring_history_replaces_local_copy_and_repairs_cached_duplicates() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let remote: Task = serde_json::from_value(serde_json::json!({
            "id": "occurrence", "projectId": "finance", "title": "Pagar HRV",
            "status": 2, "dueDate": "2026-11-05T03:00:00.000+0000",
            "startDate": "2026-11-05T03:00:00.000+0000",
            "completedTime": "2026-11-05T08:15:46.914+0000"
        }))
        .unwrap();
        let mut local = remote.clone();
        local.id = "series".into();
        local
            .extra
            .insert("repeatFlag".into(), Value::from("RRULE:FREQ=MONTHLY"));
        local.extra.insert(
            "completedTime".into(),
            Value::from("2026-11-05T08:15:47.094889+00:00"),
        );
        let mut other = remote.clone();
        other.id = "another-real-occurrence".into();
        let mut data = Snapshot {
            time_zone: Some("America/Recife".into()),
            completed: vec![local.clone(), remote.clone(), other.clone()],
            ..Default::default()
        };
        merge_completed(&mut data, vec![remote.clone()], day);
        assert_eq!(data.completed.len(), 2);
        assert!(data.completed.iter().any(|t| t.id == other.id));
        assert!(!data.completed.iter().any(|t| t.id == local.id));
        merge_completed(&mut data, vec![remote.clone()], day);
        assert_eq!(data.completed.len(), 2);
        assert_eq!(data.report_days, vec![day]);

        // Similar titles alone must never merge unrelated completions.
        let mut unrelated = local.clone();
        unrelated.project_id = "another-list".into();
        assert!(!recurring_completion(&unrelated, &remote));
        unrelated = local.clone();
        unrelated.due_date = Some("2026-12-05T03:00:00.000+0000".into());
        assert!(!recurring_completion(&unrelated, &remote));
        unrelated = local;
        unrelated.extra.insert(
            "completedTime".into(),
            Value::from("2026-11-05T09:15:47+00:00"),
        );
        assert!(!recurring_completion(&unrelated, &remote));
    }

    #[test]
    fn completed_search_records_can_be_patched_and_reopened_without_losing_edits() {
        let mut data = Snapshot::default();
        let task = Task {
            id: "old".into(),
            project_id: "p".into(),
            title: "Old activity".into(),
            status: 2,
            ..Default::default()
        };
        remember(&mut data, &task);
        remember(&mut data, &task);
        assert_eq!(data.completed.len(), 1);
        let patch = Job::Patch {
            id: task.id.clone(),
            project: task.project_id.clone(),
            fields: Map::from_iter([("title".into(), Value::from("Edited activity"))]),
        };
        assert!(!reflected(&data, &patch));
        apply(&mut data, &patch);
        assert!(reflected(&data, &patch));
        let edited = data.completed[0].clone();
        apply(&mut data, &Job::Reopen(edited));
        assert!(data.completed.is_empty());
        assert_eq!(data.tasks.len(), 1);
        assert_eq!(data.tasks[0].title, "Edited activity");
        assert_eq!(data.tasks[0].status, 0);
    }

    #[test]
    fn completed_history_survives_cache_and_delete_does_not_reward() {
        let task = Task {
            id: "parent".into(),
            project_id: "p".into(),
            ..Default::default()
        };
        let child = Task {
            id: "child".into(),
            parent_id: Some(task.id.clone()),
            ..Default::default()
        };
        let original = Snapshot {
            tasks: vec![task.clone(), child],
            ..Default::default()
        };
        let mut deleted = original.clone();
        apply(&mut deleted, &Job::Delete(task.clone()));
        assert!(deleted.tasks.is_empty());
        assert!(deleted.completed.is_empty());
        let mut completed = original.clone();
        apply(&mut completed, &Job::Complete(task.clone()));
        assert!(completed.tasks.is_empty());
        assert_eq!(completed.completed.len(), 2);
        apply(&mut completed, &Job::Complete(task));
        assert_eq!(completed.completed.len(), 2);
        let restored: Snapshot =
            serde_json::from_slice(&serde_json::to_vec(&completed).unwrap()).unwrap();
        assert_eq!(restored.completed.len(), 2);
        assert!(
            restored
                .completed
                .iter()
                .all(|t| t.status == 2 && t.extra.contains_key("completedTime"))
        );
        // Failed optimistic writes are rolled back from the confirmed snapshot.
        assert!(original.completed.is_empty());
    }
    #[test]
    fn superseded_patch_is_not_replayed_over_newer_value() {
        let patch = |p: i32| Job::Patch {
            id: "t".into(),
            project: "p".into(),
            fields: Map::from_iter([("priority".into(), Value::from(p))]),
        };
        let mut unindexed = Vec::new();
        track(&mut unindexed, patch(1));
        track(&mut unindexed, patch(3));
        // Server already shows the final priority; the stale patch must not undo it.
        let mut data = Snapshot {
            tasks: vec![Task {
                id: "t".into(),
                priority: 3,
                ..Default::default()
            }],
            ..Default::default()
        };
        unindexed.retain(|(j, _)| !reflected(&data, j));
        for (j, _) in &unindexed {
            apply(&mut data, j);
        }
        assert_eq!(data.tasks[0].priority, 3);
    }
}
