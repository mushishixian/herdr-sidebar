//! The AI commit provider: an OpenAI-compatible chat-completions endpoint, its
//! API key and the model to ask. Set in ⚙ Settings → AI commit and stored in
//! `<state dir>/ai.json`, NOT in `state.json`: the key is a secret, so the file
//! is written owner-only (0600 on Unix) and never mirrored anywhere else.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Used when the URL field is left empty.
pub const DEFAULT_API_URL: &str = "https://api.openai.com/v1";

/// Longest value any field accepts; a pasted file is not an API key.
const MAX_FIELD_LEN: usize = 2048;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Base URL (`https://api.deepseek.com/v1`) or the full
    /// `…/chat/completions` endpoint. Empty means [`DEFAULT_API_URL`].
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    /// How hard the model may reason before answering. Off by default; a
    /// missing or unrecognised value loads as Off.
    #[serde(deserialize_with = "lenient_thinking")]
    pub thinking: Thinking,
}

/// Reasoning effort requested from the model. `Off` asks for none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl Thinking {
    /// The Settings value: `off`, `low`, `medium` or `high`.
    pub fn label(self) -> &'static str {
        match self {
            Thinking::Off => "off",
            Thinking::Low => "low",
            Thinking::Medium => "medium",
            Thinking::High => "high",
        }
    }

    /// The next setting in the cycle off → low → medium → high → off.
    pub fn next(self) -> Self {
        match self {
            Thinking::Off => Thinking::Low,
            Thinking::Low => Thinking::Medium,
            Thinking::Medium => Thinking::High,
            Thinking::High => Thinking::Off,
        }
    }

    /// The effort word sent to the API, or `None` when reasoning is off.
    pub fn effort(self) -> Option<&'static str> {
        match self {
            Thinking::Off => None,
            Thinking::Low => Some("low"),
            Thinking::Medium => Some("medium"),
            Thinking::High => Some("high"),
        }
    }
}

/// Accepts any JSON value for `thinking` and maps anything but a known level
/// to Off, so one bad value cannot make the whole ai.json fail to load.
fn lenient_thinking<'de, D>(deserializer: D) -> Result<Thinking, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

impl AiConfig {
    /// A request can be made: a key and a model are set (the URL has a default).
    pub fn is_configured(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.model.trim().is_empty()
    }

    /// The URL actually POSTed to: the base URL plus `/chat/completions`,
    /// unless the user already typed the full endpoint.
    pub fn endpoint(&self) -> String {
        let base = self.api_url.trim();
        let base = if base.is_empty() {
            DEFAULT_API_URL
        } else {
            base
        };
        let base = base.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    /// The key for display: `not set`, or `••••` plus its last four characters.
    pub fn masked_key(&self) -> String {
        mask_secret(self.api_key.trim())
    }
}

/// `not set` for an empty secret, otherwise bullets with the last four
/// characters visible (all bullets when the secret is that short).
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.is_empty() {
        return "not set".to_string();
    }
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

/// Trim a typed field and reject what cannot be a URL/key/model.
pub fn clean_field(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.len() > MAX_FIELD_LEN {
        return Err("value is too long".to_string());
    }
    if value.chars().any(char::is_control) {
        return Err("value contains control characters".to_string());
    }
    Ok(value.to_string())
}

fn config_path() -> Option<PathBuf> {
    Some(crate::state::state_dir()?.join("ai.json"))
}

/// The saved settings; a missing or unreadable file is the empty default.
pub fn load() -> AiConfig {
    config_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Write the settings atomically (temp file + rename), owner-only.
pub fn save(config: &AiConfig) -> Result<(), String> {
    let path = config_path().ok_or("no plugin state directory")?;
    let dir = path.parent().ok_or("no plugin state directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("state directory: {e}"))?;
    let json = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    let temp = dir.join(format!("ai.json.{}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("could not save AI settings: {e}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(url: &str) -> AiConfig {
        AiConfig {
            api_url: url.to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            thinking: Thinking::Off,
        }
    }

    #[test]
    fn endpoint_appends_chat_completions_once() {
        assert_eq!(
            config("").endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            config("https://api.deepseek.com/v1/").endpoint(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            config(" https://gw.example/v1/chat/completions ").endpoint(),
            "https://gw.example/v1/chat/completions"
        );
    }

    #[test]
    fn configured_needs_key_and_model() {
        assert!(config("").is_configured());
        assert!(!AiConfig::default().is_configured());
        let mut no_model = config("");
        no_model.model = "  ".into();
        assert!(!no_model.is_configured());
    }

    #[test]
    fn secrets_are_masked() {
        assert_eq!(mask_secret(""), "not set");
        assert_eq!(mask_secret("abc"), "•••");
        assert_eq!(mask_secret("sk-1234567890abcd"), "••••abcd");
    }

    #[test]
    fn missing_thinking_key_loads_as_off() {
        let config: AiConfig =
            serde_json::from_str(r#"{"api_url":"u","api_key":"k","model":"m"}"#).unwrap();
        assert_eq!(config.thinking, Thinking::Off);
        assert_eq!(config.model, "m");
    }

    #[test]
    fn known_thinking_level_loads() {
        let config: AiConfig = serde_json::from_str(r#"{"model":"m","thinking":"high"}"#).unwrap();
        assert_eq!(config.thinking, Thinking::High);
    }

    #[test]
    fn invalid_thinking_loads_as_off_and_keeps_other_fields() {
        let config: AiConfig = serde_json::from_str(
            r#"{"api_url":"u","api_key":"k","model":"m","thinking":"garbage"}"#,
        )
        .unwrap();
        assert_eq!(config.thinking, Thinking::Off);
        assert_eq!(config.api_url, "u");
        assert_eq!(config.api_key, "k");
        assert_eq!(config.model, "m");

        let numeric: AiConfig = serde_json::from_str(r#"{"thinking":3}"#).unwrap();
        assert_eq!(numeric.thinking, Thinking::Off);
    }

    #[test]
    fn thinking_cycles_and_maps_to_effort() {
        assert_eq!(Thinking::Off.next(), Thinking::Low);
        assert_eq!(Thinking::Low.next(), Thinking::Medium);
        assert_eq!(Thinking::Medium.next(), Thinking::High);
        assert_eq!(Thinking::High.next(), Thinking::Off);
        assert_eq!(Thinking::Off.effort(), None);
        assert_eq!(Thinking::Medium.effort(), Some("medium"));
        assert_eq!(Thinking::High.label(), "high");
    }

    #[test]
    fn fields_are_trimmed_and_validated() {
        assert_eq!(clean_field("  gpt-4o-mini \n").unwrap(), "gpt-4o-mini");
        assert!(clean_field("a\u{1b}b").is_err());
        assert!(clean_field(&"x".repeat(MAX_FIELD_LEN + 1)).is_err());
    }
}
