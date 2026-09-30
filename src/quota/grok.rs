//! Grok Build quota: one call to the billing endpoint the CLI itself polls.

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::common::{
    Context, Error, Harness, Headers, Record, Secret, cli_version, http_get_json, read_json,
    record, usage,
};
use crate::timefmt::to_iso;

const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const TOKEN_AUTH_HEADER: &str = "xai-grok-cli";
const OAUTH_SCOPE_PREFIX: &str = "https://auth.x.ai::";
/// The timeout the Grok CLI itself uses for this endpoint.
const TIMEOUT: Duration = Duration::from_secs(15);
const TOKEN_VAR: &str = "ATB_GROK_TOKEN";
const USER_ID_VAR: &str = "ATB_GROK_USER_ID";

struct Credentials {
    token: Secret,
    user_id: String,
    /// `env`, or the credential file's path.
    source: String,
}

/// Exactly the headers the Grok CLI sends for an interactive billing read.
fn build_headers(
    token: &Secret,
    user_id: &str,
    version: &str,
    os_name: &str,
    arch: &str,
) -> Headers {
    Headers(vec![
        ("Authorization", format!("Bearer {}", token.expose())),
        ("X-XAI-Token-Auth", TOKEN_AUTH_HEADER.into()),
        ("x-userid", user_id.into()),
        ("x-grok-client-version", version.into()),
        ("x-grok-client-mode", "interactive".into()),
        (
            "User-Agent",
            format!("grok-shell/{version} ({os_name}; {arch})"),
        ),
    ])
}

fn credentials(ctx: &Context) -> Result<Credentials, Error> {
    if let Some(token) = ctx.secret(TOKEN_VAR)? {
        return Ok(Credentials {
            token,
            user_id: ctx.companion(TOKEN_VAR, USER_ID_VAR)?,
            source: "env".into(),
        });
    }
    let path = ctx.harness_dir(Harness::Grok)?.join("auth.json");
    let data = read_json(&path, "Grok credentials")?;
    // The file maps a scope to its entry; the OAuth scope is the one the
    // billing endpoint accepts.
    data.iter()
        .filter(|(scope, _)| scope.starts_with(OAUTH_SCOPE_PREFIX))
        .find_map(|(_, entry)| {
            let token = entry.get("key")?.as_str().filter(|key| !key.is_empty())?;
            let user_id = entry.get("user_id")?.as_str().filter(|id| !id.is_empty())?;
            Some(Credentials {
                token: Secret::new(token.into()),
                user_id: user_id.into(),
                source: path.display().to_string(),
            })
        })
        .ok_or_else(|| {
            usage(format!(
                "no OAuth entry with key and user_id in {}",
                path.display()
            ))
        })
}

/// A `{ "val": <US cents> }` amount in dollars. proto3 JSON drops a zero, so
/// an object without `val` is zero; a 64-bit integer may arrive as a string.
fn dollars(value: Option<&Value>) -> Value {
    let Some(Value::Object(amount)) = value else {
        return Value::Null;
    };
    let cents = match amount.get("val") {
        None => Some(0.0),
        Some(Value::Number(number)) => number.as_f64().map(f64::trunc),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok().map(|cents| cents as f64),
        Some(_) => None,
    };
    cents.map_or(Value::Null, |cents| json!(cents / 100.0))
}

fn fields(payload: &Value) -> Result<Vec<(&'static str, Value)>, Error> {
    let payload = payload
        .as_object()
        .ok_or_else(|| usage("unexpected billing response shape"))?;
    let empty = Map::new();
    let config = payload
        .get("config")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let period = config
        .get("currentPeriod")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let get =
        |object: &Map<String, Value>, key: &str| object.get(key).cloned().unwrap_or(Value::Null);
    Ok(vec![
        ("credit_usage_pct", get(config, "creditUsagePercent")),
        (
            "period",
            json!({
                "type": get(period, "type"),
                "start": to_iso(&get(period, "start")),
                "end": to_iso(&get(period, "end")),
            }),
        ),
        ("prepaid_balance_usd", dollars(config.get("prepaidBalance"))),
        ("on_demand_cap_usd", dollars(config.get("onDemandCap"))),
        ("on_demand_used_usd", dollars(config.get("onDemandUsed"))),
        ("subscription_tier", get(payload, "subscriptionTier")),
    ])
}

pub fn quota(ctx: &Context) -> Result<Record, Error> {
    let credentials = credentials(ctx)?;
    // `std::env::consts` spells both the way the Grok CLI does in its
    // User-Agent: linux / macos / windows and x86_64 / aarch64.
    let headers = build_headers(
        &credentials.token,
        &credentials.user_id,
        &cli_version("grok")?,
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    let payload = http_get_json(BILLING_URL, &headers, TIMEOUT)?;
    let mut fields = fields(&payload)?;
    fields.push(("credential_source", credentials.source.into()));
    Ok(record("grok", "billing_api", None, fields))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_context;

    const FAKE_TOKEN: &str = "synthetic-token-not-a-credential";

    #[test]
    fn headers_match_the_interactive_cli() {
        let token = Secret::new(FAKE_TOKEN.into());
        let headers = build_headers(&token, "user-123", "1.0.13", "linux", "x86_64");
        assert_eq!(
            headers.0,
            vec![
                ("Authorization", format!("Bearer {FAKE_TOKEN}")),
                ("X-XAI-Token-Auth", "xai-grok-cli".to_owned()),
                ("x-userid", "user-123".to_owned()),
                ("x-grok-client-version", "1.0.13".to_owned()),
                ("x-grok-client-mode", "interactive".to_owned()),
                ("User-Agent", "grok-shell/1.0.13 (linux; x86_64)".to_owned()),
            ]
        );
    }

    #[test]
    fn maps_the_billing_response() {
        let payload = json!({
            "config": {
                "creditUsagePercent": 42.5,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-06-01T00:00:00Z",
                    "end": "2026-06-08T00:00:00+00:00",
                },
                "prepaidBalance": {"val": 1250},
                "onDemandCap": {"val": "5000"},
                "onDemandUsed": {},
            },
            "subscriptionTier": "SuperGrok Heavy",
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
                "credit_usage_pct": 42.5,
                "period": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-06-01T00:00:00Z",
                    "end": "2026-06-08T00:00:00Z",
                },
                "prepaid_balance_usd": 12.5,
                "on_demand_cap_usd": 50.0,
                "on_demand_used_usd": 0.0,
                "subscription_tier": "SuperGrok Heavy",
            })
        );
    }

    #[test]
    fn token_and_user_id_variables_replace_the_file() {
        let vars = &[(TOKEN_VAR, FAKE_TOKEN), (USER_ID_VAR, "user-123")];
        let credentials = credentials(&test_context(Some("/nonexistent/atb-test"), vars)).unwrap();
        assert_eq!(credentials.token.expose(), FAKE_TOKEN);
        assert_eq!(credentials.user_id, "user-123");
        assert_eq!(credentials.source, "env");
    }
}
