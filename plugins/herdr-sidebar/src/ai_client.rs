//! Direct calls to an OpenAI-compatible chat-completions endpoint, the
//! provider behind AI commit and the ✧ suggestion. The request goes through a
//! `curl` subprocess (the same pattern as the update check), so there is no
//! HTTP client crate in the sidebar.
//!
//! The API key never appears in argv, where other users could read it with
//! `ps`. curl gets its whole configuration (headers, URL and the JSON body) on
//! stdin through `-K -`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::ai_config::{AiConfig, Thinking};

/// Instructions for the commit message request.
const PROMPT: &str = "Write a Conventional Commits message for the staged git diff the user \
    sends. The first line is `<type>(<optional scope>): <subject>`: imperative mood, under 72 \
    characters, no trailing period. Add a blank line and a short body only when the change \
    really needs explanation. Reply with ONLY the commit message: no code fences, no quotes, \
    no commentary.";

/// Output budget for a commit message.
const COMMIT_MAX_TOKENS: u32 = 500;

/// Output budget once any reasoning level is on: reasoning tokens count
/// against it, and a tiny budget returns empty content.
const THINKING_MAX_TOKENS: u32 = 4000;

/// How long the connection test may take.
const TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Largest reply accepted, in bytes (curl `--max-filesize`).
const MAX_REPLY_BYTES: &str = "2097152";

/// Ask the configured model for a commit message describing `diff`.
pub fn commit_message(config: &AiConfig, diff: &str, timeout: Duration) -> Result<String, String> {
    let reply = complete(config, Some(PROMPT), diff, COMMIT_MAX_TOKENS, timeout)?;
    clean_message(&reply).ok_or_else(|| "the model returned an empty message".to_string())
}

/// Send a tiny request to check the URL, key and model. Returns a short
/// success line for the Settings UI.
pub fn test_connection(config: &AiConfig) -> Result<String, String> {
    let reply = complete(
        config,
        None,
        "Reply with the single word OK.",
        16,
        TEST_TIMEOUT,
    )?;
    if reply.trim().is_empty() {
        return Err("the model returned no text".to_string());
    }
    Ok(format!(
        "connected · {} · thinking {}",
        config.model.trim(),
        config.thinking.label()
    ))
}

/// One chat completion with the configured thinking setting. If a strict
/// provider rejects the thinking fields, the request is sent once more
/// without them (and with the plain budget).
fn complete(
    config: &AiConfig,
    system: Option<&str>,
    user: &str,
    base_tokens: u32,
    timeout: Duration,
) -> Result<String, String> {
    let model = config.model.trim();
    let fields = thinking_fields(&config.endpoint(), config.thinking);
    let budget = match config.thinking {
        Thinking::Off => base_tokens,
        _ => THINKING_MAX_TOKENS,
    };
    let stdout = send(
        config,
        &request_body(model, system, user, budget, &fields),
        timeout,
    )?;
    match parse_reply(&stdout) {
        Err(error)
            if !fields.is_empty()
                && status_of(&stdout).is_some_and(|code| rejects_thinking_fields(code, &error)) =>
        {
            let stdout = send(
                config,
                &request_body(model, system, user, base_tokens, &Map::new()),
                timeout,
            )?;
            parse_reply(&stdout)
        }
        other => other,
    }
}

/// The request fields that set the thinking level for `endpoint`. OpenAI's
/// API only knows `reasoning_effort`; every other host gets DeepSeek's
/// `thinking` object, with `reasoning_effort` added for a level.
fn thinking_fields(endpoint: &str, thinking: Thinking) -> Map<String, Value> {
    let mut fields = Map::new();
    if host_of(endpoint).eq_ignore_ascii_case("api.openai.com") {
        fields.insert(
            "reasoning_effort".to_string(),
            Value::from(thinking.effort().unwrap_or("minimal")),
        );
    } else {
        match thinking.effort() {
            None => {
                fields.insert(
                    "thinking".to_string(),
                    serde_json::json!({"type": "disabled"}),
                );
            }
            Some(effort) => {
                fields.insert(
                    "thinking".to_string(),
                    serde_json::json!({"type": "enabled"}),
                );
                fields.insert("reasoning_effort".to_string(), Value::from(effort));
            }
        }
    }
    fields
}

/// The host part of a URL: scheme stripped, then everything up to the first
/// `/` or `:`.
fn host_of(endpoint: &str) -> &str {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    rest.split(['/', ':']).next().unwrap_or("")
}

/// Whether a failed reply is a strict provider refusing the thinking fields:
/// HTTP 400/422 whose message names them.
fn rejects_thinking_fields(code: u16, message: &str) -> bool {
    if code != 400 && code != 422 {
        return false;
    }
    let message = message.to_ascii_lowercase();
    [
        "thinking",
        "reasoning",
        "unrecognized",
        "unknown field",
        "unsupported parameter",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// The chat-completions JSON body. Non-streaming, low temperature, plus the
/// thinking `extra` fields.
fn request_body(
    model: &str,
    system: Option<&str>,
    user: &str,
    max_tokens: u32,
    extra: &Map<String, Value>,
) -> String {
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    messages.push(serde_json::json!({"role": "user", "content": user}));
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": 0.2,
        "max_tokens": max_tokens,
        "stream": false,
    });
    if let Some(object) = body.as_object_mut() {
        object.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    body.to_string()
}

/// The HTTP status on curl's last stdout line, when there is one.
fn status_of(stdout: &str) -> Option<u16> {
    stdout
        .rsplit_once('\n')
        .unwrap_or(("", stdout))
        .1
        .trim()
        .parse()
        .ok()
}

/// Run curl for one request and return its raw stdout: the reply body
/// followed by the HTTP status line (see [`parse_reply`]).
fn send(config: &AiConfig, body: &str, timeout: Duration) -> Result<String, String> {
    let secs = timeout.as_secs().max(1).to_string();
    let script = curl_config(&config.endpoint(), config.api_key.trim(), body);

    let mut command = Command::new("curl");
    command
        .args([
            "-q",
            "--silent",
            "--show-error",
            "--proto",
            "=https,http",
            "--connect-timeout",
            "10",
            "--max-time",
            &secs,
            "--max-filesize",
            MAX_REPLY_BYTES,
            "--user-agent",
            "herdr-sidebar",
            "--write-out",
            "\n%{http_code}",
            "-K",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }

    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "AI commit needs curl on PATH".to_string()
        } else {
            format!("could not run curl: {e}")
        }
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        // Written off-thread so a slow curl cannot deadlock the pipe; dropping
        // the handle at the end of the thread gives curl EOF.
        thread::spawn(move || {
            let _ = stdin.write_all(script.as_bytes());
        });
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("curl did not finish: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("curl failed ({})", output.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The curl configuration: headers, body and URL, with every value quoted and
/// escaped for curl's config-file syntax.
fn curl_config(endpoint: &str, key: &str, body: &str) -> String {
    format!(
        "header = \"Authorization: Bearer {}\"\n\
         header = \"Content-Type: application/json\"\n\
         data-binary = \"{}\"\n\
         url = \"{}\"\n",
        curl_quote(key),
        curl_quote(body),
        curl_quote(endpoint),
    )
}

/// Escape a value for a double-quoted curl config string.
fn curl_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Pure half of [`post`]: curl's stdout is the JSON body followed by a last
/// line holding the HTTP status code (from `--write-out`).
fn parse_reply(stdout: &str) -> Result<String, String> {
    let (body, code) = stdout.rsplit_once('\n').unwrap_or(("", stdout));
    let code: u16 = code
        .trim()
        .parse()
        .map_err(|_| "the API reply had no HTTP status".to_string())?;
    let value: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => return Err(http_error(code, body)),
    };
    let api_message = api_error(&value);
    if !(200..300).contains(&code) {
        let message = api_message.unwrap_or_else(|| format!("HTTP {code}"));
        return Err(if code == 401 || code == 403 {
            format!("API key rejected: {message}")
        } else {
            message
        });
    }
    // Some gateways wrap the completion in a `data` object.
    let root = if value.get("choices").is_some() {
        &value
    } else {
        value.get("data").unwrap_or(&value)
    };
    match root
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
    {
        Some(content) => Ok(content.to_string()),
        None => Err(api_message.unwrap_or_else(|| "the reply had no message content".to_string())),
    }
}

/// The error text the API reports, in any of the shapes providers use.
fn api_error(value: &Value) -> Option<String> {
    value
        .pointer("/error/message")
        .or_else(|| value.get("error"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `HTTP <code>: <start of the body>` for a reply that is not JSON.
fn http_error(code: u16, body: &str) -> String {
    let snippet: String = body.trim().chars().take(200).collect();
    if snippet.is_empty() {
        format!("HTTP {code}")
    } else {
        format!("HTTP {code}: {snippet}")
    }
}

/// Turn the model's raw answer into a commit message: drops `<think>` blocks
/// from reasoning models, code fences, surrounding quotes, and a trailing
/// period on the subject line. `None` when nothing is left.
fn clean_message(raw: &str) -> Option<String> {
    let mut body = raw.to_string();
    while let Some(start) = body.find("<think>") {
        match body[start..].find("</think>") {
            Some(end) => body.replace_range(start..start + end + "</think>".len(), ""),
            None => {
                body.truncate(start);
                break;
            }
        }
    }

    let mut text = body.trim();
    if let Some(rest) = text.strip_prefix("```") {
        // The opening fence line may name a language.
        let rest = match rest.split_once('\n') {
            Some((_, inner)) => inner,
            None => rest,
        };
        text = rest.trim_end().strip_suffix("```").unwrap_or(rest).trim();
    }
    if text.len() >= 2 {
        let bytes = text.as_bytes();
        let first = bytes[0];
        if matches!(first, b'"' | b'\'' | b'`') && bytes[bytes.len() - 1] == first {
            text = text[1..text.len() - 1].trim();
        }
    }

    let cleaned = match text.split_once('\n') {
        Some((subject, rest)) => format!("{}\n{}", subject.trim_end().trim_end_matches('.'), rest),
        None => text.trim_end_matches('.').to_string(),
    };
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_config_escapes_quotes_backslashes_and_newlines() {
        assert_eq!(curl_quote(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(curl_quote("x\ny\r\tz"), r"x\ny\r\tz");

        let config = curl_config(
            "https://h/v1/chat/completions",
            "k\"1",
            "{\"m\":\"a\\b\"}\n",
        );
        assert!(config.contains(r#"header = "Authorization: Bearer k\"1""#));
        assert!(config.contains("header = \"Content-Type: application/json\"\n"));
        assert!(config.contains(r#"data-binary = "{\"m\":\"a\\b\"}\n""#));
        assert!(config.contains(r#"url = "https://h/v1/chat/completions""#));
        // One config line per option: the body cannot start a new option.
        assert_eq!(config.lines().count(), 4);
    }

    #[test]
    fn reply_with_top_level_choices() {
        let stdout = r#"{"choices":[{"message":{"content":"feat: x"}}]}
200"#;
        assert_eq!(parse_reply(stdout), Ok("feat: x".to_string()));
    }

    #[test]
    fn reply_with_data_wrapped_choices() {
        let stdout = r#"{"data":{"choices":[{"message":{"content":"fix: y"}}]}}
200"#;
        assert_eq!(parse_reply(stdout), Ok("fix: y".to_string()));
    }

    #[test]
    fn openai_error_object_is_reported() {
        let stdout = "{\"error\":{\"message\":\"model not found\",\"type\":\"x\"}}\n404";
        assert_eq!(parse_reply(stdout), Err("model not found".to_string()));
    }

    #[test]
    fn unauthorized_is_labelled_as_a_rejected_key() {
        let stdout = "{\"error\":{\"message\":\"Incorrect API key\"}}\n401";
        assert_eq!(
            parse_reply(stdout),
            Err("API key rejected: Incorrect API key".to_string())
        );
        let string_error = "{\"error\":\"bad\"}\n400";
        assert_eq!(parse_reply(string_error), Err("bad".to_string()));
    }

    #[test]
    fn non_json_body_reports_the_status() {
        assert_eq!(
            parse_reply("<html>boom</html>\n502"),
            Err("HTTP 502: <html>boom</html>".to_string())
        );
        assert_eq!(parse_reply("\n503"), Err("HTTP 503".to_string()));
    }

    #[test]
    fn clean_message_drops_think_blocks() {
        assert_eq!(
            clean_message("<think>hmm\nlet me see</think>\nfeat(ui): add toggle"),
            Some("feat(ui): add toggle".to_string())
        );
        assert_eq!(clean_message("<think>never closed"), None);
    }

    #[test]
    fn clean_message_strips_fences() {
        assert_eq!(
            clean_message("```text\nfix: handle empty input\n```"),
            Some("fix: handle empty input".to_string())
        );
        assert_eq!(
            clean_message("```\nrefactor: split launch\n```"),
            Some("refactor: split launch".to_string())
        );
    }

    #[test]
    fn clean_message_strips_quotes_and_subject_period() {
        assert_eq!(
            clean_message("\"docs: update readme.\""),
            Some("docs: update readme".to_string())
        );
        assert_eq!(
            clean_message("`chore: bump deps`"),
            Some("chore: bump deps".to_string())
        );
    }

    #[test]
    fn clean_message_keeps_a_multi_line_body() {
        assert_eq!(
            clean_message("feat: add x.\n\nBody line one.\nLine two."),
            Some("feat: add x\n\nBody line one.\nLine two.".to_string())
        );
    }

    #[test]
    fn thinking_fields_for_deepseek() {
        let url = "https://api.deepseek.com/v1/chat/completions";
        let off = thinking_fields(url, Thinking::Off);
        assert_eq!(
            Value::Object(off),
            serde_json::json!({"thinking": {"type": "disabled"}})
        );
        let high = thinking_fields(url, Thinking::High);
        assert_eq!(
            Value::Object(high),
            serde_json::json!({"thinking": {"type": "enabled"}, "reasoning_effort": "high"})
        );
    }

    #[test]
    fn thinking_fields_for_openai() {
        let url = "https://api.openai.com/v1/chat/completions";
        let off = thinking_fields(url, Thinking::Off);
        assert_eq!(
            Value::Object(off),
            serde_json::json!({"reasoning_effort": "minimal"})
        );
        let low = thinking_fields(url, Thinking::Low);
        assert_eq!(
            Value::Object(low),
            serde_json::json!({"reasoning_effort": "low"})
        );
    }

    #[test]
    fn local_gateway_is_not_openai() {
        let url = "http://127.0.0.1:8080/v1/chat/completions";
        assert!(thinking_fields(url, Thinking::Off).contains_key("thinking"));
        assert_eq!(host_of(url), "127.0.0.1");
    }

    #[test]
    fn request_body_off_disables_thinking() {
        let fields = thinking_fields(
            "https://api.deepseek.com/v1/chat/completions",
            Thinking::Off,
        );
        let body = request_body("m", None, "hi", 16, &fields);
        assert!(body.contains(r#""thinking":{"type":"disabled"}"#));
        assert!(body.contains(r#""max_tokens":16"#));
    }

    #[test]
    fn strict_provider_rejections_are_recognised() {
        assert!(rejects_thinking_fields(
            400,
            "Unrecognized request argument supplied: thinking"
        ));
        assert!(rejects_thinking_fields(422, "invalid reasoning_effort"));
        assert!(rejects_thinking_fields(400, "Unknown field: thinking"));
        assert!(rejects_thinking_fields(400, "Unsupported parameter: x"));
        assert!(!rejects_thinking_fields(401, "reasoning"));
        assert!(!rejects_thinking_fields(500, "thinking"));
        assert!(!rejects_thinking_fields(400, "model not found"));
    }

    #[test]
    fn clean_message_empty_is_none() {
        assert_eq!(clean_message("  \n\n"), None);
        assert_eq!(clean_message("``````"), None);
    }
}
