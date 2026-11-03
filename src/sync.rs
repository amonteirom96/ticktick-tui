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
    Create(Task),
    Patch {
        id: String,
        project: String,
        fields: Map<String, Value>,
    },
    Complete(Task),
    Delete(Task),
}
pub enum Event {
    Data(Snapshot, String),
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
        Job::Refresh => (),
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
        Job::Create(t) | Job::Complete(t) | Job::Delete(t) => map_task(t),
        Job::Patch { id, fields, .. } => {
            if id == old {
                *id = new.into();
            }
            if fields.get("parentId").and_then(Value::as_str) == Some(old) {
                fields.insert("parentId".into(), Value::String(new.into()));
            }
        }
        Job::Refresh => (),
    }
}
fn reflected(data: &Snapshot, job: &Job) -> bool {
    match job {
        Job::Create(t) => data.tasks.iter().any(|x| x.id == t.id),
        Job::Complete(t) | Job::Delete(t) => !data.tasks.iter().any(|x| x.id == t.id),
        Job::Patch { id, fields, .. } => data.tasks.iter().find(|t| &t.id == id).is_some_and(|t| {
            let v = serde_json::to_value(t).unwrap();
            fields.iter().all(|(k, x)| v.get(k) == Some(x))
        }),
        Job::Refresh => true,
    }
}
fn cache(data: &Snapshot) -> Option<String> {
    serde_json::to_vec(data)
        .map_err(anyhow::Error::from)
        .and_then(|v| config::private_write(&config::cache_path(), &v))
        .err()
        .map(|_| "Não foi possível atualizar o cache local".into())
}
pub fn worker(api: Api, initial: Snapshot) -> (Sender<Job>, Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Job>();
    let (out, events) = mpsc::channel();
    std::thread::spawn(move || {
        let mut aliases = HashMap::<String, String>::new();
        let mut known = initial;
        // Keep confirmed writes visible while TickTick's list index catches up.
        let mut unindexed = Vec::<(Job, std::time::Instant)>::new();
        while let Ok(original) = rx.recv() {
            let mut job = original.clone();
            for (old, new) in &aliases {
                remap(&mut job, old, new);
            }
            let mutation = !matches!(job, Job::Refresh);
            let unresolved = match &job {
                Job::Create(t) => t
                    .parent_id
                    .as_deref()
                    .is_some_and(|s| s.starts_with("local:")),
                Job::Patch { id, .. } => id.starts_with("local:"),
                Job::Complete(t) | Job::Delete(t) => t.id.starts_with("local:"),
                Job::Refresh => false,
            };
            let result = if unresolved {
                Err(anyhow::anyhow!("A criação da tarefa anterior falhou"))
            } else {
                match &job {
                    Job::Refresh => Ok(job.clone()),
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
