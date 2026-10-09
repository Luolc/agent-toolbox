//! `atb linear <command>`: claim and release Linear issues by comment, write
//! comments, create issues and projects for agents, put an issue into a
//! project, relate two issues, and run read-only GraphQL queries.

mod config;
mod document;
mod key;
mod states;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::common::{Context, Error, Headers, http_post_json, usage};
use config::Config;
use key::Key;
use states::{ABANDONED, BACKLOG, CANCELED, COMPLETED, STARTED, State, UNSTARTED};

const API_URL: &str = "https://api.linear.app/graphql";
const TIMEOUT: Duration = Duration::from_secs(30);

pub const EXIT_CLAIM_LOST: u8 = 3;
pub const EXIT_RELEASE_REFUSED: u8 = 4;
/// A comment body refused before the key is read, as for a usage error.
const EXIT_BODY_REFUSED: u8 = 2;
/// `relate` given one issue twice, as for a usage error.
const EXIT_SAME_ISSUE: u8 = 2;

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
config file $XDG_CONFIG_HOME/linear/config.json, beside the key cache, holds
{\"states\": {\"<type>\": \"<state name>\", ...}, \"default_labels\": [...]}:
the state to use for a type instead of the first, and labels create adds.
release --abandon sets the state named by \"states\": {\"abandoned\": ...},
which must be of type `canceled`; without that entry it is an error.

Exit status: 0 on success, 1 on error, 2 on a usage error (including a
comment body that is empty or starts with `claim:` or `release:`, and relate
given the same issue twice) or when
Linear answers 429 (the retry-after value is printed; nothing is retried), 3
when a claim lost to an earlier claim, 4 when a release was refused (no
holder, or the holder is another agent). A release run again after it wrote
its comment but failed to set the state writes no second comment and only
sets the state, if the issue has no holder, this agent's release is the
latest claim or release comment and the state is still `started`.";

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
    /// state from before the claim, set the first completed state with
    /// --done, or the configured abandoned state with --abandon; exit 4 if
    /// refused
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
        /// End the attempt as abandoned: the comment reads `release: <agent>
        /// abandoned: <reason>` (with --force, as for any forced release),
        /// then the state named by `states.abandoned` in the config, which
        /// must be of type canceled, is set. Without that entry, or if the
        /// state is missing or of another type, nothing is written
        #[arg(long, conflicts_with = "done")]
        abandon: bool,
        /// Deprecated, accepted for 0.2.0 callers: does nothing, restoring
        /// the prior state is the default
        #[arg(long)]
        todo: bool,
    },
    /// Write a comment with the file's content, verbatim, and print its URL.
    /// A body that is empty or whose first line starts with `claim:` or
    /// `release:` is refused (exit 2) before anything is sent: those come
    /// only from claim and release
    Comment {
        /// Issue identifier, such as ABC-123
        issue: String,
        /// Markdown for the comment, sent as it is
        #[arg(long, value_name = "FILE")]
        body_file: PathBuf,
        /// Print the comment's id and URL as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run a read-only GraphQL query and print its `data` as JSON; mutations
    /// and subscriptions are refused before anything is sent
    Query {
        /// A file holding the query, or the query text itself
        graphql: String,
    },
    /// Create an issue with the labels from --label and the config's
    /// default_labels (a missing label is created on the team), optionally as
    /// a sub-issue of --parent, and print its identifier and URL
    Create(CreateArgs),
    /// Put an issue that is in no project into the project of its team
    /// named exactly --project, and print the identifier, project name and
    /// URL. Already in that project: printed, nothing written. In another
    /// project, or no single unarchived project of the team with the name:
    /// exit 1, nothing written
    SetProject {
        /// Issue identifier, such as ABC-123
        issue: String,
        /// Project name, matched exactly (case-sensitive) against the
        /// unarchived projects of the issue's team
        #[arg(long, value_name = "NAME")]
        project: String,
        /// Print identifier, project, URL and whether it changed as JSON
        #[arg(long)]
        json: bool,
    },
    /// Add a `related` relation between two issues and print both
    /// identifiers. A relation of any type, in either direction, already
    /// between them: printed, nothing written. The same issue twice: exit 2
    Relate {
        /// Issue identifier, such as ABC-123
        issue: String,
        /// The other issue, such as ABC-124
        other: String,
        /// Print both identifiers, the relation type and whether it was
        /// created as JSON
        #[arg(long)]
        json: bool,
    },
    /// Projects: create
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
}

#[derive(clap::Args)]
struct CreateArgs {
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
    /// A label to add; repeatable
    #[arg(long = "label", value_name = "NAME")]
    labels: Vec<String>,
    /// Create the issue as a sub-issue of this one, such as ABC-123; the
    /// parent may be on another team than --team
    #[arg(long, value_name = "ISSUE")]
    parent: Option<String>,
    /// Print the identifier and URL as JSON
    #[arg(long)]
    json: bool,
}

#[derive(clap::Subcommand)]
enum ProjectCommand {
    /// Create a project on the team and print its name and URL. A project
    /// with exactly this name that is already on the team is printed
    /// instead, without a change; one that is archived or not on the team,
    /// or several with the name (archived ones included), is an error and
    /// nothing is written. Projects in the trash do not count
    Create {
        /// Team key, such as TEAM
        #[arg(long)]
        team: String,
        /// Project name, matched exactly (case-sensitive) against existing
        /// projects
        #[arg(long)]
        name: String,
        /// Markdown for the project's content (the long body, not the short
        /// description)
        #[arg(long, value_name = "FILE")]
        description_file: Option<PathBuf>,
        /// Print name, URL, id and whether it was created as JSON
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
    // Likewise a comment body: read and checked before the key.
    let body = match &args.command {
        Command::Comment { body_file, .. } => match comment_body(body_file)? {
            Ok(body) => Some(body),
            Err(why) => {
                eprintln!("error: {why}; nothing written");
                return Ok(EXIT_BODY_REFUSED);
            }
        },
        _ => None,
    };
    if let Command::Relate { issue, other, .. } = &args.command
        && issue.eq_ignore_ascii_case(other)
    {
        eprintln!("error: relate needs two different issues, got {issue} twice; nothing sent");
        return Ok(EXIT_SAME_ISSUE);
    }
    let config = match &args.command {
        Command::Query { .. } => Config::default(),
        _ => Config::load(&config_dir(&ctx)?.join("config.json"))?,
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
        } => claim(&mut client, &config, &issue, &agent, &source, &scope),
        Command::Release {
            issue,
            agent,
            reason,
            force,
            done,
            abandon,
            todo: _,
        } => {
            let why = match force {
                Some(why) => Why::Force(why),
                None => Why::Reason(reason.expect("clap requires --reason or --force")),
            };
            let end = match (done, abandon) {
                (true, _) => End::Done,
                (_, true) => End::Abandon,
                _ => End::Restore,
            };
            release(&mut client, &config, &issue, &agent, why, end)
        }
        Command::Comment { issue, json, .. } => write_comment(
            &mut client,
            &issue,
            &body.expect("read before the key"),
            json,
        ),
        Command::Query { .. } => run_query(&mut client, &query.expect("read before the key")),
        Command::Create(mut args) => {
            args.labels.extend(config.default_labels);
            let mut seen = std::collections::HashSet::new();
            args.labels.retain(|label| seen.insert(label.clone()));
            create(&mut client, &args)
        }
        Command::SetProject {
            issue,
            project,
            json,
        } => set_project(&mut client, &issue, &project, json),
        Command::Relate { issue, other, json } => relate(&mut client, &issue, &other, json),
        Command::Project {
            command:
                ProjectCommand::Create {
                    team,
                    name,
                    description_file,
                    json,
                },
        } => create_project(&mut client, &team, &name, description_file.as_deref(), json),
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

/// The file's content, or why it is refused as a comment body. An unreadable
/// file is an error. The check looks past leading whitespace, since a server
/// that trims the body would otherwise turn it into a claim or release.
fn comment_body(path: &Path) -> Result<Result<String, String>, Error> {
    let body = std::fs::read_to_string(path)
        .map_err(|err| usage(format!("cannot read {}: {err}", path.display())))?;
    let start = body.trim_start();
    if start.is_empty() {
        return Ok(Err(format!("{} is empty", path.display())));
    }
    if let Some(word) = ["claim:", "release:"]
        .into_iter()
        .find(|word| start.starts_with(word))
    {
        return Ok(Err(format!(
            "the comment starts with `{word}`; only claim and release write those"
        )));
    }
    Ok(Ok(body))
}

fn write_comment(client: &mut Client, ident: &str, body: &str, as_json: bool) -> Result<u8, Error> {
    let issue = issue_info(client, ident)?;
    let data = client.request(
        "mutation($id: String!, $b: String!) { commentCreate(input: {issueId: $id, body: $b}) { success comment { id url } } }",
        json!({"id": issue.id, "b": body}),
    )?;
    let id = text(&data, "/commentCreate/comment/id")?;
    let url = text(&data, "/commentCreate/comment/url")?;
    if as_json {
        client.print_json(&json!({"id": id, "url": url}));
    } else {
        client.print(url);
    }
    Ok(0)
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
    /// The type of the issue's current state.
    state_kind: String,
    /// The team's workflow states.
    states: Vec<State>,
}

impl Issue {
    /// The state to use for `kind`; a team without one is an error.
    fn state_of_type(&self, kind: &str, config: &Config) -> Result<&State, Error> {
        states::pick(&self.states, kind, &config.states)?.ok_or_else(|| {
            usage(format!(
                "the team of {} has no {kind} state",
                self.identifier
            ))
        })
    }
}

fn issue_info(client: &mut Client, ident: &str) -> Result<Issue, Error> {
    let data = client.request(
        "query($id: String!) { issue(id: $id) { id identifier state { name type } team { states { nodes { id name type position } } } } }",
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
        state_kind: text(issue, "/state/type")?.to_owned(),
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

/// The agent after `forced by` in `release: <holder> forced by <agent>: ...`.
fn forced_by(body: &str) -> Option<&str> {
    let mut words = body.lines().next()?.split_whitespace().skip(2);
    if (words.next()?, words.next()?) != ("forced", "by") {
        return None;
    }
    words.next()?.strip_suffix(':')
}

/// The holder and claim of an interrupted release that `agent` can finish:
/// the latest claim or release comment is a release this run would have
/// written (`release: <agent> ...` for --reason, `release: <holder> forced by
/// <agent>: ...` for --force), and the claim is the one it undid. Only asked
/// when the issue has no holder.
fn resumable<'a>(comments: &'a [Comment], agent: &str, why: &Why) -> Option<(&'a str, &'a str)> {
    let (i, (kind, who)) = comments
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, c)| Some((i, head(&c.body)?)))?;
    let body = &comments[i].body;
    let mine = match why {
        Why::Reason(_) => who == agent && forced_by(body).is_none(),
        Why::Force(_) => forced_by(body) == Some(agent),
    };
    if kind != Kind::Release || !mine {
        return None;
    }
    comments[..i]
        .iter()
        .rev()
        .find(|c| head(&c.body) == Some((Kind::Claim, who)))
        .map(|c| (who, c.body.as_str()))
}

/// The state recorded by `from: <name>`, which claim writes as the last line.
/// Only the last line counts, so a scope spanning lines cannot forge it.
fn claimed_from(claim: &str) -> Option<&str> {
    claim.lines().skip(1).last()?.strip_prefix("from: ")
}

fn claim(
    client: &mut Client,
    config: &Config,
    ident: &str,
    agent: &str,
    source: &str,
    scope: &str,
) -> Result<u8, Error> {
    let issue = issue_info(client, ident)?;
    let started = issue.state_of_type(STARTED, config)?;
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
fn prior_state<'a>(issue: &'a Issue, claim: &str, config: &Config) -> Result<&'a State, Error> {
    let recorded = claimed_from(claim);
    if let Some(state) = recorded.and_then(|name| issue.states.iter().find(|s| s.name == name)) {
        return Ok(state);
    }
    for kind in [UNSTARTED, BACKLOG] {
        if let Some(state) = states::pick(&issue.states, kind, &config.states)? {
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

/// The state a release leaves the issue in.
enum End {
    Restore,
    Done,
    Abandon,
}

/// The state named by `states.abandoned`, which must be a team state of type
/// `canceled`. There is no default: picking a canceled state by position
/// could mark the attempt with a state the team uses for something else.
fn abandoned_state<'a>(issue: &'a Issue, config: &Config) -> Result<&'a State, Error> {
    let Some(name) = config.states.get(ABANDONED) else {
        return Err(usage(format!(
            "release --abandon needs the abandoned state: set \"states\": {{\"{ABANDONED}\": \
             \"<state name>\"}} in the config file to a {CANCELED} state; nothing written"
        )));
    };
    match issue.states.iter().find(|s| &s.name == name) {
        Some(state) if state.kind == CANCELED => Ok(state),
        Some(state) => Err(usage(format!(
            "the config's {ABANDONED} state {name:?} is of type {}, not {CANCELED}; nothing written",
            state.kind
        ))),
        None => Err(usage(format!(
            "the team of {} has no state {name:?} (the config's {ABANDONED} state); nothing written",
            issue.identifier
        ))),
    }
}

fn release(
    client: &mut Client,
    config: &Config,
    ident: &str,
    agent: &str,
    why: Why,
    end: End,
) -> Result<u8, Error> {
    let issue = issue_info(client, ident)?;
    let abandoned = match end {
        End::Abandon => Some(abandoned_state(&issue, config)?),
        _ => None,
    };
    let all = comments(client, &issue)?;
    let Some((holder, claim)) = holder(&all) else {
        // A release whose comment was written but whose state was not set
        // left no holder; finishing it must not write a second comment.
        let resume = resumable(&all, agent, &why).filter(|_| issue.state_kind == STARTED);
        let Some((holder, claim)) = resume else {
            client.note(&format!(
                "{} has no holder; nothing written, state unchanged",
                issue.identifier
            ));
            return Ok(EXIT_RELEASE_REFUSED);
        };
        client.note(&format!(
            "{} was released by {agent} but is still {:?}; resuming: no comment written, \
             setting the state",
            issue.identifier, issue.state
        ));
        let (holder, claim) = (holder.to_owned(), claim.to_owned());
        return set_release_state(client, config, &issue, &holder, &claim, end, abandoned);
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
        Why::Reason(reason) if abandoned.is_some() => {
            format!("release: {agent} abandoned: {reason}")
        }
        Why::Reason(reason) => format!("release: {agent} {reason}"),
    };
    let (holder, claim) = (holder.to_owned(), claim.to_owned());
    comment(client, &issue, &body)?;
    set_release_state(client, config, &issue, &holder, &claim, end, abandoned)
}

/// The second half of a release, after its comment: set the state `end`
/// asks for.
fn set_release_state(
    client: &mut Client,
    config: &Config,
    issue: &Issue,
    holder: &str,
    claim: &str,
    end: End,
    abandoned: Option<&State>,
) -> Result<u8, Error> {
    let state = match (end, abandoned) {
        (_, Some(state)) => state,
        (End::Done, _) => issue.state_of_type(COMPLETED, config)?,
        _ => prior_state(issue, claim, config)?,
    };
    set_state(client, issue, state)?;
    client.print(&format!("released {} (held by {holder})", issue.identifier));
    Ok(0)
}

/// `args.labels` already holds the config's default labels, each once.
fn create(client: &mut Client, args: &CreateArgs) -> Result<u8, Error> {
    let CreateArgs {
        team: team_key,
        project,
        title,
        description_file,
        labels,
        parent,
        json: as_json,
    } = args;
    let description = std::fs::read_to_string(description_file)
        .map_err(|err| usage(format!("cannot read {}: {err}", description_file.display())))?;
    // Before any label is created: a missing parent writes nothing.
    let parent = parent
        .as_deref()
        .map(|ident| {
            let data = client.request(
                "query($id: String!) { issue(id: $id) { id } }",
                json!({"id": ident}),
            )?;
            data.pointer("/issue/id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| usage(format!("no parent issue {ident}; nothing created")))
        })
        .transpose()?;
    let team = team_id(client, team_key)?;
    let mut input = json!({"teamId": team, "title": title, "description": description});
    if let Some(parent) = parent {
        input["parentId"] = parent.into();
    }
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
    if !labels.is_empty() {
        let ids = labels
            .iter()
            .map(|name| label_id(client, &team, name))
            .collect::<Result<Vec<_>, _>>()?;
        input["labelIds"] = json!(ids);
    }
    let data = client.request(
        "mutation($i: IssueCreateInput!) { issueCreate(input: $i) { success issue { identifier url } } }",
        json!({"i": input}),
    )?;
    let identifier = text(&data, "/issueCreate/issue/identifier")?;
    let url = text(&data, "/issueCreate/issue/url")?;
    if *as_json {
        client.print_json(&json!({"identifier": identifier, "url": url}));
    } else {
        client.print(&format!("{identifier} {url}"));
    }
    Ok(0)
}

fn team_id(client: &mut Client, key: &str) -> Result<String, Error> {
    let data = client.request(
        "query($k: String!) { teams(filter: {key: {eq: $k}}) { nodes { id } } }",
        json!({"k": key}),
    )?;
    match data.pointer("/teams/nodes").and_then(Value::as_array) {
        Some(nodes) if !nodes.is_empty() => Ok(text(&nodes[0], "/id")?.to_owned()),
        _ => Err(usage(format!("no team with key {key}"))),
    }
}

struct Project {
    id: String,
    name: String,
    url: String,
    on_team: bool,
    archived: bool,
}

/// Every project named exactly `name`, archived ones included and trashed
/// (deleted) ones left out, across pages, with whether `team` is among its
/// teams. The name is compared here as well, so the match is
/// case-sensitive whatever collation the server uses.
fn projects_named(client: &mut Client, name: &str, team: &str) -> Result<Vec<Project>, Error> {
    let mut all = Vec::new();
    let mut after = Value::Null;
    loop {
        let data = client.request(
            "query($n: String!, $t: ID!, $after: String) { projects(first: 50, after: $after, includeArchived: true, filter: {name: {eq: $n}}) { nodes { id name url archivedAt trashed teams(filter: {id: {eq: $t}}) { nodes { id } } } pageInfo { hasNextPage endCursor } } }",
            json!({"n": name, "t": team, "after": after}),
        )?;
        let page = &data["projects"];
        for node in page["nodes"].as_array().into_iter().flatten() {
            if text(node, "/name")? != name || node["trashed"] == true {
                continue;
            }
            all.push(Project {
                id: text(node, "/id")?.to_owned(),
                name: name.to_owned(),
                url: text(node, "/url")?.to_owned(),
                on_team: node
                    .pointer("/teams/nodes")
                    .and_then(Value::as_array)
                    .is_some_and(|teams| !teams.is_empty()),
                archived: !node["archivedAt"].is_null(),
            });
        }
        if page.pointer("/pageInfo/hasNextPage") != Some(&Value::Bool(true)) {
            break;
        }
        after = Value::from(text(page, "/pageInfo/endCursor")?);
    }
    Ok(all)
}

/// Idempotent by name: an existing project on the team is printed as it is,
/// and nothing is written unless no project has the name.
fn create_project(
    client: &mut Client,
    team_key: &str,
    name: &str,
    description_file: Option<&Path>,
    as_json: bool,
) -> Result<u8, Error> {
    let content = description_file
        .map(|path| {
            std::fs::read_to_string(path)
                .map_err(|err| usage(format!("cannot read {}: {err}", path.display())))
        })
        .transpose()?;
    let team = team_id(client, team_key)?;
    let mut existing = projects_named(client, name, &team)?;
    if existing.len() > 1 {
        return Err(usage(format!(
            "{} projects are named {name:?}; nothing created",
            existing.len()
        )));
    }
    let (project, created) = match existing.pop() {
        // Exit 0 would tell the caller to file work into an archived project.
        Some(project) if project.archived => {
            return Err(usage(format!(
                "project {name:?} exists but is archived ({}); nothing created: \
                 unarchive it in Linear or choose another name",
                project.url
            )));
        }
        Some(project) if project.on_team => {
            client.note(&format!(
                "project {name:?} already exists on team {team_key}; nothing created"
            ));
            (project, false)
        }
        Some(project) => {
            return Err(usage(format!(
                "project {name:?} exists but is not on team {team_key} ({}); \
                 nothing created, the team was not added",
                project.url
            )));
        }
        None => {
            let mut input = json!({"name": name, "teamIds": [team]});
            if let Some(content) = content {
                input["content"] = content.into();
            }
            let data = client.request(
                "mutation($i: ProjectCreateInput!) { projectCreate(input: $i) { success project { id name url } } }",
                json!({"i": input}),
            )?;
            let project = Project {
                id: text(&data, "/projectCreate/project/id")?.to_owned(),
                name: text(&data, "/projectCreate/project/name")?.to_owned(),
                url: text(&data, "/projectCreate/project/url")?.to_owned(),
                on_team: true,
                archived: false,
            };
            (project, true)
        }
    };
    if as_json {
        client.print_json(&json!({
            "name": project.name,
            "url": project.url,
            "id": project.id,
            "created": created,
        }));
    } else {
        client.print(&format!("{} {}", project.name, project.url));
    }
    Ok(0)
}

/// The issue's project is set only when it has none, so this never moves an
/// issue out of a project. Archived projects (trashed ones are archived too)
/// do not count: Linear leaves them out unless `includeArchived` is given.
fn set_project(client: &mut Client, ident: &str, name: &str, as_json: bool) -> Result<u8, Error> {
    let data = client.request(
        "query($id: String!) { issue(id: $id) { id identifier team { id key } project { id name url } } }",
        json!({"id": ident}),
    )?;
    let issue = &data["issue"];
    if issue.is_null() {
        return Err(usage(format!("no issue {ident}; nothing written")));
    }
    let identifier = text(issue, "/identifier")?.to_owned();
    let team = text(issue, "/team/id")?.to_owned();
    let team_key = text(issue, "/team/key")?.to_owned();
    let mut matches = Vec::new();
    let mut after = Value::Null;
    loop {
        let data = client.request(
            "query($t: String!, $n: String!, $after: String) { team(id: $t) { projects(first: 50, after: $after, filter: {name: {eq: $n}}) { nodes { id name url } pageInfo { hasNextPage endCursor } } } }",
            json!({"t": team, "n": name, "after": after}),
        )?;
        let page = &data["team"]["projects"];
        for node in page["nodes"].as_array().into_iter().flatten() {
            // Compared here too, so the match is case-sensitive whatever
            // collation the server uses.
            if text(node, "/name")? == name {
                matches.push((
                    text(node, "/id")?.to_owned(),
                    text(node, "/url")?.to_owned(),
                ));
            }
        }
        if page.pointer("/pageInfo/hasNextPage") != Some(&Value::Bool(true)) {
            break;
        }
        after = Value::from(text(page, "/pageInfo/endCursor")?);
    }
    let [(project, url)] = matches.as_slice() else {
        return Err(usage(format!(
            "team {team_key} has {} unarchived projects named {name:?}; exactly one is needed, \
             nothing written",
            matches.len()
        )));
    };
    let changed = match issue["project"].as_object() {
        None => {
            let data = client.request(
                "mutation($id: String!, $p: String!) { issueUpdate(id: $id, input: {projectId: $p}) { success } }",
                json!({"id": text(issue, "/id")?, "p": project}),
            )?;
            if data.pointer("/issueUpdate/success") != Some(&Value::Bool(true)) {
                return Err(usage(format!(
                    "Linear did not put {identifier} into project {name:?}"
                )));
            }
            true
        }
        Some(_) if text(issue, "/project/id")? == project => {
            client.note(&format!(
                "{identifier} is already in project {name:?}; nothing written"
            ));
            false
        }
        Some(_) => {
            return Err(usage(format!(
                "{identifier} is in project {:?} ({}); nothing written: \
                 set-project does not move an issue between projects",
                text(issue, "/project/name")?,
                text(issue, "/project/url")?,
            )));
        }
    };
    if as_json {
        client.print_json(&json!({
            "identifier": identifier,
            "project": name,
            "url": url,
            "changed": changed,
        }));
    } else {
        client.print(&format!("{identifier} {name} {url}"));
    }
    Ok(0)
}

/// The id and identifier of an issue, or an error saying nothing was written.
fn issue_id(client: &mut Client, ident: &str) -> Result<(String, String), Error> {
    let data = client.request(
        "query($id: String!) { issue(id: $id) { id identifier } }",
        json!({"id": ident}),
    )?;
    let issue = &data["issue"];
    if issue.is_null() {
        return Err(usage(format!("no issue {ident}; nothing written")));
    }
    Ok((
        text(issue, "/id")?.to_owned(),
        text(issue, "/identifier")?.to_owned(),
    ))
}

/// Every relation on one side of the issue: `relations` (the issue is
/// `issue`) or `inverseRelations` (the issue is `relatedIssue`), as the type
/// and the id of the issue on the other side, across pages.
fn relations(client: &mut Client, id: &str, field: &str) -> Result<Vec<(String, String)>, Error> {
    let far = if field == "relations" {
        "relatedIssue"
    } else {
        "issue"
    };
    let mut all = Vec::new();
    let mut after = Value::Null;
    loop {
        let data = client.request(
            &format!(
                "query($id: String!, $after: String) {{ issue(id: $id) {{ {field}(first: 100, after: $after) {{ nodes {{ type {far} {{ id }} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"
            ),
            json!({"id": id, "after": after}),
        )?;
        let page = &data["issue"][field];
        for node in page["nodes"].as_array().into_iter().flatten() {
            all.push((
                text(node, "/type")?.to_owned(),
                text(node, &format!("/{far}/id"))?.to_owned(),
            ));
        }
        if page.pointer("/pageInfo/hasNextPage") != Some(&Value::Bool(true)) {
            break;
        }
        after = Value::from(text(page, "/pageInfo/endCursor")?);
    }
    Ok(all)
}

/// Idempotent: any relation already between the two, whatever its type or
/// direction, means nothing is written. Both directions are read from the
/// first issue's side, which sees every relation it takes part in.
fn relate(client: &mut Client, ident: &str, other: &str, as_json: bool) -> Result<u8, Error> {
    let (id, identifier) = issue_id(client, ident)?;
    let (other_id, other_identifier) = issue_id(client, other)?;
    if id == other_id {
        client.note(&format!(
            "error: {ident} and {other} are the same issue, {identifier}; nothing written"
        ));
        return Ok(EXIT_SAME_ISSUE);
    }
    let outgoing = relations(client, &id, "relations")?;
    let incoming = relations(client, &id, "inverseRelations")?;
    let existing = outgoing
        .iter()
        .find(|(_, far)| *far == other_id)
        .map(|(kind, _)| (kind, &identifier, &other_identifier))
        .or_else(|| {
            incoming
                .iter()
                .find(|(_, far)| *far == other_id)
                .map(|(kind, _)| (kind, &other_identifier, &identifier))
        });
    let (kind, created) = match existing {
        Some((kind, from, to)) => {
            let added = if kind == "related" {
                String::new()
            } else {
                "; no `related` relation added".to_owned()
            };
            client.note(&format!(
                "a `{kind}` relation already links {from} to {to}; nothing written{added}"
            ));
            (kind.clone(), false)
        }
        None => {
            let data = client.request(
                "mutation($i: IssueRelationCreateInput!) { issueRelationCreate(input: $i) { success } }",
                json!({"i": {"issueId": id, "relatedIssueId": other_id, "type": "related"}}),
            )?;
            if data.pointer("/issueRelationCreate/success") != Some(&Value::Bool(true)) {
                return Err(usage(format!(
                    "Linear did not relate {identifier} to {other_identifier}"
                )));
            }
            ("related".to_owned(), true)
        }
    };
    if as_json {
        client.print_json(&json!({
            "issue": identifier,
            "other": other_identifier,
            "type": kind,
            "created": created,
        }));
    } else {
        client.print(&format!("{identifier} {other_identifier}"));
    }
    Ok(0)
}

/// The id of the label `name` usable in the team: a workspace label or the
/// team's own. Created on the team when there is none.
fn label_id(client: &mut Client, team: &str, name: &str) -> Result<String, Error> {
    let data = client.request(
        "query($n: String!) { issueLabels(filter: {name: {eq: $n}}) { nodes { id team { id } } } }",
        json!({"n": name}),
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
        json!({"n": name, "t": team}),
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
        assert_eq!(who(&["claim: a s", "release: a abandoned: stuck"]), None);
        assert_eq!(
            who(&["claim: a s", "release: a done", "claim: b s"]),
            Some("b".into())
        );
    }
}
