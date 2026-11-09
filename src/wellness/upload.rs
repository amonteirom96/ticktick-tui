use super::Timer;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(super) struct Session {
    start: i64,
    task: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FocusUpload {
    pub id: String,
    pub body: serde_json::Value,
    account: String,
    /// Persisted before dispatch: an uncertain POST is never retried automatically.
    attempted: bool,
}
impl Timer {
    pub fn bind_account(&mut self, account: String) {
        if self.account.as_ref() != Some(&account) {
            // Preserve the old session under its original credential before switching.
            self.finish_session();
            self.session = None;
            self.started = None;
            self.elapsed = 0;
            self.task = None;
            self.account = Some(account);
        }
    }
    pub(super) fn begin_session(&mut self) {
        if self.session.is_none() && (!self.pomodoro || self.phase == super::Phase::Focus) {
            self.session = Some(Session {
                start: chrono::Utc::now().timestamp(),
                task: self.task.as_ref().map(|t| t.id.clone()),
            });
        }
    }
    pub(super) fn finish_session(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let Some(account) = self.account.clone() else {
            return;
        };
        let now = chrono::Utc::now().timestamp();
        let seconds = self
            .target()
            .map_or(self.seconds(), |t| self.seconds().min(t));
        if seconds == 0 {
            return;
        }
        // Ignore any wall time after a pomodoro reached its target, including while closed.
        let overrun = self
            .target()
            .map_or(0, |t| self.seconds().saturating_sub(t));
        let end = now
            .saturating_sub(overrun.min(i64::MAX as u64) as i64)
            .max(session.start);
        let stamp = |t| {
            chrono::DateTime::from_timestamp(t, 0)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S%z")
                .to_string()
        };
        let mut body = serde_json::json!({
            "type": if self.pomodoro { 0 } else { 1 },
            "startTime": stamp(session.start), "endTime": stamp(end),
            "duration": seconds,
            "pauseDuration": (end - session.start).max(0) as u64 - seconds.min((end - session.start).max(0) as u64),
        });
        if let Some(task) = session.task {
            body["taskId"] = task.into();
        }
        self.uploads.push(FocusUpload {
            id: uuid::Uuid::new_v4().to_string(),
            body,
            account,
            attempted: false,
        });
    }
    pub fn finish_if_due(&mut self) -> bool {
        if self.finished() && self.session.is_some() {
            self.finish_session();
            true
        } else {
            false
        }
    }
    pub fn ready_uploads(&self) -> Vec<FocusUpload> {
        self.uploads
            .iter()
            .filter(|u| !u.attempted && self.account.as_ref() == Some(&u.account))
            .cloned()
            .collect()
    }
    pub fn mark_attempted(&mut self, id: &str) {
        if let Some(u) = self.uploads.iter_mut().find(|u| u.id == id) {
            u.attempted = true;
        }
    }
    pub fn acknowledge(&mut self, id: &str) {
        self.uploads.retain(|u| u.id != id);
    }
    pub fn retry_uploads(&mut self) {
        for u in &mut self.uploads {
            if self.account.as_ref() == Some(&u.account) {
                u.attempted = false;
            }
        }
    }
    pub fn remap_task(&mut self, old: &str, new: &str) {
        if let Some(task) = &mut self.task
            && task.id == old
        {
            task.id = new.into();
        }
        if let Some(session) = &mut self.session
            && session.task.as_deref() == Some(old)
        {
            session.task = Some(new.into());
        }
        for upload in &mut self.uploads {
            if self.account.as_ref() == Some(&upload.account)
                && upload.body["taskId"].as_str() == Some(old)
            {
                upload.body["taskId"] = new.into();
            }
        }
    }
    pub fn sync_status(&self) -> String {
        let pending = self
            .uploads
            .iter()
            .filter(|u| self.account.as_ref() == Some(&u.account))
            .count();
        if self.account.is_none() {
            "demo · no uploads".into()
        } else if pending > 0 {
            format!("{pending} pending uploads · :focus-retry after checking history")
        } else {
            "sessions saved to TickTick".into()
        }
    }
    pub fn uncertain_uploads(&self) -> usize {
        self.uploads
            .iter()
            .filter(|u| u.attempted && self.account.as_ref() == Some(&u.account))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FocusTask, Phase};
    use super::*;
    fn running(pomodoro: bool, seconds: u64) -> Timer {
        let mut timer = Timer::default();
        timer.bind_account("account-a".into());
        timer.pomodoro = pomodoro;
        timer.task = Some(FocusTask {
            id: "task-1".into(),
            title: "Work".into(),
        });
        timer.toggle(pomodoro);
        let start = chrono::Utc::now().timestamp() - seconds as i64;
        timer.started = Some(start);
        timer.session.as_mut().unwrap().start = start;
        timer
    }
    #[test]
    fn completed_pomodoro_caps_time_and_never_uploads_breaks_twice() {
        let mut timer = running(true, 1800);
        assert!(timer.finish_if_due());
        assert!(!timer.finish_if_due());
        let uploads = timer.ready_uploads();
        assert_eq!(uploads.len(), 1);
        let body = &uploads[0].body;
        assert_eq!(body["type"], 0);
        assert_eq!(body["duration"], 1500);
        assert_eq!(body["taskId"], "task-1");
        let start = chrono::DateTime::parse_from_str(
            body["startTime"].as_str().unwrap(),
            "%Y-%m-%dT%H:%M:%S%z",
        )
        .unwrap();
        let end = chrono::DateTime::parse_from_str(
            body["endTime"].as_str().unwrap(),
            "%Y-%m-%dT%H:%M:%S%z",
        )
        .unwrap();
        assert_eq!((end - start).num_seconds(), 1500);
        timer.toggle(true);
        assert_eq!(timer.phase, Phase::Short);
        timer.started = Some(chrono::Utc::now().timestamp() - 300);
        timer.finish_if_due();
        timer.advance();
        assert_eq!(timer.ready_uploads().len(), 1);
    }
    #[test]
    fn pause_resume_preserves_effective_duration_and_stopwatch_type() {
        let mut timer = running(false, 60);
        timer.toggle(false);
        assert!(timer.ready_uploads().is_empty());
        assert!(timer.started.is_none());
        // 30 seconds of pause, then 20 seconds more work.
        timer.session.as_mut().unwrap().start -= 50;
        timer.toggle(false);
        timer.started = Some(chrono::Utc::now().timestamp() - 20);
        timer.reset(false);
        let uploads = timer.ready_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].body["type"], 1);
        assert_eq!(uploads[0].body["duration"], 80);
        assert_eq!(uploads[0].body["pauseDuration"], 30);
        assert_eq!(timer.seconds(), 0);
    }
    #[test]
    fn retry_state_survives_restart_and_stays_with_original_account() {
        let mut timer = running(true, 120);
        timer.reset(true);
        let upload = timer.ready_uploads().pop().unwrap();
        timer.mark_attempted(&upload.id);
        let mut timer: Timer =
            serde_json::from_slice(&serde_json::to_vec(&timer).unwrap()).unwrap();
        assert!(timer.ready_uploads().is_empty());
        assert_eq!(timer.uncertain_uploads(), 1);
        timer.bind_account("account-b".into());
        timer.retry_uploads();
        assert!(timer.ready_uploads().is_empty());
        timer.bind_account("account-a".into());
        timer.retry_uploads();
        assert_eq!(timer.ready_uploads().len(), 1);
        timer.acknowledge(&upload.id);
        assert!(timer.ready_uploads().is_empty());
    }
    #[test]
    fn changing_task_saves_old_task_and_keeps_running_with_new_task() {
        let mut timer = running(true, 60);
        timer.set_task(None);
        assert_eq!(timer.ready_uploads()[0].body["taskId"], "task-1");
        assert!(timer.started.is_some());
        assert!(timer.session.as_ref().unwrap().task.is_none());
        assert!(timer.set_durations("50 10 20").is_err());
    }
    #[test]
    fn restores_running_session_and_mode_switch_saves_paused_session() {
        let timer = running(true, 1500);
        let mut timer: Timer =
            serde_json::from_slice(&serde_json::to_vec(&timer).unwrap()).unwrap();
        assert!(timer.finish_if_due());
        assert_eq!(timer.ready_uploads().len(), 1);
        let mut timer = running(false, 75);
        timer.toggle(false);
        assert!(timer.select_mode(true));
        assert_eq!(timer.ready_uploads()[0].body["duration"], 75);
        assert!(timer.pomodoro);
        assert_eq!(timer.phase, Phase::Focus);
    }
}
