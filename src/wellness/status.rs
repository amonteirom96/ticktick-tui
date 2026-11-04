use super::{Timer, Phase, clock};
use crate::config;

impl Timer {
    /// Share the resolved TUI theme without probing a terminal from status commands.
    pub fn save_status_theme(theme: &config::Theme) -> anyhow::Result<()> {
        config::private_write(&config::cache_path().with_file_name("focus-theme.json"), &serde_json::to_vec(theme)?)
    }
    pub fn status_tmux(&self) -> anyhow::Result<String> {
        if self.state() == "idle" { return Ok(String::new()); }
        let theme: config::Theme = match std::fs::read(config::cache_path().with_file_name("focus-theme.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => config::Theme::default(),
            Err(e) => return Err(e.into()),
        };
        let color = if self.finished() || (self.pomodoro && self.phase != Phase::Focus) { &theme.success } else { &theme.accent };
        // Canonicalize validated colors before embedding tmux format syntax.
        let color: ratatui::style::Color = color.parse().map_err(|_| anyhow::anyhow!("Invalid timer theme color"))?;
        let elapsed = self.seconds();
        Ok(format!("#[fg={color}]{}#[default]", clock(self.target().map_or(elapsed, |t| t.saturating_sub(elapsed)))))
    }

    /// Read-only access for integrations. Missing state is idle; invalid state is an error.
    pub fn read() -> anyhow::Result<Self> {
        match std::fs::read(Self::path()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    fn state(&self) -> &'static str {
        if self.finished() {
            "completed"
        } else if self.started.is_some() {
            "running"
        } else if self.elapsed > 0 {
            "paused"
        } else {
            "idle"
        }
    }
    pub fn status(&self) -> serde_json::Value {
        let elapsed = self.seconds();
        serde_json::json!({
            "version": 1,
            "state": self.state(),
            "mode": if self.pomodoro { "pomodoro" } else { "stopwatch" },
            "phase": match self.phase { Phase::Focus => "focus", Phase::Short => "short_break", Phase::Long => "long_break" },
            "elapsed_seconds": elapsed,
            "remaining_seconds": self.target().map(|t| t.saturating_sub(elapsed)),
            "duration_seconds": self.target(),
            "cycle": self.cycle(),
            "task": self.task,
        })
    }
    /// No user content or tmux format sequences: safe for status bars.
    pub fn status_text(&self) -> String {
        let icon = match self.state() {
            "idle" => return String::new(),
            "completed" => "✔",
            "paused" => "⏸",
            _ if !self.pomodoro => "⏱",
            _ if self.phase != Phase::Focus => "☕",
            _ => "🍅",
        };
        let elapsed = self.seconds();
        format!(
            "{icon} {}",
            clock(self.target().map_or(elapsed, |t| t.saturating_sub(elapsed)))
        )
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    #[test]
    fn status_covers_idle_paused_running_completed_and_breaks() {
        let mut timer = Timer::default();
        assert_eq!(timer.status()["state"], "idle");
        assert_eq!(timer.status_text(), "");
        timer.elapsed = 60;
        assert_eq!(timer.status()["state"], "paused");
        assert_eq!(timer.status_text(), "⏸ 24:00");
        timer.started = Some(chrono::Utc::now().timestamp());
        assert_eq!(timer.status()["state"], "running");
        assert!(timer.status_text().starts_with("🍅 "));
        timer.elapsed = 1500;
        assert_eq!(timer.status()["remaining_seconds"], 0);
        assert_eq!(timer.status_text(), "✔ 00:00");
        timer.advance();
        timer.toggle(true);
        assert_eq!(timer.status()["phase"], "short_break");
        assert!(timer.status_text().starts_with("☕ "));
    }
    #[test]
    fn restored_stopwatch_and_task_are_exposed_without_tmux_injection() {
        let timer: Timer = serde_json::from_str(
            r##"{"pomodoro":false,"elapsed":3661,"task":{"id":"abc","title":"#[fg=red]task"}}"##,
        )
        .unwrap();
        assert_eq!(timer.status_text(), "⏸ 01:01:01");
        assert_eq!(timer.status()["remaining_seconds"], serde_json::Value::Null);
        assert_eq!(timer.status()["task"]["id"], "abc");
        assert_eq!(timer.status()["version"], 1);
    }
}
