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
    #[serde(default)]
    pub sort_order: i64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
impl Task {
    pub fn is_note(&self) -> bool {
        self.extra.get("kind").and_then(Value::as_str) == Some("NOTE")
    }

    pub fn marker(&self) -> &'static str {
        if self.is_note() { "≡" } else { "○" }
    }
}

impl Snapshot {
    /// Resolve ancestors across active tasks and completed history, tolerating cycles.
    pub fn ancestry(&self, task: &Task) -> String {
        let mut seen = std::collections::HashSet::from([task.id.as_str()]);
        let mut next = task.parent_id.as_deref();
        let mut titles = Vec::new();
        while let Some(id) = next.filter(|id| !id.is_empty()) {
            if !seen.insert(id) {
                break;
            }
            let Some(parent) = self
                .tasks
                .iter()
                .chain(self.completed.iter())
                .find(|t| t.id == id)
            else {
                break;
            };
            titles.push(parent.title.replace(['\n', '\r'], " "));
            next = parent.parent_id.as_deref();
        }
        titles.reverse();
        titles.join(" › ")
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub habit_checkins: Vec<Value>,
    #[serde(default)]
    pub habit_checkin_error: Option<String>,
    #[serde(default)]
    pub focus_records: Vec<Value>,
    #[serde(default)]
    pub wellness_error: Option<String>,
    #[serde(default)]
    pub habits: Vec<Value>,
    #[serde(default)]
    pub habit_error: Option<String>,
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
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub completed: Vec<Task>,
    #[serde(default)]
    pub report_days: Vec<chrono::NaiveDate>,
    #[serde(default)]
    pub focus_days: Vec<crate::focus::FocusDay>,
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
        let mut attempt = 0;
        let response = loop {
            let mut req = self
                .client
                .request(method.clone(), format!("{}{path}", self.base))
                .bearer_auth(&self.token);
            if let Some(body) = &body {
                req = req.json(body);
            }
            let response = req.send().context("Cannot reach TickTick")?;
            // Rate limited: wait (Retry-After or exponential) and retry a few times.
            if response.status().as_u16() != 429 || attempt == 3 {
                break response;
            }
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()?.parse().ok())
                .unwrap_or(2u64 << attempt);
            std::thread::sleep(Duration::from_secs(wait.min(30)));
            attempt += 1;
        };
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
        let focus_result = (|| -> Result<Vec<Value>> {
            let (from, to) = crate::focus::bounds(crate::sidebar::today(&result), &result)?;
            let mut records = Vec::new();
            for kind in [0, 1] {
                let mut url = reqwest::Url::parse("https://api.ticktick.com/open/v1/focus")?;
                url.query_pairs_mut()
                    .append_pair("from", &from.format("%Y-%m-%dT%H:%M:%S%z").to_string())
                    .append_pair("to", &to.format("%Y-%m-%dT%H:%M:%S%z").to_string())
                    .append_pair("type", &kind.to_string());
                let value = self.request(
                    Method::GET,
                    &format!("/focus?{}", url.query().unwrap()),
                    None,
                )?;
                let rows = value.as_array().context("Resposta de foco inválida")?;
                for row in rows {
                    let mut row = row.clone();
                    row.as_object_mut()
                        .context("Registro de foco inválido")?
                        .insert("type".into(), json!(kind));
                    records.push(row);
                }
            }
            Ok(records)
        })();
        match focus_result {
            Ok(records) => result.focus_records = records,
            Err(e) => result.wellness_error = Some(format!("Histórico de foco indisponível: {e}")),
        }
        match self
            .request(Method::GET, "/habit", None)
            .and_then(|v| Ok(serde_json::from_value::<Vec<Value>>(v)?))
        {
            Ok(mut habits) => {
                habits.sort_by_key(|h| h["sortOrder"].as_i64().unwrap_or(0));
                result.habits = habits;
                match self.habit_checkins(&result) {
                    Ok(checkins) => result.habit_checkins = checkins,
                    Err(e) => result.habit_checkin_error = Some(e.to_string()),
                }
            }
            Err(e) => result.habit_error = Some(format!("Hábitos indisponíveis: {e}")),
        }
        Ok(result)
    }
    pub fn habit_checkins(&self, data: &Snapshot) -> Result<Vec<Value>> {
        let ids = data
            .habits
            .iter()
            .filter_map(|h| h["id"].as_str())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let stamp = crate::sidebar::today(data).format("%Y%m%d").to_string();
        let mut records = Vec::new();
        for chunk in ids.chunks(50) {
            let mut url = reqwest::Url::parse("https://api.ticktick.com/open/v1/habit/checkins")?;
            url.query_pairs_mut()
                .append_pair("habitIds", &chunk.join(","))
                .append_pair("from", &stamp)
                .append_pair("to", &stamp);
            let value = self.request(
                Method::GET,
                &format!("/habit/checkins?{}", url.query().unwrap()),
                None,
            )?;
            records.extend(
                value
                    .as_array()
                    .context("Resposta de check-in inválida")?
                    .iter()
                    .cloned(),
            );
        }
        Ok(records)
    }
    /// Upserts the day's check-in; `value` 0 undoes a completion.
    pub fn check_habit(
        &self,
        id: &str,
        day: chrono::NaiveDate,
        goal: f64,
        value: f64,
    ) -> Result<()> {
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "ID de hábito inválido"
        );
        anyhow::ensure!(goal.is_finite() && goal > 0.0, "Meta de hábito inválida");
        anyhow::ensure!(
            value.is_finite() && value >= 0.0,
            "Valor de hábito inválido"
        );
        let stamp = day.format("%Y%m%d").to_string().parse::<i32>()?;
        let time = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%z").to_string();
        self.request(
            Method::POST,
            &format!("/habit/{id}/checkin"),
            Some(json!({
                "stamp": stamp, "value": value, "goal": goal, "time": time, "opTime": time
            })),
        )?;
        Ok(())
    }
    /// Optional endpoints are not available for every Open API account.
    pub fn profile_name(&self) -> Option<String> {
        let v = self.request(Method::GET, "/user/info", None).ok()?;
        ["displayName", "name", "username"]
            .into_iter()
            .find_map(|key| {
                v[key]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
            })
    }
    pub fn completed_on(&self, day: chrono::NaiveDate, data: &Snapshot) -> Result<Vec<Task>> {
        let next = day.succ_opt().context("Date out of range")?;
        let mut tasks = Vec::new();
        use chrono::TimeZone;
        let boundary = |date: chrono::NaiveDate| -> Result<String> {
            let midnight = date.and_hms_opt(0, 0, 0).context("Invalid midnight")?;
            if let Some(tz) = data
                .time_zone
                .as_deref()
                .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
            {
                Ok(tz
                    .from_local_datetime(&midnight)
                    .earliest()
                    .context("Invalid local date")?
                    .to_rfc3339())
            } else {
                Ok(chrono::Local
                    .from_local_datetime(&midnight)
                    .earliest()
                    .context("Invalid local date")?
                    .to_rfc3339())
            }
        };
        // Treat full pages conservatively; account-specific server caps can be smaller than requested.
        let value = self.request(
            Method::POST,
            "/task/completed",
            Some(json!({
                "startDate": boundary(day)?, "endDate": boundary(next)?
            })),
        )?;
        let rows = value
            .as_array()
            .or_else(|| value.get("tasks").and_then(Value::as_array))
            .context("Unexpected completed-task response")?;
        if rows.len() >= 200 {
            bail!("Completed-task history may be truncated");
        }
        for row in rows {
            let task: Task = serde_json::from_value(row.clone())?;
            if task.status != 2 {
                bail!("Unexpected status in completed-task history");
            }
            let completed_day = task
                .extra
                .get("completedTime")
                .and_then(Value::as_str)
                .and_then(|v| crate::sidebar::date(v, &Task::default(), data))
                .context("Missing or invalid completion time")?;
            if completed_day == day {
                tasks.push(task);
            }
        }
        Ok(tasks)
    }
    pub fn focus_on(
        &self,
        day: chrono::NaiveDate,
        data: &Snapshot,
    ) -> Result<Vec<crate::focus::FocusTotal>> {
        let (from, to) = crate::focus::bounds(day, data)?;
        let mut records = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for kind in [0, 1] {
            let mut url = reqwest::Url::parse("https://api.ticktick.com/open/v1/focus")?;
            url.query_pairs_mut()
                .append_pair("from", &(from - chrono::Duration::days(1)).to_rfc3339())
                .append_pair("to", &to.to_rfc3339())
                .append_pair("type", &kind.to_string());
            let value = self.request(
                Method::GET,
                &format!("/focus?{}", url.query().unwrap()),
                None,
            )?;
            let rows = value.as_array().context("Unexpected focus response")?;
            if rows.len() >= 200 {
                bail!("Focus history may be truncated");
            }
            for row in rows {
                let id = row["id"].as_str().context("Missing focus record id")?;
                if seen.insert(id.to_owned()) {
                    records.push(row.clone());
                }
            }
        }
        crate::focus::summarize(day, data, &records)
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
        if task.is_note() {
            bail!("Anotações não podem ser concluídas");
        }
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
    fn ancestry_resolves_active_and_completed_parents_and_cycles() {
        let mut data = Snapshot::default();
        let root = Task {
            id: "root".into(),
            title: "FEAT - Buser Voos".into(),
            ..Default::default()
        };
        let parent = Task {
            id: "parent".into(),
            title: "Tela de viagem".into(),
            parent_id: Some("root".into()),
            ..Default::default()
        };
        let child = Task {
            id: "child".into(),
            parent_id: Some("parent".into()),
            ..Default::default()
        };
        data.tasks.push(root);
        data.completed.push(parent);
        assert_eq!(data.ancestry(&child), "FEAT - Buser Voos › Tela de viagem");
        data.tasks[0].parent_id = Some("parent".into());
        assert_eq!(data.ancestry(&child), "FEAT - Buser Voos › Tela de viagem");
        data.completed.clear();
        assert!(data.ancestry(&child).is_empty());
    }

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
    fn report_api_checks_timezone_and_rejects_incomplete_history() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let row = json!({"id":"t", "projectId":"p", "title":"Finished", "status":2, "completedTime":"2026-11-05T01:00:00Z"});
            let replies = [
                json!({"name":"Alex"}),
                json!([row.clone()]),
                json!([{"id":"t", "projectId":"p", "title":"Missing time", "status":2}]),
                Value::Array(vec![row; 200]),
            ];
            for (i, reply) in replies.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&request);
                if i == 0 {
                    assert!(text.starts_with("GET /open/v1/user/info"));
                } else {
                    assert!(text.starts_with("POST /open/v1/task/completed"));
                    let body: Value =
                        serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                    assert_eq!(body["startDate"], "2026-11-04T00:00:00-03:00");
                    assert_eq!(body["endDate"], "2026-11-05T00:00:00-03:00");
                    assert!(body.get("from").is_none());
                    assert!(body.get("to").is_none());
                }
                let body = reply.to_string();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let mut api = Api::new("test".into()).unwrap();
        api.base = format!("http://{addr}/open/v1");
        assert_eq!(api.profile_name().as_deref(), Some("Alex"));
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let data = Snapshot {
            time_zone: Some("America/Recife".into()),
            ..Default::default()
        };
        assert_eq!(api.completed_on(day, &data).unwrap().len(), 1);
        assert!(api.completed_on(day, &data).is_err());
        assert!(api.completed_on(day, &data).is_err());
        server.join().unwrap();
    }
    #[test]
    fn focus_reads_both_timer_types_and_deduplicates_sessions() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for kind in [0, 1] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&bytes);
                let path = text.split_whitespace().nth(1).unwrap();
                let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                assert_eq!(url.path(), "/open/v1/focus");
                let query = url
                    .query_pairs()
                    .into_owned()
                    .collect::<std::collections::HashMap<_, _>>();
                assert_eq!(query["type"], kind.to_string());
                assert_eq!(query["from"], "2026-11-02T03:00:00+00:00");
                assert_eq!(query["to"], "2026-11-04T03:00:00+00:00");
                let body=json!([{"id":"f","status":1,"duration":3600000,"startTime":"2026-11-03T12:00:00Z","endTime":"2026-11-03T13:00:00Z","tasks":[]}]).to_string();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let mut api = Api::new("test".into()).unwrap();
        api.base = format!("http://{addr}/open/v1");
        let data = Snapshot {
            time_zone: Some("America/Recife".into()),
            ..Default::default()
        };
        let totals = api
            .focus_on(chrono::NaiveDate::from_ymd_opt(2026, 11, 3).unwrap(), &data)
            .unwrap();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].seconds, 3600);
        server.join().unwrap();
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
