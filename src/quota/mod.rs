//! `atb quota <harness>`: the subscription quota the vendor enforces, read
//! with one request (or, for Codex, from a local snapshot when that fails).

mod claude;
mod codex;
mod grok;

use std::path::PathBuf;

use serde_json::Value;

use crate::common::{Context, Error, Harness, Record, dump_json, fmt_cell, render_table};

const AFTER_HELP: &str = "\
Credential sources, highest first:
  1. A token passed in the environment: ATB_CLAUDE_TOKEN; ATB_CODEX_TOKEN with
     ATB_CODEX_ACCOUNT_ID; ATB_GROK_TOKEN with ATB_GROK_USER_ID. When the token
     variable is set, no credential file is read, and for Codex and Grok the
     id variable is required. There is no flag for a token: a flag would show
     up in the process list and the shell history.
  2. --config-dir <DIR>: the harness config directory itself.
  3. The harness's own variable: CLAUDE_CONFIG_DIR, CODEX_HOME or GROK_HOME.
  4. ATB_HOME as a whole-home override: <ATB_HOME>/.claude, .codex or .grok.
  5. The default home: ~/.claude, ~/.codex or ~/.grok.
An empty variable counts as unset. The Codex rollout fallback reads sessions/
under the directory that 2 to 5 resolve to.

Exit status: 0 on success, 1 on error, 2 when the endpoint answers 429 (the
retry-after value is printed; nothing is retried).";

#[derive(clap::Args)]
#[command(after_help = AFTER_HELP)]
pub struct Args {
    /// Whose quota to read
    harness: Harness,
    /// The harness config directory, the one holding `.credentials.json`
    /// (Claude) or `auth.json` (Codex, Grok)
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,
    /// Emit the raw record as JSON
    #[arg(long)]
    json: bool,
}

pub fn run(args: Args) -> Result<(), Error> {
    let ctx = Context::from_process(args.config_dir);
    let record = match args.harness {
        Harness::Claude => claude::quota(&ctx)?,
        Harness::Codex => codex::quota(&ctx)?,
        Harness::Grok => grok::quota(&ctx)?,
    };
    if args.json {
        println!("{}", dump_json(&record));
    } else {
        print_table(args.harness, &record);
    }
    Ok(())
}

fn print_table(harness: Harness, record: &Record) {
    let cell = |key: &str| fmt_cell(record.get(key));
    let mut header = format!(
        "{} quota  ts={}  provenance={}",
        harness.name(),
        cell("ts"),
        cell("provenance")
    );
    if harness == Harness::Codex {
        header += &format!("  plan={}", cell("plan_type"));
    }
    if record.contains_key("credential_source") {
        header += &format!("  credential={}", cell("credential_source"));
    }
    println!("{header}");
    match harness {
        Harness::Claude => {
            print_windows(record, "utilization", "UTILIZATION_PCT");
            if let Some(extra @ Value::Object(_)) = record.get("extra_usage") {
                println!(
                    "\nextra usage: enabled={} utilization={} used_credits={} monthly_limit={}",
                    fmt_cell(extra.get("is_enabled")),
                    fmt_cell(extra.get("utilization")),
                    fmt_cell(extra.get("used_credits")),
                    fmt_cell(extra.get("monthly_limit")),
                );
            }
        }
        Harness::Codex => {
            print_windows(record, "used_percent", "USED_PCT");
            if record.get("provenance") == Some(&Value::from("local_rollout")) {
                println!("\nts is when Codex recorded the snapshot, not when this command ran.");
            }
        }
        Harness::Grok => {
            let period = record.get("period");
            let in_period = |key: &str| fmt_cell(period.and_then(|period| period.get(key)));
            let rows = vec![
                vec!["credit_usage_pct".to_owned(), cell("credit_usage_pct")],
                vec!["period_type".to_owned(), in_period("type")],
                vec!["period_start".to_owned(), in_period("start")],
                vec!["period_end".to_owned(), in_period("end")],
                vec![
                    "prepaid_balance_usd".to_owned(),
                    cell("prepaid_balance_usd"),
                ],
                vec!["on_demand_cap_usd".to_owned(), cell("on_demand_cap_usd")],
                vec!["on_demand_used_usd".to_owned(), cell("on_demand_used_usd")],
                vec!["subscription_tier".to_owned(), cell("subscription_tier")],
            ];
            println!("{}", render_table(&["FIELD", "VALUE"], &rows));
        }
    }
}

fn print_windows(record: &Record, value_key: &str, value_header: &str) {
    let rows: Vec<Vec<String>> = record
        .get("windows")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, window)| {
            vec![
                name.clone(),
                fmt_cell(window.get(value_key)),
                fmt_cell(window.get("resets_at")),
            ]
        })
        .collect();
    println!(
        "{}",
        render_table(&["WINDOW", value_header, "RESETS_AT"], &rows)
    );
}
