//! Text formats for the due-date and reminder prompts.
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};

const API_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%z";

/// Parses `hoje`, `amanhã`, `+3`, `25/12`, `25/12/2026` or `2026-12-25`, optionally followed by `HH:MM`.
fn parse_due(input: &str, today: NaiveDate) -> Result<(NaiveDate, Option<NaiveTime>), String> {
    let mut parts = input.split_whitespace();
    let day = parts.next().unwrap_or_default().to_lowercase();
    let time = parts
        .next()
        .map(|t| NaiveTime::parse_from_str(t, "%H:%M").map_err(|_| format!("Hora inválida: {t}")))
        .transpose()?;
    let date = match day.as_str() {
        "hoje" => Some(today),
        "amanhã" | "amanha" => Some(today + Duration::days(1)),
        d if d.starts_with('+') => d[1..].parse().ok().map(|n| today + Duration::days(n)),
        d => NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .or_else(|_| NaiveDate::parse_from_str(d, "%d/%m/%Y"))
            .or_else(|_| NaiveDate::parse_from_str(&format!("{d}/{}", today.format("%Y")), "%d/%m/%Y"))
            .ok(),
    };
    match (date, parts.next()) {
        (Some(date), None) => Ok((date, time)),
        _ => Err(format!("Data inválida: {input}")),
    }
}

fn to_utc(local: NaiveDateTime, tz: Option<Tz>) -> Option<DateTime<Utc>> {
    match tz {
        Some(tz) => tz.from_local_datetime(&local).earliest().map(|d| d.to_utc()),
        None => Local.from_local_datetime(&local).earliest().map(|d| d.to_utc()),
    }
}

/// Fields for `POST /task/{id}`. Empty input clears the date.
pub fn due_fields(input: &str, today: NaiveDate, tz: Option<Tz>) -> Result<Map<String, Value>, String> {
    let mut fields = Map::new();
    if input.trim().is_empty() {
        fields.insert("dueDate".into(), Value::Null);
        fields.insert("startDate".into(), Value::Null);
        return Ok(fields);
    }
    let (date, time) = parse_due(input, today)?;
    let utc = to_utc(date.and_time(time.unwrap_or_default()), tz).ok_or("Horário inexistente nesse fuso")?;
    let value = json!(utc.format(API_FORMAT).to_string());
    fields.insert("dueDate".into(), value.clone());
    fields.insert("startDate".into(), value);
    fields.insert("isAllDay".into(), json!(time.is_none()));
    if let Some(tz) = tz {
        fields.insert("timeZone".into(), json!(tz.name()));
    }
    Ok(fields)
}

/// Inverse of `due_fields`, used to prefill the prompt.
pub fn format_due(due: &str, all_day: bool, tz: Option<Tz>) -> String {
    let Ok(parsed) = DateTime::parse_from_str(due, "%Y-%m-%dT%H:%M:%S%.f%z") else {
        return String::new();
    };
    let local = match tz {
        Some(tz) => parsed.with_timezone(&tz).naive_local(),
        None => parsed.with_timezone(&Local).naive_local(),
    };
    local
        .format(if all_day { "%d/%m/%Y" } else { "%d/%m/%Y %H:%M" })
        .to_string()
}

/// Short label for the task list: `hoje 14:30`, `amanhã`, `sex`, `25/12`. `true` when overdue.
pub fn relative_due(due: &str, all_day: bool, tz: Option<Tz>, today: NaiveDate) -> Option<(String, bool)> {
    let parsed = DateTime::parse_from_str(due, "%Y-%m-%dT%H:%M:%S%.f%z").ok()?;
    let local = match tz {
        Some(tz) => parsed.with_timezone(&tz).naive_local(),
        None => parsed.with_timezone(&Local).naive_local(),
    };
    let days = (local.date() - today).num_days();
    let day = match days {
        0 => "hoje".to_owned(),
        1 => "amanhã".to_owned(),
        2..=6 => ["seg", "ter", "qua", "qui", "sex", "sáb", "dom"]
            [local.date().weekday().num_days_from_monday() as usize]
            .to_owned(),
        _ if local.year() == today.year() => local.format("%d/%m").to_string(),
        _ => local.format("%d/%m/%y").to_string(),
    };
    let label = if all_day { day } else { format!("{day} {}", local.format("%H:%M")) };
    Some((label, days < 0))
}

/// `0, 15m, 1h, 2d` → minutes/hours/days before the due time. Raw `TRIGGER:` values pass through.
pub fn parse_reminders(input: &str) -> Result<Vec<String>, String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with("TRIGGER:") {
                return Ok(s.to_owned());
            }
            if s == "0" {
                return Ok("TRIGGER:PT0S".into());
            }
            let unit = |suffix, fmt: fn(u32) -> String| {
                s.strip_suffix(suffix).and_then(|n| n.parse().ok()).map(fmt)
            };
            unit('m', |n| format!("TRIGGER:-PT{n}M"))
                .or_else(|| unit('h', |n| format!("TRIGGER:-PT{n}H")))
                .or_else(|| unit('d', |n| format!("TRIGGER:-P{n}D")))
                .ok_or(format!("Lembrete inválido: {s} (use 0, 15m, 1h ou 1d)"))
        })
        .collect()
}

pub fn format_reminders(reminders: &[String]) -> String {
    reminders
        .iter()
        .map(|r| {
            let short = r.strip_prefix("TRIGGER:").unwrap_or(r);
            let before = |prefix: &str, unit: &str| {
                short
                    .strip_prefix(prefix)?
                    .strip_suffix(unit)?
                    .parse::<u32>()
                    .ok()
                    .map(|n| format!("{n}{}", unit.to_lowercase()))
            };
            if short == "PT0S" {
                return "0".into();
            }
            before("-PT", "M")
                .or_else(|| before("-PT", "H"))
                .or_else(|| before("-P", "D"))
                .unwrap_or_else(|| r.clone())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 3).unwrap()
    }
    #[test]
    fn due_accepts_relative_and_brazilian_dates() {
        let tz = Some(chrono_tz::America::Sao_Paulo);
        let f = due_fields("amanhã 14:30", today(), tz).unwrap();
        assert_eq!(f["dueDate"], "2026-11-04T17:30:00+0000");
        assert_eq!(f["startDate"], f["dueDate"]);
        assert_eq!(f["isAllDay"], false);
        assert_eq!(f["timeZone"], "America/Sao_Paulo");
        let f = due_fields("25/12", today(), tz).unwrap();
        assert_eq!(f["dueDate"], "2026-12-25T03:00:00+0000");
        assert_eq!(f["isAllDay"], true);
        assert_eq!(due_fields("+2", today(), tz).unwrap()["dueDate"], "2026-11-05T03:00:00+0000");
        assert_eq!(due_fields("2027-01-10", today(), tz).unwrap()["dueDate"], "2027-01-10T03:00:00+0000");
        assert!(due_fields("", today(), tz).unwrap()["dueDate"].is_null());
        assert!(due_fields("ontem", today(), tz).is_err());
        assert!(due_fields("hoje 25:00", today(), tz).is_err());
    }
    #[test]
    fn due_roundtrips_to_prompt_text() {
        let tz = Some(chrono_tz::America::Sao_Paulo);
        assert_eq!(format_due("2026-11-04T17:30:00+0000", false, tz), "04/11/2026 14:30");
        assert_eq!(format_due("2026-12-25T03:00:00.000+0000", true, tz), "25/12/2026");
    }
    #[test]
    fn relative_due_labels() {
        let tz = Some(chrono_tz::America::Sao_Paulo);
        let label = |due, all_day| relative_due(due, all_day, tz, today()).unwrap();
        assert_eq!(label("2026-11-03T17:30:00+0000", false), ("hoje 14:30".into(), false));
        assert_eq!(label("2026-11-04T03:00:00+0000", true), ("amanhã".into(), false));
        assert_eq!(label("2026-11-06T03:00:00+0000", true), ("sex".into(), false));
        assert_eq!(label("2026-11-01T03:00:00+0000", true), ("01/11".into(), true));
        assert_eq!(label("2027-01-10T03:00:00+0000", true), ("10/01/27".into(), false));
    }
    #[test]
    fn reminders_roundtrip() {
        let r = parse_reminders("0, 15m, 1h, 2d, TRIGGER:P0DT9H0M0S").unwrap();
        assert_eq!(r, ["TRIGGER:PT0S", "TRIGGER:-PT15M", "TRIGGER:-PT1H", "TRIGGER:-P2D", "TRIGGER:P0DT9H0M0S"]);
        assert_eq!(format_reminders(&r), "0, 15m, 1h, 2d, TRIGGER:P0DT9H0M0S");
        assert!(parse_reminders("").unwrap().is_empty());
        assert!(parse_reminders("amanhã").is_err());
    }
}
