use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Theme {
    pub background: String,
    pub panel: String,
    pub border: String,
    pub heading: String,
    pub code: String,
    pub code_background: String,
    pub foreground: String,
    pub muted: String,
    pub accent: String,
    pub selection: String,
    pub danger: String,
    pub success: String,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#20242c".into(),
            panel: "#20242c".into(),
            border: "#42485a".into(),
            heading: "#e6edf3".into(),
            code: "#5dbbc1".into(),
            code_background: "#2b313e".into(),
            foreground: "#c9d1d9".into(),
            muted: "#8b949e".into(),
            accent: "#79c0ff".into(),
            selection: "#33435a".into(),
            danger: "#ff7b72".into(),
            success: "#7ee787".into(),
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub sync_seconds: u64,
    pub user_name: Option<String>,
    pub theme_file: Option<String>,
    pub theme: Theme,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            sync_seconds: 60,
            user_name: None,
            theme_file: None,
            theme: Theme::default(),
        }
    }
}
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::BaseDirs::new()
                .expect("home directory")
                .home_dir()
                .join(".config")
        })
        .join("ticktick-tui")
}
pub fn cache_path() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::BaseDirs::new()
                .expect("home directory")
                .home_dir()
                .join(".cache")
        })
        .join("ticktick-tui/cache.json")
}
pub fn load() -> Result<Config> {
    let path = config_dir().join("config.toml");
    let text = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let table: toml::Table = toml::from_str(&text).context("Invalid config.toml")?;
    let mut c: Config = toml::from_str(&text).context("Invalid config.toml")?;
    if let Some(file) = &c.theme_file {
        c.theme = read_theme(&config_dir().join(file))?;
    } else if !table.contains_key("theme") {
        // No explicit theme: follow the terminal background (user's theme-*.toml first).
        // Must run before raw mode.
        let light = terminal_light::luma().is_ok_and(|l| l > 0.6);
        c.theme = named_theme(if light { "light" } else { "dark" })?;
    }
    validate_theme(&c.theme)?;
    Ok(c)
}
fn validate_theme(t: &Theme) -> Result<()> {
    for color in [
        &t.background,
        &t.panel,
        &t.border,
        &t.heading,
        &t.code,
        &t.code_background,
        &t.foreground,
        &t.muted,
        &t.accent,
        &t.selection,
        &t.danger,
        &t.success,
    ] {
        if color.parse::<ratatui::style::Color>().is_err() {
            bail!("Invalid theme color: {color}");
        }
    }
    Ok(())
}
fn read_theme(path: &Path) -> Result<Theme> {
    let config: Config = toml::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("Cannot read theme {}", path.display()))?,
    )?;
    validate_theme(&config.theme)?;
    Ok(config.theme)
}
pub fn named_theme(name: &str) -> Result<Theme> {
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("Use a theme name such as dark, light or tokyo-night");
    }
    let path = config_dir().join(format!("theme-{name}.toml"));
    if path.exists() {
        return read_theme(&path);
    }
    let source = match name {
        "dark" => include_str!("../themes/dark.toml"),
        "light" => include_str!("../themes/light.toml"),
        "tokyo-night" => include_str!("../themes/tokyo-night.toml"),
        _ => bail!("Theme not found: {}", path.display()),
    };
    let config: Config = toml::from_str(source)?;
    validate_theme(&config.theme)?;
    Ok(config.theme)
}

pub fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().context("Missing parent")?)?;
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    use std::io::Write;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn token() -> Result<Option<String>> {
    let stored = match fs::read_to_string(config_dir().join("token")) {
        Ok(value) => Some(value),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("Cannot read saved token"),
    };
    Ok(stored
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("TICKTICK_TOKEN").ok())
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().to_owned()))
}
