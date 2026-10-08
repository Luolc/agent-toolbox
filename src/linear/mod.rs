//! `atb linear <command>`: claim and release Linear issues by comment, create
//! issues for agents, and run read-only GraphQL queries.

mod document;
mod key;
mod states;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::common::{Context, Error, Headers, http_post_json, usage};
use key::Key;
use states::{BACKLOG, COMPLETED, Overrides, STARTED, State, UNSTARTED};

const API_URL: &str = "https://api.linear.app/graphql";
const TIMEOUT: Duration = Duration::from_secs(30);
const LABEL: &str = "agent";

pub const EXIT_CLAIM_LOST: u8 = 3;
pub const EXIT_RELEASE_REFUSED: u8 = 4;

const AFTER_HELP: &str = "\
Key sources, highest first:
  1. LINEAR_API_KEY: the key itself. A 401 is an error; there is nothing to
     refetch. There is no flag for the key.
  2. LINEAR_API_KEY_CMD: a command run with `sh -c` that prints the key, such
     as a secret manager's read. Its trimmed stdout is cached for 24 hours in
     $XDG_CONFIG_HOME/linear/api-key (default <home>/.config/linear/api-key,
     where ATB_HOME overrides the home), mode 0600 in a 0700 directory. Its
     stderr is discarded. On a 401 the command runs once more and the request
     is retried once; a second 401 is an error.
An empty variable counts as unset. LINEAR_API_URL overrides the endpoint.

States are chosen by type: the first `started` state (lowest position, then
name) for claim, the first `completed` for release --done. The optional
$XDG_CONFIG_HOME/linear/config.json, {\"states\": {\"started\": \"<name>\", ...}},
names the state to use for a type instead.

Exit status: 0 on success, 1 on error, 2 on a usage error or when Linear
answers 429 (the retry-after value is printed; nothing is retried), 3 when a
claim lost to an earlier claim, 4 when a release was refused (no holder, or
the holder is another agent).";

#[derive(clap::Args)]
#[command(after_help = AFTER_HELP)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Set the issue to the first started state and write a claim comment
    /// that records the prior state; exit 3 if an earlier claim by another
    /// agent is still held
    Claim {
        /// Issue identifier, such as ABC-123
        issue: String,
        #[arg(long)]
        agent: String,
        /// Where the work came from: a thread key or the dispatching agent
        #[arg(long)]
        source: String,
        /// What will be touched, as `<repo>: <paths>`
        #[arg(long)]
        scope: String,
    },
    /// Write a release comment for the current holder, then restore the
    /// state from before the claim, or set the first completed state with
    /// --done; exit 4 if refused
    #[command(group = clap::ArgGroup::new("why").required(true))]
    Release {
        /// Issue identifier, such as ABC-123
        issue: String,
        #[arg(long)]
        agent: String,
        /// Why the holder (--agent) releases, such as `merged`
        #[arg(long, group = "why")]
        reason: Option<String>,
        /// Release another agent's claim, giving why; the comment names the
        /// holder and --agent
        #[arg(long, group = "why", value_name = "WHY")]
        force: Option<String>,
        /// Set the first completed state instead of restoring the prior one
        #[arg(long, conflicts_with = "todo")]
        done: bool,
        /// Deprecated, accepted for 0.2.0 callers: does nothing, restoring
        /// the prior state is the default
        #[arg(long)]
        todo: bool,
    },
    /// Run a read-only GraphQL query and print its `data` as JSON; mutations
    /// and subscriptions are refused before anything is sent
    Query {
        /// A file holding the query, or the query text itself
        graphql: String,
    },
    /// Create an issue labelled `agent` (the label is created if the team
    /// has none) and print its identifier and URL
    Create {
        /// Team key, such as TEAM
        #[arg(long)]
        team: String,
        /// Project name within the team
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long, value_name = "FILE")]
        description_file: PathBuf,
        /// Print the identifier and URL as JSON
        #[arg(long)]
        json: bool,
    },
}

/// The exit status on success or on a protocol outcome (3, 4).
pub fn run(args: Args) -> Result<u8, Error> {
    let ctx = Context::from_process(None);
    // The query is checked before the key is read: a refused write must not
    // even run the key command.
    let query = match &args.command {
        Command::Query { graphql } => Some(read_only_query(graphql)?),
        _ => None,
    };
    let overrides = match &args.command {
        Command::Claim { .. } | Command::Release { .. } => {
            Overrides::load(&config_dir(&ctx)?.join("config.json"))?
        }
        _ => Overrides::default(),
    };
    let mut client = Client {
        url: match ctx.var("LINEAR_API_URL") {
            Some(url) => url
                .into_string()
                .map_err(|_| usage("LINEAR_API_URL is not valid UTF-8"))?,
            None => API_URL.to_owned(),
        },
        key: Key::resolve(&ctx)?,
    };
    let result = match args.command {
        Command::Claim {
            issue,
            agent,
            source,
            scope,
        } => claim(&mut client, &overrides, &issue, &agent, &source, &scope),
        Command::Release {
            issue,
            agent,
            reason,
            force,
            done,
            todo: _,
        } => {
            let why = match force {
                Some(why) => Why::Force(why),
                None => Why::Reason(reason.expect("clap requires --reason or --force")),
            };
            release(&mut client, &overrides, &issue, &agent, why, done)
        }
        Command::Query { .. } => run_query(&mut client, &query.expect("read before the key")),
        Command::Create {
            team,
            project,
            title,
            description_file,
            json,
        } => create(
            &mut client,
            &team,
            project.as_deref(),
            &title,
            &description_file,
            json,
        ),
    };
    result.map_err(|err| match err {
        Error::Usage(message) => Error::Usage(client.key.mask(&message)),
        other => other,
    })
}

/// `$XDG_CONFIG_HOME/linear`, else `<home>/.config/linear` with the home
/// that `ATB_HOME` overrides: the key cache and the config file live here.
fn config_dir(ctx: &Context) -> Result<PathBuf, Error> {
    let config = match ctx.var("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => ctx.home()?.join(".config"),
    };
    Ok(config.join("linear"))
}

/// The query text (from the file if `arg` names one), refused unless every
/// operation in it is a query.
fn read_only_query(arg: &str) -> Result<String, Error> {
    let path = Path::new(arg);
    let text = if path.is_file() {
        std::fs::read_to_string(path)
            .map_err(|err| usage(format!("cannot read {}: {err}", path.display())))?
    } else {
        arg.to_owned()
    };
    let definitions = document::definitions(&text)
        .map_err(|why| usage(format!("cannot read the GraphQL document: {why}")))?;
    if definitions.iter().any(|d| {
        matches!(
            d,
            document::Definition::Mutation | document::Definition::Subscription
        )
    }) {
        return Err(usage(
            "query is read-only: the document contains a mutation or subscription; \
             use claim, release or create to write",
        ));
    }
    Ok(text)
}

fn run_query(client: &mut Client, query: &str) -> Result<u8, Error> {
    let data = client.request(query, json!({}))?;
    client.print_json(&data);
    Ok(0)
}

struct Client {
    url: String,
    key: Key,
}

/// Everything `linear` prints goes through these, masked against every key
/// held during the run: Linear's data and the comments it echoes are text
/// this binary does not control.
impl Client {
    fn print(&self, line: &str) {
        println!("{}", self.key.mask(line));
    }

    fn note(&self, line: &str) {
        eprintln!("{}", self.key.mask(line));
    }

    /// JSON masked value by value, so the output stays valid JSON.
    fn print_json(&self, value: &Value) {
        let masked = self.key.mask_json(value);
        println!(
            "{}",
            serde_json::to_string_pretty(&masked).expect("a JSON value always serializes")
        );
    }

    /// One GraphQL request, returning `data`. The only retry anywhere in this
    /// binary: on a 401 with a key from LINEAR_API_KEY_CMD, the key is fetched
    /// again and this request is sent once more.
    fn request(&mut self, query: &str, variables: Value) -> Result<Value, Error> {
        let body = json!({"query": query, "variables": variables});
        let mut response = self.send(&body)?;
        if response.status == 401 && self.key.refresh()? {
            response = self.send(&body)?;
        }
        if response.status == 401 {
            return Err(usage(format!(
                "Linear rejected the API key from {} (HTTP 401)",
                self.key.source_name()
            )));
        }
        if response.status == 429 {
            return Err(Error::RateLimited {
                retry_after: response.retry_after,
            });
        }
        let payload: Option<Value> = serde_json::from_str(&response.body).ok();
        let errors = payload
            .as_ref()
            .and_then(|p| p.get("errors"))
            .and_then(Value::as_array)
            .filter(|errors| !errors.is_empty());
        if let Some(errors) = errors {
            let messages: Vec<&str> = errors
                .iter()
                .map(|e| e.get("message").and_then(Value::as_str).unwrap_or("?"))
                .collect();
            return Err(usage(format!("GraphQL: {}", messages.join("; "))));
        }
        if !(200..300).contains(&response.status) {
            return Err(usage(format!("HTTP {} from {}", response.status, self.url)));
        }
        match payload.and_then(|mut p| p.get_mut("data").map(Value::take)) {
            Some(data @ Value::Object(_)) => Ok(data),
            _ => Err(usage(format!("{} returned no data", self.url))),
        }
    }

    fn send(&self, body: &Value) -> Result<crate::common::Response, Error> {
        let headers = Headers(vec![(
            "Authorization",
            self.key.current().expose().to_owned(),
        )]);
        http_post_json(&self.url, &headers, body, TIMEOUT)
    }
}

/// A JSON string at `pointer`, or an error naming what was expected.
fn text<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, Error> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| usage(format!("unexpected Linear response: no {pointer}")))
}

struct Issue {
    id: String,
    identifier: String,
    /// The name of the issue's current state.
    state: String,
    /// The team's workflow states.
    states: Vec<State>,
}

impl Issue {
    /// The state to use for `kind`; a team without one is an error.
    fn state_of_type(&self, kind: &str, overrides: &Overrides) -> Result<&State, Error> {
        states::pick(&self.states, kind, overrides)?.ok_or_else(|| {
            usage(format!(
                "the team of {} has no {kind} state",
                self.identifier
            ))
        })
    }
}

fn issue_info(client: &mut Client, ident: &str) -> Result<Issue, Error> {
    let data = client.request(
        "query($id: String!) { issue(id: $id) { id identifier state { name } team { states { nodes { id name type position } } } } }",
        json!({"id": ident}),
    )?;
    let issue = &data["issue"];
    if issue.is_null() {
        return Err(usage(format!("no issue {ident}")));
    }
    let states = issue
        .pointer("/team/states/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(State {
                id: s["id"].as_str()?.to_owned(),
                name: s["name"].as_str()?.to_owned(),
                kind: s["type"].as_str()?.to_owned(),
                position: s["position"].as_f64()?,
            })
        })
        .collect();
    Ok(Issue {
        id: text(issue, "/id")?.to_owned(),
        identifier: text(issue, "/identifier")?.to_owned(),
        state: text(issue, "/state/name")?.to_owned(),
        states,
    })
}

fn set_state(client: &mut Client, issue: &Issue, state: &State) -> Result<(), Error> {
    let data = client.request(
        "mutation($id: String!, $s: String!) { issueUpdate(id: $id, input: {stateId: $s}) { success } }",
        json!({"id": issue.id, "s": state.id}),
    )?;
    if data.pointer("/issueUpdate/success") != Some(&Value::Bool(true)) {
        return Err(usage(format!(
            "Linear did not set {} to {}",
            issue.identifier, state.name
        )));
    }
    Ok(())
}

/// Write a comment and return its id.
fn comment(client: &mut Client, issue: &Issue, body: &str) -> Result<String, Error> {
    let data = client.request(
        "mutation($id: String!, $b: String!) { commentCreate(input: {issueId: $id, body: $b}) { success comment { id } } }",
        json!({"id": issue.id, "b": body}),
    )?;
    Ok(text(&data, "/commentCreate/comment/id")?.to_owned())
}

struct Comment {
    id: String,
    created_at: String,
    body: String,
}

/// Every comment on the issue, ordered by `createdAt`, then by `id` so that
/// every reader breaks a tie the same way. Pagination stops when Linear says
/// there is no next page.
fn comments(client: &mut Client, issue: &Issue) -> Result<Vec<Comment>, Error> {
    let mut all = Vec::new();
    let mut after = Value::Null;
    loop {
        let data = client.request(
            "query($id: String!, $after: String) { issue(id: $id) { comments(first: 100, after: $after) { nodes { id body createdAt } pageInfo { hasNextPage endCursor } } } }",
            json!({"id": issue.id, "after": after}),
        )?;
        let page = &data["issue"]["comments"];
        for node in page["nodes"].as_array().into_iter().flatten() {
            all.push(Comment {
                id: text(node, "/id")?.to_owned(),
                created_at: text(node, "/createdAt")?.to_owned(),
                body: text(node, "/body")?.to_owned(),
            });
        }
        if page.pointer("/pageInfo/hasNextPage") != Some(&Value::Bool(true)) {
            break;
        }
        after = Value::from(text(page, "/pageInfo/endCursor")?);
    }
    all.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(all)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Claim,
    Release,
}

/// `claim: <agent> ...` or `release: <agent> ...` on the first line.
fn head(body: &str) -> Option<(Kind, &str)> {
    let mut words = body.lines().next()?.split_whitespace();
    let kind = match words.next()? {
        "claim:" => Kind::Claim,
        "release:" => Kind::Release,
        _ => return None,
    };
    Some((kind, words.next()?))
}

fn released_after(comments: &[Comment], who: &str) -> bool {
    comments
        .iter()
        .any(|c| head(&c.body) == Some((Kind::Release, who)))
}

/// The first claim before `mine` by another agent that no later release by
/// that agent undoes.
fn earlier_claim<'a>(comments: &'a [Comment], mine: &str, agent: &str) -> Option<&'a str> {
    let pos = comments.iter().position(|c| c.id == mine)?;
    comments[..pos].iter().enumerate().find_map(|(i, c)| {
        let (kind, who) = head(&c.body)?;
        (kind == Kind::Claim && who != agent && !released_after(&comments[i + 1..], who))
            .then_some(who)
    })
}

/// The current holder and its claim comment: the latest claim that no later
/// release by the same agent undoes. `release: <holder> lost` and
/// `release: <holder> forced by ...` both count as releases by that holder.
fn holder(comments: &[Comment]) -> Option<(&str, &str)> {
    comments.iter().enumerate().rev().find_map(|(i, c)| {
        let (kind, who) = head(&c.body)?;
        (kind == Kind::Claim && !released_after(&comments[i + 1..], who))
            .then_some((who, c.body.as_str()))
    })
}

/// The state recorded by `from: <name>`, which claim writes as the last line.
/// Only the last line counts, so a scope spanning lines cannot forge it.
fn claimed_from(claim: &str) -> Option<&str> {
    claim.lines().skip(1).last()?.strip_prefix("from: ")
}

fn claim(
    client: &mut Client,
    overrides: &Overrides,
    ident: &str,
    agent: &str,
    source: &str,
    scope: &str,
) -> Result<u8, Error> {
    let issue = issue_info(client, ident)?;
    let started = issue.state_of_type(STARTED, overrides)?;
    set_state(client, &issue, started)?;
    let mine = comment(
        client,
        &issue,
        &format!(
            "claim: {agent} {source}\nscope: {scope}\nfrom: {}",
            issue.state
        ),
    )?;
    let checked = comments(client, &issue).and_then(|all| {
        let winner = earlier_claim(&all, &mine, agent).map(str::to_owned);
        if all.iter().all(|c| c.id != mine) {
            return Err(usage("the claim comment is missing from the read-back"));
        }
        Ok(winner)
    });
    let winner = checked.inspect_err(|_| {
        client.note(&format!(
            "note: the claim comment on {} was written, but the conflict check did not finish",
            issue.identifier
        ));
    })?;
    if let Some(winner) = winner {
        comment(client, &issue, &format!("release: {agent} lost"))?;
        client.note(&format!(
            "{} is held by {winner}, who claimed it first; wrote `release: {agent} lost`",
            issue.identifier
        ));
        return Ok(EXIT_CLAIM_LOST);
    }
    client.print(&format!("claimed {}", issue.identifier));
    Ok(0)
}

enum Why {
    Reason(String),
    Force(String),
}

/// The state a release without --done restores: the one the claim recorded
/// if the team still has it, else the first unstarted state, else the first
/// backlog state. Overrides apply to both fallbacks.
fn prior_state<'a>(
    issue: &'a Issue,
    claim: &str,
    overrides: &Overrides,
) -> Result<&'a State, Error> {
    let recorded = claimed_from(claim);
    if let Some(state) = recorded.and_then(|name| issue.states.iter().find(|s| s.name == name)) {
        return Ok(state);
    }
    for kind in [UNSTARTED, BACKLOG] {
        if let Some(state) = states::pick(&issue.states, kind, overrides)? {
            return Ok(state);
        }
    }
    Err(usage(format!(
        "the team of {} has no state {} and no {UNSTARTED} or {BACKLOG} state to fall back to; \
         the state is unchanged",
        issue.identifier,
        match recorded {
            Some(name) => format!("named {name:?} (recorded by the claim)"),
            None => "recorded by the claim".to_owned(),
        }
    )))
}

fn release(
    client: &mut Client,
    overrides: &Overrides,
    ident: &str,
    agent: &str,
    why: Why,
    done: bool,
) -> Result<u8, Error> {
    let issue = issue_info(client, ident)?;
    let all = comments(client, &issue)?;
    let Some((holder, claim)) = holder(&all) else {
        client.note(&format!(
            "{} has no holder; nothing written, state unchanged",
            issue.identifier
        ));
        return Ok(EXIT_RELEASE_REFUSED);
    };
    let body = match &why {
        Why::Force(why) => format!("release: {holder} forced by {agent}: {why}"),
        Why::Reason(_) if holder != agent => {
            client.note(&format!(
                "{} is held by {holder}, not {agent}; nothing written, state unchanged \
                 (--force releases another agent's claim)",
                issue.identifier
            ));
            return Ok(EXIT_RELEASE_REFUSED);
        }
        Why::Reason(reason) => format!("release: {agent} {reason}"),
    };
    let (holder, claim) = (holder.to_owned(), claim.to_owned());
    comment(client, &issue, &body)?;
    let state = if done {
        issue.state_of_type(COMPLETED, overrides)?
    } else {
        prior_state(&issue, &claim, overrides)?
    };
    set_state(client, &issue, state)?;
    client.print(&format!("released {} (held by {holder})", issue.identifier));
    Ok(0)
}

fn create(
    client: &mut Client,
    team_key: &str,
    project: Option<&str>,
    title: &str,
    description_file: &Path,
    as_json: bool,
) -> Result<u8, Error> {
    let description = std::fs::read_to_string(description_file)
        .map_err(|err| usage(format!("cannot read {}: {err}", description_file.display())))?;
    let data = client.request(
        "query($k: String!) { teams(filter: {key: {eq: $k}}) { nodes { id } } }",
        json!({"k": team_key}),
    )?;
    let team = match data.pointer("/teams/nodes").and_then(Value::as_array) {
        Some(nodes) if !nodes.is_empty() => text(&nodes[0], "/id")?.to_owned(),
        _ => return Err(usage(format!("no team with key {team_key}"))),
    };
    let mut input = json!({"teamId": team, "title": title, "description": description});
    if let Some(name) = project {
        let data = client.request(
            "query($t: String!, $n: String!) { team(id: $t) { projects(filter: {name: {eq: $n}}) { nodes { id } } } }",
            json!({"t": team, "n": name}),
        )?;
        let nodes = data
            .pointer("/team/projects/nodes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let [node] = nodes else {
            return Err(usage(format!(
                "team {team_key} has {} projects named {name:?}; exactly one is needed",
                nodes.len()
            )));
        };
        input["projectId"] = text(node, "/id")?.into();
    }
    input["labelIds"] = json!([agent_label(client, &team)?]);
    let data = client.request(
        "mutation($i: IssueCreateInput!) { issueCreate(input: $i) { success issue { identifier url } } }",
        json!({"i": input}),
    )?;
    let identifier = text(&data, "/issueCreate/issue/identifier")?;
    let url = text(&data, "/issueCreate/issue/url")?;
    if as_json {
        client.print_json(&json!({"identifier": identifier, "url": url}));
    } else {
        client.print(&format!("{identifier} {url}"));
    }
    Ok(0)
}

/// The id of the `agent` label usable in the team: a workspace label or the
/// team's own. Created on the team when there is none.
fn agent_label(client: &mut Client, team: &str) -> Result<String, Error> {
    let data = client.request(
        "query($n: String!) { issueLabels(filter: {name: {eq: $n}}) { nodes { id team { id } } } }",
        json!({"n": LABEL}),
    )?;
    let existing = data
        .pointer("/issueLabels/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|l| l["team"].is_null() || l.pointer("/team/id") == Some(&Value::from(team)));
    if let Some(label) = existing {
        return Ok(text(label, "/id")?.to_owned());
    }
    let data = client.request(
        "mutation($n: String!, $t: String!) { issueLabelCreate(input: {name: $n, teamId: $t}) { success issueLabel { id } } }",
        json!({"n": LABEL, "t": team}),
    )?;
    Ok(text(&data, "/issueLabelCreate/issueLabel/id")?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(bodies: &[&str]) -> Vec<Comment> {
        bodies
            .iter()
            .enumerate()
            .map(|(i, body)| Comment {
                id: format!("c{i}"),
                created_at: format!("2026-10-08T00:00:0{i}Z"),
                body: (*body).to_owned(),
            })
            .collect()
    }

    #[test]
    fn holder_is_the_latest_claim_not_released_by_its_agent() {
        let who = |bodies: &[&str]| holder(&thread(bodies)).map(|(who, _)| who.to_owned());
        assert_eq!(who(&[]), None);
        assert_eq!(who(&["claim: a s\nscope: x"]), Some("a".into()));
        assert_eq!(
            who(&["claim: a s", "claim: b s", "release: b lost"]),
            Some("a".into())
        );
        assert_eq!(who(&["claim: a s", "release: a forced by b: stale"]), None);
        assert_eq!(
            who(&["claim: a s", "release: a done", "claim: b s"]),
            Some("b".into())
        );
    }
}
