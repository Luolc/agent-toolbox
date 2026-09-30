//! Claude Code quota: one `GET /api/oauth/usage`, the request `/usage` makes.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::common::{
    Context, Error, Harness, Headers, Record, Secret, cli_version, http_get_json, read_json,
    record, usage,
};
use crate::timefmt::to_iso;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// The timeout Claude Code itself uses for this endpoint.
const TIMEOUT: Duration = Duration::from_secs(5);
const WINDOWS: [&str; 4] = [
    "five_hour",
    "seven_day",
    "seven_day_opus",
    "seven_day_sonnet",
];
const TOKEN_VAR: &str = "ATB_CLAUDE_TOKEN";

/// Exactly the headers Claude Code sends for `/api/oauth/usage`.
fn build_headers(token: &Secret, version: &str) -> Headers {
    Headers(vec![
        ("Authorization", format!("Bearer {}", token.expose())),
        ("anthropic-beta", OAUTH_BETA.into()),
        ("Content-Type", "application/json".into()),
        ("User-Agent", format!("claude-code/{version}")),
    ])
}

/// The token and where it came from: `env`, or the credential file's path.
fn credentials(ctx: &Context) -> Result<(Secret, String), Error> {
    if let Some(token) = ctx.secret(TOKEN_VAR)? {
        return Ok((token, "env".into()));
    }
    let path = ctx.harness_dir(Harness::Claude)?.join(".credentials.json");
    let data = read_json(&path, "Claude credentials")?;
    match data
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
    {
        Some(Value::String(token)) if !token.is_empty() => {
            Ok((Secret::new(token.clone()), path.display().to_string()))
        }
        _ => Err(usage(format!(
            "no claudeAiOauth.accessToken in {}",
            path.display()
        ))),
    }
}

/// The record fields out of the endpoint's response. Absent and null windows
/// are left out; `extra_usage` is passed through.
fn fields(payload: &Value) -> Result<Vec<(&'static str, Value)>, Error> {
    let payload = payload
        .as_object()
        .ok_or_else(|| usage("unexpected /api/oauth/usage response shape"))?;
    let mut windows = Map::new();
    for name in WINDOWS {
        if let Some(Value::Object(window)) = payload.get(name) {
            let mut entry = Map::new();
            let utilization = window.get("utilization").cloned().unwrap_or(Value::Null);
            entry.insert("utilization".into(), utilization);
            let resets_at = to_iso(window.get("resets_at").unwrap_or(&Value::Null));
            entry.insert("resets_at".into(), resets_at);
            windows.insert(name.into(), entry.into());
        }
    }
    let extra = match payload.get("extra_usage") {
        Some(extra @ Value::Object(_)) => extra.clone(),
        _ => Value::Null,
    };
    Ok(vec![("windows", windows.into()), ("extra_usage", extra)])
}

pub fn quota(ctx: &Context) -> Result<Record, Error> {
    let (token, source) = credentials(ctx)?;
    let headers = build_headers(&token, &cli_version("claude")?);
    let payload = http_get_json(USAGE_URL, &headers, TIMEOUT)?;
    let mut fields = fields(&payload)?;
    fields.push(("credential_source", source.into()));
    Ok(record("claude", "oauth_usage", None, fields))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::common::test_context;

    const FAKE_TOKEN: &str = "synthetic-token-not-a-credential";

    #[test]
    fn headers_match_the_oauth_usage_client() {
        let headers = build_headers(&Secret::new(FAKE_TOKEN.into()), "2.1.252");
        assert_eq!(
            headers.0,
            vec![
                ("Authorization", format!("Bearer {FAKE_TOKEN}")),
                ("anthropic-beta", "oauth-2025-04-20".to_owned()),
                ("Content-Type", "application/json".to_owned()),
                ("User-Agent", "claude-code/2.1.252".to_owned()),
            ]
        );
    }

    #[test]
    fn maps_windows_and_passes_extra_usage_through() {
        let payload = json!({
            "five_hour": {"utilization": 12.0, "resets_at": "2026-09-01T05:00:00.123456+00:00"},
            "seven_day": {"utilization": 40, "resets_at": null},
            "seven_day_oauth_apps": {"utilization": 1.0, "resets_at": null},
            "seven_day_opus": null,
            "extra_usage": {"is_enabled": false, "monthly_limit": null},
        });
        assert_eq!(
            Value::Object(
                fields(&payload)
                    .unwrap()
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v))
                    .collect()
            ),
            json!({
                "windows": {
                    "five_hour": {"utilization": 12.0, "resets_at": "2026-09-01T05:00:00Z"},
                    "seven_day": {"utilization": 40, "resets_at": null},
                },
                "extra_usage": {"is_enabled": false, "monthly_limit": null},
            })
        );
    }

    #[test]
    fn token_variable_wins_over_the_flag_and_reads_no_file() {
        // The flag points at a directory that does not exist: a file read
        // would fail, so success shows that none happened.
        let ctx = test_context(Some("/nonexistent/atb-test"), &[(TOKEN_VAR, FAKE_TOKEN)]);
        let (token, source) = credentials(&ctx).unwrap();
        assert_eq!(token.expose(), FAKE_TOKEN);
        assert_eq!(source, "env");
    }
}
