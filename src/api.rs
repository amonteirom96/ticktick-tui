use anyhow::{Context, Result, bail};
use reqwest::{Method, blocking::Client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default)]
    pub color: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub sort_order: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub status: i32,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub title: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub items: Vec<ChecklistItem>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub status: i32,
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub groups: Vec<ProjectGroup>,
    #[serde(default)]
    pub inbox_id: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}
pub struct Api {
    client: Client,
    token: String,
    base: String,
}
impl Api {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(20)).build()?,
            token,
            base: "https://api.ticktick.com/open/v1".into(),
        })
    }
    fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().context("Cannot reach TickTick")?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "TickTick HTTP {}{}",
                status.as_u16(),
                if status.as_u16() == 401 {
                    ": run auth again"
                } else {
                    ""
                }
            );
        }
        let bytes = response.bytes()?;
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }
    pub fn validate(&self) -> Result<()> {
        let _: Vec<Project> =
            serde_json::from_value(self.request(Method::GET, "/project", None)?)?;
        Ok(())
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let projects: Vec<Project> =
            serde_json::from_value(self.request(Method::GET, "/project", None)?)?;
        let mut result = Snapshot {
            projects: projects.into_iter().filter(|p| !p.closed).collect(),
            ..Default::default()
        };
        result.groups = match self
            .request(Method::GET, "/project/group", None)
            .and_then(|v| Ok(serde_json::from_value(v)?))
        {
            Ok(groups) => groups,
            Err(_) => {
                result.warnings.push("Grupos indisponíveis na API".into());
                vec![]
            }
        };
        result.groups.sort_by_key(|g| g.sort_order);
        result.projects.sort_by_key(|p| p.sort_order);
        result.time_zone = self
            .request(Method::POST, "/preference", None)
            .ok()
            .and_then(|v| v["timeZone"].as_str().map(str::to_owned));
        for p in &result.projects {
            let data = self.request(Method::GET, &format!("/project/{}/data", p.id), None)?;
            let tasks: Vec<Task> =
                serde_json::from_value(data.get("tasks").cloned().unwrap_or(json!([])))?;
            result
                .tasks
                .extend(tasks.into_iter().filter(|t| t.status == 0));
        }
        // Inbox may be absent from /project. Resolve the alias through project data first.
        match self.request(Method::GET, "/project/inbox/data", None) {
            Ok(data) => {
                result.inbox_id = data["project"]["id"].as_str().map(str::to_owned);
                let tasks: Vec<Task> =
                    serde_json::from_value(data.get("tasks").cloned().unwrap_or(json!([])))?;
                if result.inbox_id.is_none() {
                    result.inbox_id = tasks.first().map(|t| t.project_id.clone());
                }
                result
                    .tasks
                    .extend(tasks.into_iter().filter(|t| t.status == 0));
            }
            Err(_) => match self
                .request(
                    Method::POST,
                    "/task/filter",
                    Some(json!({"projectIds":["inbox"],"status":[0]})),
                )
                .and_then(|v| Ok(serde_json::from_value::<Vec<Task>>(v)?))
            {
                Ok(tasks) => {
                    result.inbox_id = tasks.first().map(|t| t.project_id.clone());
                    if tasks.len() >= 200 {
                        result
                            .warnings
                            .push("Inbox: API limitou a leitura a 200 tarefas".into());
                    }
                    result
                        .tasks
                        .extend(tasks.into_iter().filter(|t| t.status == 0));
                }
                Err(_) => result
                    .warnings
                    .push("Inbox indisponível na API; use r para tentar novamente".into()),
            },
        }
        if result.inbox_id.is_none() {
            result.inbox_id = Some("inbox".into());
        }
        let mut seen = std::collections::HashSet::new();
        result.tasks.retain(|t| seen.insert(t.id.clone()));
        result.tasks.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        Ok(result)
    }
    pub fn create(&self, project: &str, title: &str, parent: Option<&str>) -> Result<Task> {
        let mut body = json!({"projectId": project,"title": title});
        if let Some(parent) = parent {
            body["parentId"] = json!(parent);
        }
        let value = self.request(Method::POST, "/task", Some(body))?;
        serde_json::from_value(value)
            .context("TickTick saved the task but returned no task object; refresh before retrying")
    }
    pub fn patch(
        &self,
        id: &str,
        project: &str,
        mut fields: serde_json::Map<String, Value>,
    ) -> Result<()> {
        fields.insert("id".into(), json!(id));
        fields.insert("projectId".into(), json!(project));
        self.request(
            Method::POST,
            &format!("/task/{id}"),
            Some(Value::Object(fields)),
        )?;
        Ok(())
    }
    pub fn move_task(&self, task: &Task, to: &str) -> Result<()> {
        self.request(
            Method::POST,
            "/task/move",
            Some(json!([{"fromProjectId": task.project_id, "toProjectId": to, "taskId": task.id}])),
        )?;
        Ok(())
    }
    pub fn complete(&self, task: &Task) -> Result<()> {
        self.request(
            Method::POST,
            &format!("/project/{}/task/{}/complete", task.project_id, task.id),
            None,
        )?;
        Ok(())
    }
    pub fn delete(&self, task: &Task) -> Result<()> {
        self.request(
            Method::DELETE,
            &format!("/project/{}/task/{}", task.project_id, task.id),
            None,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_roundtrip_preserves_checklists_and_recurrence() {
        let v = json!({"id":"t","projectId":"p","title":"Title","items":[{"id":"s","title":"Step","status":0}],"repeatFlag":"RRULE:FREQ=DAILY","timeZone":"America/Recife"});
        let mut t: Task = serde_json::from_value(v.clone()).unwrap();
        t.title = "Renamed".into();
        let out = serde_json::to_value(t).unwrap();
        assert_eq!(out["items"], v["items"]);
        assert_eq!(out["repeatFlag"], v["repeatFlag"]);
        assert_eq!(out["title"], "Renamed");
    }
    #[test]
    fn http_routes_auth_and_empty_success() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for expected in [
                "POST /open/v1/project/p/task/t/complete",
                "DELETE /open/v1/project/p/task/t",
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buf = [0; 8192];
                let n = socket.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                assert!(request.starts_with(expected));
                assert!(
                    request
                        .to_lowercase()
                        .contains("authorization: bearer test-token")
                );
                socket
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        });
        let mut api = Api::new("test-token".into()).unwrap();
        api.base = format!("http://{addr}/open/v1");
        let task = Task {
            id: "t".into(),
            project_id: "p".into(),
            ..Default::default()
        };
        api.complete(&task).unwrap();
        api.delete(&task).unwrap();
        server.join().unwrap();
    }
}
