//! Codex quota, from one of two sources.
//!
//! The primary arm is one `GET /backend-api/wham/usage`, the request the
//! Codex CLI makes for `/status` (provenance `usage_api`). The fallback arm is
//! the newest rate-limit snapshot Codex itself wrote into its rollout files
//! (provenance `local_rollout`, no request).
//!
//! The fallback runs when the usage API gives no answer: the credential file
//! is missing or unusable, the `codex` binary cannot supply a version for the
//! User-Agent, the request fails or times out, the status is not 2xx, or the
//! body is not the expected JSON. A note on stderr says why. Two cases never
//! fall back: a 429 (it is reported with its `retry-after`), and a token
//! variable that is set without its account id (a configuration error).
//!
//! The same upstream client also has a `rate-limit-reset-credits` family of
//! endpoints. Nothing here calls it: spending a reset is a manual user action.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::common::{
    Context, Error, Harness, Headers, Record, Secret, cli_version, http_get_json, read_json,
    record, usage,
};
use crate::timefmt::to_iso;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Upstream sets no timeout of its own on this call. Ten seconds is ample for
/// one small JSON response, and a local fallback exists when it is not.
const TIMEOUT: Duration = Duration::from_secs(10);
const ORIGINATOR: &str = "codex_cli_rs";
const TOKEN_VAR: &str = "ATB_CODEX_TOKEN";
const ACCOUNT_ID_VAR: &str = "ATB_CODEX_ACCOUNT_ID";

struct Credentials {
    token: Secret,
    account_id: Option<String>,
    /// `env`, or the credential file's path.
    source: String,
}

/// Credentials passed in the environment, if the token variable is set.
fn env_credentials(ctx: &Context) -> Result<Option<Credentials>, Error> {
    let Some(token) = ctx.secret(TOKEN_VAR)? else {
        return Ok(None);
    };
    Ok(Some(Credentials {
        token,
        account_id: Some(ctx.companion(TOKEN_VAR, ACCOUNT_ID_VAR)?),
        source: "env".into(),
    }))
}

fn file_credentials(dir: &Path) -> Result<Credentials, Error> {
    let path = dir.join("auth.json");
    let data = read_json(&path, "Codex credentials")?;
    let tokens = data.get("tokens");
    let field = |name: &str| {
        tokens?
            .get(name)?
            .as_str()
            .filter(|value| !value.is_empty())
    };
    let token = field("access_token")
        .ok_or_else(|| usage(format!("no tokens.access_token in {}", path.display())))?;
    Ok(Credentials {
        token: Secret::new(token.into()),
        account_id: field("account_id").map(str::to_owned),
        source: path.display().to_string(),
    })
}

/// The headers the Codex backend client sends for the usage read. Like it,
/// leave the account header out when no account id is known.
fn build_headers(token: &Secret, account_id: Option<&str>, user_agent: &str) -> Headers {
    let mut headers = vec![("Authorization", format!("Bearer {}", token.expose()))];
    if let Some(account_id) = account_id {
        headers.push(("ChatGPT-Account-Id", account_id.into()));
    }
    headers.push(("User-Agent", user_agent.into()));
    Headers(headers)
}

/// The User-Agent as the Codex CLI builds it:
/// `codex_cli_rs/<version> (<os> <os version>; <arch>) <terminal>`, with any
/// character outside printable ASCII replaced by an underscore.
fn user_agent(version: &str, os: &str, os_version: &str, arch: &str, terminal: &str) -> String {
    format!("{ORIGINATOR}/{version} ({os} {os_version}; {arch}) {terminal}")
        .chars()
        .map(|ch| if matches!(ch, ' '..='~') { ch } else { '_' })
        .collect()
}

/// The terminal token the Codex CLI appends to its User-Agent, detected from
/// the environment in the same order as upstream's terminal detection.
fn terminal_token(env: &dyn Fn(&str) -> Option<String>) -> String {
    let has = |name: &str| env(name).is_some();
    let non_empty = |name: &str| env(name).filter(|value| !value.trim().is_empty());
    let versioned = |name: &str, version_var: &str| match non_empty(version_var) {
        Some(version) => format!("{name}/{version}"),
        None => name.to_owned(),
    };
    let term = env("TERM").unwrap_or_default();

    let raw = if let Some(program) =
        non_empty("TERM_PROGRAM").filter(|program| !program.eq_ignore_ascii_case("tmux"))
    {
        versioned(&program, "TERM_PROGRAM_VERSION")
    } else if non_empty("GHOSTTY_RESOURCES_DIR").is_some() {
        "Ghostty".into()
    } else if has("WEZTERM_VERSION") {
        versioned("WezTerm", "WEZTERM_VERSION")
    } else if has("ITERM_SESSION_ID") || has("ITERM_PROFILE") || has("ITERM_PROFILE_NAME") {
        "iTerm.app".into()
    } else if has("TERM_SESSION_ID") {
        "Apple_Terminal".into()
    } else if has("KITTY_WINDOW_ID") || term.contains("kitty") {
        "kitty".into()
    } else if has("ALACRITTY_SOCKET") || term == "alacritty" {
        "Alacritty".into()
    } else if has("KONSOLE_VERSION") {
        versioned("Konsole", "KONSOLE_VERSION")
    } else if has("GNOME_TERMINAL_SCREEN") {
        "gnome-terminal".into()
    } else if has("VTE_VERSION") {
        versioned("VTE", "VTE_VERSION")
    } else if has("WT_SESSION") {
        "WindowsTerminal".into()
    } else if let Some(term) = non_empty("TERM") {
        term
    } else {
        "unknown".into()
    };
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Codex reports a window by its length, not by a name; these are the two it uses.
fn window_name(minutes: &Value) -> String {
    match minutes.as_f64() {
        Some(300.0) => "five_hour".into(),
        Some(10080.0) => "seven_day".into(),
        Some(other) if other != 0.0 => format!("window_{minutes}m"),
        _ => "unknown_window".into(),
    }
}

fn get(object: &Value, key: &str) -> Value {
    object.get(key).cloned().unwrap_or(Value::Null)
}

fn window_entry(slot: &str, used_percent: Value, minutes: Value, resets_at: &Value) -> Value {
    let mut entry = Map::new();
    entry.insert("slot".into(), slot.into());
    entry.insert("used_percent".into(), used_percent);
    entry.insert("window_minutes".into(), minutes);
    entry.insert("resets_at".into(), to_iso(resets_at));
    entry.into()
}

/// The record fields out of the usage API response, in the shape of the
/// rollout record: the account-wide `rate_limit` becomes limit `codex`, a
/// window's length in seconds is rounded up to minutes, and credits keep the
/// three fields the rollout snapshot has. Account and user ids in the
/// response are dropped.
///
/// A body that is not the payload upstream decodes is an error, so the caller
/// falls back instead of reporting an empty quota: `plan_type` is required,
/// and `rate_limit`, its windows and `credits` may be absent or null but must
/// otherwise have the fields upstream requires of them.
fn usage_fields(payload: &Value) -> Result<Vec<(&'static str, Value)>, Error> {
    let bad = || usage("unexpected /wham/usage response shape");
    // An optional object: absent and null are both "not there".
    let optional = |parent: &Value, key: &str| match parent.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(object @ Value::Object(_)) => Ok(Some(object.clone())),
        Some(_) => Err(bad()),
    };
    let plan_type = payload.get("plan_type").filter(|plan| plan.is_string());
    let plan_type = plan_type.ok_or_else(bad)?.clone();

    let mut windows = Map::new();
    if let Some(rate_limit) = optional(payload, "rate_limit")? {
        for (slot, key) in [
            ("primary", "primary_window"),
            ("secondary", "secondary_window"),
        ] {
            let Some(window) = optional(&rate_limit, key)? else {
                continue;
            };
            let number = |name: &str| window.get(name).filter(|value| value.is_number());
            let used_percent = number("used_percent").and_then(Value::as_f64);
            let seconds = number("limit_window_seconds").and_then(Value::as_i64);
            let reset_at = number("reset_at").ok_or_else(bad)?;
            let minutes = match seconds.ok_or_else(bad)? {
                seconds if seconds > 0 => Value::from((seconds + 59) / 60),
                _ => Value::Null,
            };
            let used_percent = used_percent.ok_or_else(bad)?.into();
            let entry = window_entry(slot, used_percent, minutes.clone(), reset_at);
            windows.insert(window_name(&minutes), entry);
        }
    }
    let credits = match optional(payload, "credits")? {
        Some(credits) => {
            let flag = |name: &str| credits.get(name).filter(|value| value.is_boolean());
            let mut kept = Map::new();
            kept.insert(
                "has_credits".into(),
                flag("has_credits").ok_or_else(bad)?.clone(),
            );
            kept.insert(
                "unlimited".into(),
                flag("unlimited").ok_or_else(bad)?.clone(),
            );
            kept.insert("balance".into(), get(&credits, "balance"));
            kept.into()
        }
        None => Value::Null,
    };
    Ok(vec![
        ("plan_type", plan_type),
        ("limit_id", "codex".into()),
        ("windows", windows.into()),
        ("credits", credits),
    ])
}

fn api_quota(credentials: &Credentials) -> Result<Record, Error> {
    let version = cli_version("codex")?;
    let os = os_info::get();
    let user_agent = user_agent(
        &version,
        &os.os_type().to_string(),
        &os.version().to_string(),
        os.architecture().unwrap_or("unknown"),
        &terminal_token(&|name| std::env::var(name).ok()),
    );
    let headers = build_headers(
        &credentials.token,
        credentials.account_id.as_deref(),
        &user_agent,
    );
    let payload = http_get_json(USAGE_URL, &headers, TIMEOUT)?;
    let mut fields = usage_fields(&payload)?;
    fields.push(("credential_source", credentials.source.clone().into()));
    Ok(record("codex", "usage_api", None, fields))
}

/// Rollout files, newest first.
fn rollout_files(sessions: &Path) -> Vec<PathBuf> {
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.path()))
            .collect()
    };
    // sessions/<year>/<month>/<day>/rollout-*.jsonl
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = children(sessions)
        .iter()
        .flat_map(|year| children(year))
        .flat_map(|month| children(&month))
        .flat_map(|day| children(&day))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        })
        .filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path)))
        .collect();
    files.sort_by(|a, b| b.cmp(a));
    files.into_iter().map(|(_, path)| path).collect()
}

const CHUNK: u64 = 64 * 1024;

/// The last line in `source` for which `pick` returns a value, reading
/// backwards in chunks so a large file is not read past the line wanted.
fn last_matching_line<S: Read + Seek, T>(
    mut source: S,
    pick: impl Fn(&[u8]) -> Option<T>,
) -> std::io::Result<Option<T>> {
    let mut position = source.seek(SeekFrom::End(0))?;
    // The start of a line whose beginning lies in a chunk not read yet.
    let mut partial: Vec<u8> = Vec::new();
    while position > 0 {
        let length = CHUNK.min(position);
        position -= length;
        source.seek(SeekFrom::Start(position))?;
        let mut buffer = vec![0; length as usize];
        source.read_exact(&mut buffer)?;
        let fresh = buffer.len();
        buffer.extend_from_slice(&partial);
        let mut end = buffer.len();
        // `partial` holds no newline, so only the fresh bytes are searched.
        while let Some(newline) = buffer[..end.min(fresh)].iter().rposition(|b| *b == b'\n') {
            if let Some(found) = pick(&buffer[newline + 1..end]) {
                return Ok(Some(found));
            }
            end = newline;
        }
        buffer.truncate(end);
        partial = buffer;
    }
    Ok(pick(&partial))
}

/// A `token_count` event that carries rate limits, or nothing.
fn snapshot_row(line: &[u8]) -> Option<Value> {
    let row: Value = serde_json::from_slice(line).ok()?;
    let payload = row.get("payload")?;
    let wanted = row.get("type")? == "event_msg"
        && payload.get("type")? == "token_count"
        && payload.get("rate_limits")?.is_object();
    wanted.then_some(row)
}

/// The timestamp Codex wrote and the record fields of one snapshot row.
fn snapshot_fields(row: &Value) -> (Option<String>, Vec<(&'static str, Value)>) {
    let limits = &row["payload"]["rate_limits"];
    let mut windows = Map::new();
    for slot in ["primary", "secondary"] {
        if let Some(window @ Value::Object(_)) = limits.get(slot) {
            let minutes = get(window, "window_minutes");
            let entry = window_entry(
                slot,
                get(window, "used_percent"),
                minutes.clone(),
                &get(window, "resets_at"),
            );
            windows.insert(window_name(&minutes), entry);
        }
    }
    let ts = to_iso(&get(row, "timestamp")).as_str().map(str::to_owned);
    let fields = vec![
        ("plan_type", get(limits, "plan_type")),
        ("limit_id", get(limits, "limit_id")),
        ("windows", windows.into()),
        ("credits", get(limits, "credits")),
    ];
    (ts, fields)
}

fn rollout_quota(sessions: &Path) -> Result<Record, Error> {
    for path in rollout_files(sessions) {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        if let Ok(Some(row)) = last_matching_line(file, snapshot_row) {
            let (ts, fields) = snapshot_fields(&row);
            return Ok(record("codex", "local_rollout", ts, fields));
        }
    }
    Err(usage(format!(
        "no rate-limit snapshot found under {}",
        sessions.display()
    )))
}

pub fn quota(ctx: &Context) -> Result<Record, Error> {
    let dir = ctx.harness_dir(Harness::Codex);
    let credentials = match env_credentials(ctx)? {
        Some(credentials) => Ok(credentials),
        None => dir
            .as_ref()
            .map_err(|err| usage(err.to_string()))
            .and_then(|dir| file_credentials(dir)),
    };
    let reason = match credentials.and_then(|credentials| api_quota(&credentials)) {
        Ok(record) => return Ok(record),
        Err(Error::Usage(reason)) => reason,
        Err(rate_limited) => return Err(rate_limited),
    };
    eprintln!("note: usage API gave no answer ({reason}); using the local rollout snapshot");
    rollout_quota(&dir?.join("sessions"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;
    use crate::common::test_context;

    const FAKE_TOKEN: &str = "synthetic-token-not-a-credential";

    fn object(fields: Vec<(&'static str, Value)>) -> Value {
        Value::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    #[test]
    fn headers_match_the_backend_client() {
        let token = Secret::new(FAKE_TOKEN.into());
        let agent = user_agent("0.153.0", "Ubuntu", "24.4.0", "x86_64", "xterm-256color");
        assert_eq!(
            agent,
            "codex_cli_rs/0.153.0 (Ubuntu 24.4.0; x86_64) xterm-256color"
        );
        assert_eq!(
            build_headers(&token, Some("account-123"), &agent).0,
            vec![
                ("Authorization", format!("Bearer {FAKE_TOKEN}")),
                ("ChatGPT-Account-Id", "account-123".to_owned()),
                ("User-Agent", agent.clone()),
            ]
        );
        let names: Vec<_> = build_headers(&token, None, &agent)
            .0
            .into_iter()
            .map(|h| h.0)
            .collect();
        assert_eq!(names, ["Authorization", "User-Agent"]);
    }

    #[test]
    fn terminal_token_follows_upstream_detection() {
        let token = |vars: &'static [(&str, &str)]| {
            terminal_token(&|name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
        };
        assert_eq!(token(&[("TERM", "xterm-256color")]), "xterm-256color");
        assert_eq!(
            token(&[
                ("TERM_PROGRAM", "vscode"),
                ("TERM_PROGRAM_VERSION", "1.99.0"),
                ("TERM", "xterm")
            ]),
            "vscode/1.99.0"
        );
        // tmux names itself in TERM_PROGRAM; the terminal underneath is what counts.
        assert_eq!(
            token(&[("TERM_PROGRAM", "tmux"), ("TERM", "tmux-256color")]),
            "tmux-256color"
        );
        assert_eq!(
            token(&[("TERM_PROGRAM", "Apple Terminal (x)")]),
            "Apple_Terminal__x_"
        );
        assert_eq!(
            token(&[("WEZTERM_VERSION", "2026.01"), ("TERM", "xterm")]),
            "WezTerm/2026.01"
        );
        assert_eq!(token(&[]), "unknown");
    }

    #[test]
    fn usage_response_maps_into_the_rollout_shape() {
        let payload = json!({
            "plan_type": "prolite",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 42,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 1234,
                    "reset_at": 1788747923,
                },
                "secondary_window": {
                    "used_percent": 30,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 99,
                    "reset_at": 1788747923,
                },
            },
            "credits": {"has_credits": false, "unlimited": false, "balance": "0", "approx_local_messages": [1, 2]},
            "rate_limit_reset_credits": {"available_count": 3},
            "account_id": "account-123",
            "user_id": "user-123",
        });
        assert_eq!(
            object(usage_fields(&payload).unwrap()),
            json!({
                "plan_type": "prolite",
                "limit_id": "codex",
                "windows": {
                    "five_hour": {
                        "slot": "primary",
                        "used_percent": 42.0,
                        "window_minutes": 300,
                        "resets_at": "2026-09-07T02:25:23Z",
                    },
                    "seven_day": {
                        "slot": "secondary",
                        "used_percent": 30.0,
                        "window_minutes": 10080,
                        "resets_at": "2026-09-07T02:25:23Z",
                    },
                },
                "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            })
        );
    }

    #[test]
    fn usage_window_of_another_length_keeps_its_minutes() {
        let payload = json!({"plan_type": "plus", "rate_limit": {"primary_window": {
            "used_percent": 1, "limit_window_seconds": 86390, "reset_at": 1788747923,
        }}});
        let fields = object(usage_fields(&payload).unwrap());
        assert_eq!(fields["windows"]["window_1440m"]["window_minutes"], 1440);
        assert_eq!(fields["credits"], Value::Null);
    }

    #[test]
    fn usage_response_of_another_shape_is_rejected() {
        // A 2xx body that is JSON but not the usage payload must not pass as
        // an empty quota; the error is what sends the caller to the fallback.
        for payload in [
            json!({}),
            json!({"plan_type": "pro", "rate_limit": "none"}),
            json!({"plan_type": "pro", "rate_limit": {"primary_window": {"used_percent": 1}}}),
            json!({"plan_type": "pro", "credits": {"has_credits": "yes", "unlimited": false}}),
        ] {
            assert!(usage_fields(&payload).is_err(), "{payload}");
        }
        // The optional parts may be null or absent.
        let minimal = json!({"plan_type": "pro", "rate_limit": null});
        let fields = object(usage_fields(&minimal).unwrap());
        assert_eq!(fields["windows"], json!({}));
    }

    #[test]
    fn snapshot_row_maps_like_the_original_tool() {
        let row = json!({
            "timestamp": "2026-08-31T11:00:00.000Z",
            "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": {
                "limit_id": "codex",
                "plan_type": "prolite",
                "primary": {"used_percent": 42.5, "window_minutes": 300, "resets_at": 1788747923},
                "secondary": null,
                "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            }},
        });
        let (ts, fields) = snapshot_fields(&row);
        assert_eq!(ts.as_deref(), Some("2026-08-31T11:00:00Z"));
        assert_eq!(
            object(fields),
            json!({
                "plan_type": "prolite",
                "limit_id": "codex",
                "windows": {"five_hour": {
                    "slot": "primary",
                    "used_percent": 42.5,
                    "window_minutes": 300,
                    "resets_at": "2026-09-07T02:25:23Z",
                }},
                "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            })
        );
    }

    #[test]
    fn reads_the_last_matching_line_across_chunk_boundaries() {
        let pick = |line: &[u8]| {
            line.starts_with(b"hit")
                .then(|| String::from_utf8_lossy(line).into_owned())
        };
        let filler = "x".repeat(CHUNK as usize + 17);
        let text = format!("hit 1\n{filler}\nhit 2 {filler}\nmiss\n\n");
        let found = last_matching_line(Cursor::new(text), pick).unwrap();
        assert_eq!(found, Some(format!("hit 2 {filler}")));
        assert_eq!(
            last_matching_line(Cursor::new("hit 0\nmiss"), pick)
                .unwrap()
                .as_deref(),
            Some("hit 0")
        );
        assert_eq!(last_matching_line(Cursor::new(""), pick).unwrap(), None);
    }

    #[test]
    fn token_without_account_id_is_an_error_naming_the_variable() {
        let ctx = test_context(None, &[(TOKEN_VAR, FAKE_TOKEN)]);
        let message = env_credentials(&ctx).err().unwrap().to_string();
        assert!(message.contains(ACCOUNT_ID_VAR), "{message}");
        assert!(!message.contains(FAKE_TOKEN), "{message}");
    }
}
