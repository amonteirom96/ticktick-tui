use crate::api::Snapshot;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct FocusDay {
    pub day: NaiveDate,
    pub totals: Vec<FocusTotal>,
    pub error: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct FocusTotal {
    pub project_id: Option<String>,
    pub label: String,
    pub seconds: u64,
}
pub fn bounds(day: NaiveDate, data: &Snapshot) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let boundary = |date: NaiveDate| -> Result<DateTime<Utc>> {
        let midnight = date.and_hms_opt(0, 0, 0).context("Invalid midnight")?;
        let value = if let Some(tz) = data
            .time_zone
            .as_deref()
            .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
        {
            tz.from_local_datetime(&midnight)
                .earliest()
                .map(|t| t.with_timezone(&Utc))
        } else {
            chrono::Local
                .from_local_datetime(&midnight)
                .earliest()
                .map(|t| t.with_timezone(&Utc))
        };
        value.context("Invalid local date")
    };
    Ok((
        boundary(day)?,
        boundary(day.succ_opt().context("Date out of range")?)?,
    ))
}
fn timestamp(v: &Value, key: &str) -> Result<DateTime<Utc>> {
    let text = v[key].as_str().context("Missing focus timestamp")?;
    Ok(DateTime::parse_from_rfc3339(text)
        .or_else(|_| DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z"))?
        .with_timezone(&Utc))
}
/// Normalize the public API's seconds to the milliseconds used by cached history.
/// Older responses contain milliseconds; distinguish them by the wall interval.
pub fn normalize_record(mut record: Value) -> Value {
    if let (Ok(start), Ok(end), Some(duration)) = (
        timestamp(&record, "startTime"),
        timestamp(&record, "endTime"),
        record["duration"].as_u64(),
    ) {
        let wall = (end - start).num_seconds().max(0) as u64;
        if duration <= wall {
            record["duration"] = duration.saturating_mul(1000).into();
        }
    }
    if record["status"].as_i64() == Some(2) && record["endTime"].is_string() {
        record["status"] = 1.into();
    }
    record
}

/// Focus duration is returned in milliseconds; relation intervals split a session between tasks.
pub fn summarize(day: NaiveDate, data: &Snapshot, records: &[Value]) -> Result<Vec<FocusTotal>> {
    let (from, to) = bounds(day, data)?;
    let mut totals = BTreeMap::<String, FocusTotal>::new();
    for record in records {
        if record["status"].as_i64().is_some_and(|s| s != 1) {
            continue;
        }
        let start = timestamp(record, "startTime")?;
        let end = timestamp(record, "endTime")?;
        let wall = (end - start).num_milliseconds();
        if wall <= 0 || end <= from || start >= to {
            continue;
        }
        let duration = record["duration"]
            .as_u64()
            .context("Missing focus duration")?;
        let relations = record["tasks"].as_array().filter(|r| !r.is_empty());
        let free = serde_json::json!({"taskId": record["taskId"], "startTime":record["startTime"],"endTime":record["endTime"]});
        let segments: Vec<&Value> = relations
            .map(|r| r.iter().collect())
            .unwrap_or_else(|| vec![&free]);
        for relation in segments {
            let begin = timestamp(relation, "startTime")?.max(start).max(from);
            let finish = timestamp(relation, "endTime")?.min(end).min(to);
            let overlap = (finish - begin).num_milliseconds();
            if overlap <= 0 {
                continue;
            }
            let task_id = relation["taskId"].as_str();
            let task = task_id.and_then(|id| {
                data.tasks
                    .iter()
                    .chain(&data.completed)
                    .find(|t| t.id == id)
            });
            let project_id = relation["projectId"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| task.map(|t| t.project_id.clone()));
            let label = project_id
                .as_ref()
                .and_then(|id| {
                    data.projects
                        .iter()
                        .find(|p| &p.id == id)
                        .map(|p| p.name.clone())
                })
                .unwrap_or_else(|| {
                    if project_id
                        .as_ref()
                        .is_some_and(|id| data.inbox_id.as_ref() == Some(id))
                    {
                        "Inbox"
                    } else if task_id.is_some() {
                        "Lista não identificada"
                    } else {
                        "Foco livre"
                    }
                    .into()
                });
            let key = project_id.clone().unwrap_or_else(|| label.clone());
            let entry = totals.entry(key).or_insert_with(|| FocusTotal {
                project_id,
                label,
                seconds: 0,
            });
            // Prorate the recorded duration (which excludes pauses) across interval/day boundaries.
            entry.seconds += ((duration as u128 * overlap as u128 / wall as u128) / 1000) as u64;
        }
    }
    let mut values = totals
        .into_values()
        .filter(|v| v.seconds > 0)
        .collect::<Vec<_>>();
    values.sort_by(|a, b| b.seconds.cmp(&a.seconds).then(a.label.cmp(&b.label)));
    Ok(values)
}
pub fn duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    if seconds == 0 {
        "0min".into()
    } else if minutes == 0 {
        "<1min".into()
    } else if minutes < 60 {
        format!("{minutes}min")
    } else if minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{}h {:02}min", minutes / 60, minutes % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Project, Task};
    use serde_json::json;
    #[test]
    fn normalizes_public_seconds_legacy_milliseconds_and_top_level_task() {
        let record = json!({"status":2,"taskId":"t","duration":1500,"startTime":"2026-11-03T09:00:00Z","endTime":"2026-11-03T09:25:00Z"});
        let normalized = normalize_record(record);
        assert_eq!(normalized["duration"], 1500000);
        assert_eq!(normalize_record(normalized.clone())["duration"], 1500000);
        let data = Snapshot {
            time_zone: Some("UTC".into()),
            tasks: vec![crate::api::Task {
                id: "t".into(),
                project_id: "p".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let totals = summarize(
            NaiveDate::from_ymd_opt(2026, 11, 3).unwrap(),
            &data,
            &[normalized],
        )
        .unwrap();
        assert_eq!(totals[0].seconds, 1500);
        assert_eq!(totals[0].project_id.as_deref(), Some("p"));
    }

    #[test]
    fn focus_uses_real_milliseconds_and_splits_tasks_and_local_days() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let data = Snapshot {
            time_zone: Some("America/Recife".into()),
            projects: vec![Project {
                id: "p".into(),
                name: "Expin".into(),
                ..Default::default()
            }],
            tasks: vec![Task {
                id: "t".into(),
                project_id: "p".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let records = vec![
            json!({"id":"a","status":1,"duration":3600000,"startTime":"2026-11-03T12:00:00-03:00","endTime":"2026-11-03T13:00:00-03:00","tasks":[{"taskId":"t","startTime":"2026-11-03T12:00:00-03:00","endTime":"2026-11-03T12:30:00-03:00"},{"startTime":"2026-11-03T12:30:00-03:00","endTime":"2026-11-03T13:00:00-03:00"}]}),
            json!({"id":"b","status":1,"duration":7200000,"startTime":"2026-11-03T23:00:00-03:00","endTime":"2026-11-04T01:00:00-03:00","tasks":[{"taskId":"t","startTime":"2026-11-03T23:00:00-03:00","endTime":"2026-11-04T01:00:00-03:00"}]}),
        ];
        let totals = summarize(day, &data, &records).unwrap();
        assert_eq!(totals[0].label, "Expin");
        assert_eq!(totals[0].seconds, 5400);
        assert_eq!(totals[1].label, "Foco livre");
        assert_eq!(totals[1].seconds, 1800);
        let next = summarize(day.succ_opt().unwrap(), &data, &records).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].seconds, 3600);
        assert_eq!(duration(5400), "1h 30min");
        assert_eq!(duration(216000), "60h");
        assert_eq!(duration(60), "1min");
        assert_eq!(duration(1), "<1min");
        assert!(
            summarize(day.pred_opt().unwrap(), &data, &records)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn focus_preserves_unresolved_tasks_and_does_not_count_paused_time_as_work() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let data = Snapshot {
            time_zone: Some("UTC".into()),
            ..Default::default()
        };
        let row = json!({"status":1,"duration":1800000,"pauseDuration":1800000,"startTime":"2026-11-03T12:00:00Z","endTime":"2026-11-03T13:00:00Z","tasks":[{"taskId":"unknown","startTime":"2026-11-03T12:00:00Z","endTime":"2026-11-03T13:00:00Z"}]});
        let totals = summarize(day, &data, &[row]).unwrap();
        assert_eq!(totals[0].seconds, 1800);
        assert_eq!(totals[0].label, "Lista não identificada");
        assert!(summarize(day, &data, &[json!({"duration":1000})]).is_err());
    }
}
