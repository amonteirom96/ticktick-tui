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
    /// `value` equal to `goal` completes the habit; 0 undoes it.
    CheckHabit { id: String, day: chrono::NaiveDate, goal: f64, value: f64 },
    Report(chrono::NaiveDate),
    Create(Task),
    Patch {
        id: String,
        project: String,
        fields: Map<String, Value>,
    },
    Complete(Task),
    Delete(Task),
    Move {
        task: Task,
        to: String,
    },
}
pub enum Event {
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
/// The same operation is used for optimistic rendering, commit and rollback replay.
pub fn apply(data: &mut Snapshot, job: &Job) {
    match job {
        Job::Refresh | Job::Report(_) | Job::CheckHabit { .. } => (),
        Job::Create(task) => {
            if !data.tasks.iter().any(|t| t.id == task.id) {
                data.tasks.push(task.clone());
            }
        }
        Job::Patch { id, fields, .. } => {
            if let Some(t) = data.tasks.iter_mut().find(|t| &t.id == id) {
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
        Job::Create(t) | Job::Complete(t) | Job::Delete(t) | Job::Move { task: t, .. } => {
            map_task(t)
        }
        Job::Patch { id, fields, .. } => {
            if id == old {
                *id = new.into();
            }
            if fields.get("parentId").and_then(Value::as_str) == Some(old) {
                fields.insert("parentId".into(), Value::String(new.into()));
            }
        }
        Job::Refresh | Job::Report(_) | Job::CheckHabit { .. } => (),
    }
}
fn reflected(data: &Snapshot, job: &Job) -> bool {
    match job {
        Job::Create(t) => data.tasks.iter().any(|x| x.id == t.id),
        Job::Complete(t) | Job::Delete(t) => !data.tasks.iter().any(|x| x.id == t.id),
        Job::Move { task, to } => data
            .tasks
            .iter()
            .any(|x| x.id == task.id && &x.project_id == to),
        Job::Patch { id, fields, .. } => data.tasks.iter().find(|t| &t.id == id).is_some_and(|t| {
            let v = serde_json::to_value(t).unwrap();
            fields.iter().all(|(k, x)| v.get(k) == Some(x))
        }),
        Job::Refresh | Job::Report(_) | Job::CheckHabit { .. } => true,
    }
}
fn cache(data: &Snapshot) -> Option<String> {
    serde_json::to_vec(data)
        .map_err(anyhow::Error::from)
        .and_then(|v| config::private_write(&config::cache_path(), &v))
        .err()
        .map(|_| "Não foi possível atualizar o cache local".into())
}
fn fetch_report(api: &Api, data: &mut Snapshot, day: chrono::NaiveDate) -> String {
    let message = match api.completed_on(day, data) {
        Ok(tasks) => {
            for task in tasks {
                let existing = data.completed.iter().position(|t| {
                    t.id == task.id
                        && t.extra
                            .get("completedTime")
                            .and_then(Value::as_str)
                            .and_then(|v| crate::sidebar::date(v, &Task::default(), data))
                            == Some(day)
                });
                if let Some(i) = existing {
                    data.completed[i] = task;
                } else {
                    data.completed.push(task);
                }
            }
            if !data.report_days.contains(&day) {
                data.report_days.push(day);
            }
            format!(
                "Relatório de {} atualizado · Sincronizado com TickTick",
                day.format("%d/%m/%Y")
            )
        }
        Err(e) => {
            data.report_days.retain(|d| *d != day);
            format!("Histórico indisponível: {e}. Exibindo registros locais.")
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
        while let Ok(original) = rx.recv() {
            let mut job = original.clone();
            for (old, new) in &aliases {
                remap(&mut job, old, new);
            }
            if let Job::CheckHabit { ref id, day, goal, value } = job {
                match api.check_habit(id, day, goal, value) {
                    Ok(()) => {
                        crate::wellness::record_checkin(&mut known, id, day, goal, value);
                        let mut message = if value > 0.0 { "Hábito concluído · salvo no TickTick" } else { "Hábito desmarcado · salvo no TickTick" }.to_string();
                        if let Some(warning) = cache(&known) { message.push_str(&format!(" · {warning}")); }
                        if out.send(Event::Data(known.clone(), message)).is_err() { break; }
                    }
                    Err(error) => {
                        let _ = out.send(Event::Error { message: format!("Não foi possível {} o hábito: {error}. Use r para conferir antes de tentar novamente.", if value > 0.0 { "concluir" } else { "desmarcar" }), mutation: false });
                    }
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
            let mutation = !matches!(job, Job::Refresh | Job::Report(_) | Job::CheckHabit { .. });
            let unresolved = match &job {
                Job::Create(t) => t
                    .parent_id
                    .as_deref()
                    .is_some_and(|s| s.starts_with("local:")),
                Job::Patch { id, .. } => id.starts_with("local:"),
                Job::Complete(t) | Job::Delete(t) | Job::Move { task: t, .. } => {
                    t.id.starts_with("local:")
                }
                Job::Refresh | Job::Report(_) | Job::CheckHabit { .. } => false,
            };
            let result = if unresolved {
                Err(anyhow::anyhow!("A criação da tarefa anterior falhou"))
            } else {
                match &job {
                    Job::Refresh | Job::Report(_) | Job::CheckHabit { .. } => Ok(job.clone()),
                    Job::Create(t) => api
                        .create(&t.project_id, &t.title, t.parent_id.as_deref())
                        .map(|remote| {
                            aliases.insert(t.id.clone(), remote.id.clone());
                            Job::Create(remote)
                        }),
                    Job::Patch {
                        id,
                        project,
                        fields,
                    } => api.patch(id, project, fields.clone()).map(|_| job.clone()),
                    Job::Complete(t) => api.complete(t).map(|_| job.clone()),
                    Job::Delete(t) => api.delete(t).map(|_| job.clone()),
                    Job::Move { task, to } => api.move_task(task, to).map(|_| job.clone()),
                }
            };
            let actual = match result {
                Ok(job) => job,
                Err(e) => {
                    let _=out.send(Event::Error{message:format!("Falha na gravação: {e}. Alteração local desfeita; use r para conferir antes de tentar novamente."),mutation});
                    continue;
                }
            };
            if mutation {
                apply(&mut known, &actual);
                unindexed.push((actual.clone(), std::time::Instant::now()));
            }
            let mut message = String::from("Sincronizado com TickTick");
            match api.snapshot() {
                Ok(mut data) => {
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
                                for task in tasks {
                                    let day = task
                                        .extra
                                        .get("completedTime")
                                        .and_then(Value::as_str)
                                        .and_then(|v| {
                                            crate::sidebar::date(v, &Task::default(), &data)
                                        });
                                    let existing = data.completed.iter().position(|t| {
                                        t.id == task.id
                                            && t.extra
                                                .get("completedTime")
                                                .and_then(Value::as_str)
                                                .and_then(|v| {
                                                    crate::sidebar::date(v, &Task::default(), &data)
                                                })
                                                == day
                                    });
                                    if let Some(i) = existing {
                                        data.completed[i] = task;
                                    } else {
                                        data.completed.push(task);
                                    }
                                }
                                if !data.report_days.contains(&report_day) {
                                    data.report_days.push(report_day);
                                }
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
                Err(e) if mutation => {
                    message = format!("Salvo no TickTick; atualização da lista falhou: {e}")
                }
                Err(e) => {
                    let _ = out.send(Event::Error {
                        message: format!("Falha na sincronização: {e}"),
                        mutation: false,
                    });
                    continue;
                }
            }
            if let Some(warning) = cache(&known) {
                message = format!("{message} · {warning}");
            }
            let event = if mutation {
                Event::Applied {
                    original: Box::new(original),
                    actual: Box::new(actual),
                    data: known.clone(),
                    message,
                }
            } else {
                Event::Data(known.clone(), message)
            };
            if out.send(event).is_err() {
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
}
