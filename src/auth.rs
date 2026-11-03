use crate::config;
use anyhow::{Context, Result, bail};
use std::io::{self, Write};
pub fn login() -> Result<()> {
    let id = std::env::var("TICKTICK_CLIENT_ID")
        .context("Set TICKTICK_CLIENT_ID from developer.ticktick.com/manage")?;
    let secret = std::env::var("TICKTICK_CLIENT_SECRET").context("Set TICKTICK_CLIENT_SECRET")?;
    let redirect =
        std::env::var("TICKTICK_REDIRECT_URI").unwrap_or("http://localhost:8080/callback".into());
    let state = uuid::Uuid::new_v4().to_string();
    let mut url = reqwest::Url::parse("https://ticktick.com/oauth/authorize")?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", id.as_str()),
        ("scope", "tasks:read tasks:write"),
        ("state", &state),
        ("redirect_uri", &redirect),
        ("response_type", "code"),
    ]);
    println!(
        "Open this URL in your browser:\n{url}\n\nAfter approval, copy the full redirect URL from the address bar.\nThe localhost page need not load."
    );
    print!("Callback URL: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let callback = reqwest::Url::parse(input.trim()).context("Invalid callback URL")?;
    let mut expected = reqwest::Url::parse(&redirect)?;
    expected.set_query(None);
    let mut actual = callback.clone();
    actual.set_query(None);
    actual.set_fragment(None);
    if actual != expected {
        bail!("Callback does not match redirect URI");
    }
    let pairs: std::collections::HashMap<_, _> = callback.query_pairs().into_owned().collect();
    if pairs.get("state") != Some(&state) {
        bail!("OAuth state mismatch; restart authentication");
    }
    let code = pairs
        .get("code")
        .context("No authorization code; access may have been denied")?;
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .post("https://ticktick.com/oauth/token")
        .basic_auth(&id, Some(&secret))
        .form(&[
            ("code", code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", &redirect),
            ("scope", "tasks:read tasks:write"),
        ])
        .send()?;
    if !response.status().is_success() {
        bail!(
            "Token exchange failed (HTTP {})",
            response.status().as_u16()
        );
    }
    let body: serde_json::Value = response.json()?;
    let token = body["access_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Missing access token")?;
    config::private_write(&config::config_dir().join("token"), token.as_bytes())?;
    println!("Authenticated. Run ticktick-tui.");
    Ok(())
}
