use crate::{
    api::Api,
    config::{self, Config},
};
use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::{prelude::*, widgets::*};
use std::{
    sync::mpsc::{self, Receiver},
    time::Duration,
};

#[derive(Default)]
struct Form {
    token: String,
    message: String,
    pending: Option<Receiver<Result<Api>>>,
}
impl Form {
    fn submit(&mut self) {
        let token = self.token.trim().to_owned();
        if token.is_empty() {
            self.message = "Paste your API token before continuing.".into();
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.message = "Validating with TickTick…".into();
        std::thread::spawn(move || {
            let result = Api::new(token).and_then(|api| {
                api.validate()?;
                Ok(api)
            });
            let _ = tx.send(result);
        });
    }
    fn paste(&mut self, value: &str) {
        // Ignore terminal control characters, including line breaks from pasted text.
        self.token.extend(value.chars().filter(|c| !c.is_control()));
    }
    fn draw(&self, f: &mut Frame, c: &Config) {
        let color = |s: &str| s.parse::<Color>().unwrap_or(Color::Reset);
        let style = Style::default()
            .bg(color(&c.theme.background))
            .fg(color(&c.theme.foreground));
        f.render_widget(Block::default().style(style), f.area());
        let width = f.area().width.min(76);
        let height = f.area().height.min(15);
        let area = Rect::new(
            (f.area().width - width) / 2,
            (f.area().height - height) / 2,
            width,
            height,
        );
        let mask = "•".repeat(
            self.token
                .chars()
                .count()
                .min(width.saturating_sub(14) as usize),
        );
        let text = format!(
            "\nConnect your account with a TickTick API token.\nPaste your token below; input is hidden.\n\nAPI token: {mask}\n\n{}\n\nEnter  validate and save    Ctrl-u  clear    Esc  cancel\n\nYour token is stored separately from the theme, with restricted access.",
            self.message
        );
        if area.width >= 14 && area.height > 6 {
            f.set_cursor_position((area.x + 12 + mask.chars().count() as u16, area.y + 5));
        }
        f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(style)
                .block(
                    Block::bordered()
                        .title(" TickTick · API token ")
                        .border_style(Style::default().fg(color(&c.theme.accent))),
                ),
            area,
        );
    }
}
struct PasteGuard;
impl Drop for PasteGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    }
}
pub fn configure<B: Backend>(terminal: &mut Terminal<B>, config: &Config) -> Result<Option<Api>> {
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
    let _paste = PasteGuard;
    let mut form = Form::default();
    loop {
        if let Some(result) = form.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
            form.pending = None;
            match result {
                Ok(api) => {
                    match config::private_write(
                        &config::config_dir().join("token"),
                        form.token.trim().as_bytes(),
                    ) {
                        Ok(()) => {
                            form.token.clear();
                            return Ok(Some(api));
                        }
                        Err(_) => {
                            form.message =
                                "Could not save token. Check permissions and try again.".into()
                        }
                    }
                }
                Err(_) => {
                    form.message =
                        "Validation failed. Check your token and connection and try again.".into()
                }
            }
        }
        terminal.draw(|f| form.draw(f, config))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    return Ok(None);
                }
                if form.pending.is_some() {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => form.submit(),
                    KeyCode::Backspace => {
                        form.token.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        form.token.clear()
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        form.paste(&c.to_string())
                    }
                    _ => (),
                }
            }
            Event::Paste(value) if form.pending.is_none() => form.paste(&value),
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_is_masked_and_control_characters_are_removed() {
        let mut form = Form::default();
        form.paste("secret-token\r\n\x1b");
        assert_eq!(form.token, "secret-token");
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| form.draw(f, &Config::default())).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!text.contains("secret-token"));
        assert!(text.contains("••••"));
    }
    #[test]
    fn empty_token_does_not_start_validation() {
        let mut form = Form {
            token: "   ".into(),
            ..Default::default()
        };
        form.submit();
        assert!(form.pending.is_none());
        assert!(!form.message.is_empty());
    }
}
