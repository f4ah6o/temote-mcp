use std::path::PathBuf;
#[cfg(feature = "network")]
use std::{net::SocketAddr, str::FromStr};

#[path = "codex.rs"]
pub(crate) mod codex;

use crate::change_cli;
use crate::config;
use crate::environment_prepare_cli;
#[cfg(feature = "network")]
use crate::fabric_browser;
#[cfg(feature = "network")]
use crate::gateway;
use crate::observation;
use crate::profile;
use crate::task_cli;

const CLI_NAME: &str = "temote";

pub struct Cli {
    pub command: Option<Command>,
}

pub enum Command {
    Doctor {
        profile: Option<profile::Profile>,
        cloudflare: bool,
        tunnel_token_file: Option<PathBuf>,
    },
    Start {
        session_id: Option<String>,
        yolo: bool,
    },
    Supervisor {
        restore_plan: Option<PathBuf>,
        capabilities: bool,
    },
    Upgrade {
        dry_run: bool,
        force: bool,
    },
    Activity {
        session_id: Option<String>,
        tail: usize,
        follow: bool,
    },
    Observation {
        command: observation::cli::ObservationCommand,
    },
    #[cfg(unix)]
    PromptIngress,
    #[cfg(unix)]
    PromptLink {
        prompt_id: uuid::Uuid,
        session_id: String,
        task_id: uuid::Uuid,
    },
    #[cfg(unix)]
    Friction {
        command: FrictionCommand,
    },
    UpgradeCoordinator {
        transaction_id: String,
        commit_fd: i32,
        executable_fd: i32,
        installed_locator: PathBuf,
    },
    Session {
        command: SessionCommand,
    },
    Task {
        request: task_cli::TaskInvocation,
    },
    Change {
        request: change_cli::ChangeInvocation,
    },
    EnvironmentPrepare {
        request: environment_prepare_cli::Invocation,
    },
    Mcp,
    #[cfg(feature = "network")]
    Serve {
        profile: profile::Profile,
        public_url: Option<String>,
        addr: SocketAddr,
        tunnel_token_file: Option<PathBuf>,
    },
    #[cfg(all(feature = "network", unix))]
    Up {
        profile: profile::Profile,
        public_url: Option<String>,
        addr: SocketAddr,
        tunnel_token_file: Option<PathBuf>,
    },
    #[cfg(all(feature = "network", unix))]
    Down,
    #[cfg(all(feature = "network", unix))]
    Migrate {
        dry_run: bool,
    },
    #[cfg(feature = "network")]
    Openai {
        command: OpenaiCommand,
    },
    #[cfg(feature = "network")]
    GatewayAgent {
        gateway_url: String,
        session_id: Option<String>,
        host_id: Option<String>,
        host_token: String,
        access_client_id: Option<String>,
        access_client_secret: Option<String>,
        platform: gateway::Platform,
        reconnect_delay_seconds: u64,
    },
    #[cfg(feature = "network")]
    EventsSender {
        addr: SocketAddr,
    },
    #[cfg(feature = "network")]
    FabricStatus,
    #[cfg(feature = "network")]
    FabricConnect {
        options: fabric_browser::BrowserConnectOptions,
    },
    #[cfg(feature = "network")]
    FabricLogout {
        options: fabric_browser::BrowserConnectOptions,
    },
}

#[cfg(feature = "network")]
pub enum OpenaiCommand {
    Setup {
        name: String,
        description: String,
        organization_ids: Vec<String>,
        workspace_ids: Vec<String>,
        config_file: Option<PathBuf>,
        force: bool,
    },
}

pub enum SessionCommand {
    Start {
        session_id: String,
        path: String,
    },
    StartManaged {
        source: String,
        operation_id: String,
        base: Option<String>,
        vcs: String,
    },
    List,
    Info {
        session_id: String,
    },
    Stop {
        session_id: String,
    },
    Forget {
        session_id: String,
    },
    Gc {
        apply: bool,
        limit: usize,
    },
    Restart {
        session_id: String,
    },
    RestartPolicy {
        session_id: String,
        policy: String,
    },
    Permission {
        session_id: String,
        command: SessionPermissionCommand,
    },
    Console,
}

pub enum SessionPermissionCommand {
    Status,
    Ask,
    Agent,
    Yolo,
    Allow {
        path: PathBuf,
    },
    Revoke {
        path: PathBuf,
    },
    Grant {
        request: config::SessionGrantRequest,
    },
    Ungrant {
        request: config::SessionGrantRequest,
    },
}

#[cfg(unix)]
pub enum FrictionCommand {
    ScanMany {
        session_ids: Vec<String>,
        consumer_id: String,
        generation: u64,
    },
    Scan {
        session_id: String,
        consumer_id: String,
        generation: u64,
    },
    Preview {
        fingerprint: String,
    },
    KnownIssue {
        fingerprint: String,
        issue_ref: String,
        consumer_id: String,
        generation: u64,
    },
    RecordPr {
        fingerprint: String,
        pr_url: String,
    },
    ReconcilePr {
        fingerprint: String,
    },
    Publish {
        fingerprint: String,
        publication_session_id: String,
        temote_repo_root: PathBuf,
        model: String,
        effort: String,
        authorization: crate::friction::publisher::PublicationAuthorization,
    },
}

pub enum ParseOutcome {
    Run(Box<Cli>),
    Print(String),
}

impl ParseOutcome {
    fn run(cli: Cli) -> Self {
        Self::Run(Box::new(cli))
    }
}

pub fn parse_env() -> Result<ParseOutcome, String> {
    let raw = std::env::args().collect::<Vec<_>>();
    if raw.get(1).map(String::as_str) == Some("codex") {
        return codex::run(&raw[2..]).map(ParseOutcome::Print);
    }
    if raw.get(1).map(String::as_str) == Some("delegate") {
        return codex::run_delegate(&raw[2..]).map(ParseOutcome::Print);
    }
    if raw.get(1).map(String::as_str) == Some("task") {
        if raw.len() == 2 || raw[2..].iter().any(|arg| arg == "--help" || arg == "-h") {
            return Ok(ParseOutcome::Print(task_cli::USAGE.to_owned()));
        }
        return task_cli::parse(&raw[2..]).map(|request| {
            ParseOutcome::run(Cli {
                command: Some(Command::Task { request }),
            })
        });
    }
    if raw.get(1).map(String::as_str) == Some("env-prepare") {
        if raw.len() == 2 || raw[2..].iter().any(|arg| arg == "--help" || arg == "-h") {
            return Ok(ParseOutcome::Print(
                environment_prepare_cli::USAGE.to_owned(),
            ));
        }
        return environment_prepare_cli::parse(&raw[2..]).map(|request| {
            ParseOutcome::run(Cli {
                command: Some(Command::EnvironmentPrepare { request }),
            })
        });
    }
    if raw.get(1).map(String::as_str) == Some("change") {
        if raw.len() == 2 || raw[2..].iter().any(|arg| arg == "--help" || arg == "-h") {
            return Ok(ParseOutcome::Print(change_cli::USAGE.to_owned()));
        }
        return change_cli::parse(&raw[2..]).map(|request| {
            ParseOutcome::run(Cli {
                command: Some(Command::Change { request }),
            })
        });
    }
    parse(raw.into_iter())
}

fn parse<I>(raw: I) -> Result<ParseOutcome, String>
where
    I: Iterator<Item = String>,
{
    let raw = raw.collect::<Vec<_>>();
    if matches!(raw.get(1).map(String::as_str), Some("--version" | "-V")) {
        return Ok(ParseOutcome::Print(format!(
            "{} {}\n",
            CLI_NAME,
            env!("CARGO_PKG_VERSION")
        )));
    }

    let mut args = noargs::RawArgs::new(raw.into_iter());
    args.metadata_mut().app_name = CLI_NAME;
    args.metadata_mut().app_description = env!("CARGO_PKG_DESCRIPTION");
    noargs::HELP_FLAG.take_help(&mut args);

    if args.metadata().help_mode && args.remaining_args().next().is_none() {
        // clap exposes version on the root command, not on every subcommand.
        noargs::flag("version")
            .short('V')
            .doc("Print version")
            .take(&mut args);
    }

    if !args.metadata().help_mode && args.remaining_args().next().is_none() {
        return Ok(ParseOutcome::run(Cli {
            command: Some(Command::Start {
                session_id: None,
                yolo: false,
            }),
        }));
    }

    if noargs::cmd("doctor")
        .doc("Diagnose temote-mcp, local Tunnel prerequisites, and the host sandbox")
        .take(&mut args)
        .is_present()
    {
        let command = parse_doctor(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    if noargs::cmd("start")
        .doc("Start a session in the current directory and show its permission UI")
        .take(&mut args)
        .is_present()
    {
        let command = parse_start(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    if noargs::cmd("supervisor")
        .doc("Run the local Temote session supervisor")
        .take(&mut args)
        .is_present()
    {
        let restore_plan = noargs::opt("restore-plan")
            .ty("PATH")
            .doc("Internal: restore a validated supervisor handoff plan")
            .take(&mut args)
            .present()
            .map(|opt| PathBuf::from(opt.value()));
        let capabilities = noargs::flag("capabilities")
            .doc("Print supervisor handoff protocol capabilities as JSON")
            .take(&mut args)
            .is_present();
        if capabilities && restore_plan.is_some() {
            return Err("--capabilities and --restore-plan cannot be combined".to_owned());
        }
        return finish(
            args,
            Command::Supervisor {
                restore_plan,
                capabilities,
            },
        );
    }
    if noargs::cmd("upgrade")
        .doc("Safely hand off the running supervisor to this installed Temote binary")
        .take(&mut args)
        .is_present()
    {
        let dry_run = noargs::flag("dry-run")
            .doc("Validate and print the handoff plan without changing processes or sessions")
            .take(&mut args)
            .is_present();
        let force = noargs::flag("force")
            .doc("Perform a handoff even when versions match, stopping sessions that cannot be restored")
            .take(&mut args)
            .is_present();
        return finish(args, Command::Upgrade { dry_run, force });
    }
    if noargs::cmd("activity")
        .doc("Show recent local supervisor activity and follow new events")
        .take(&mut args)
        .is_present()
    {
        let command = parse_activity(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    if noargs::cmd("observation")
        .doc("Owner-only observation journal debug surface")
        .take(&mut args)
        .is_present()
    {
        let command = parse_observation(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    #[cfg(unix)]
    if noargs::cmd("friction")
        .doc("Owner-only bounded friction scan, preview, and authorized publication")
        .take(&mut args)
        .is_present()
    {
        let command = parse_friction(&mut args).map_err(format_error)?;
        return finish(args, Command::Friction { command });
    }
    if noargs::cmd("upgrade-coordinator")
        .doc("Internal: continue one accepted remote upgrade transaction")
        .take(&mut args)
        .is_present()
    {
        let transaction_id = noargs::opt("transaction")
            .ty("ID")
            .take(&mut args)
            .then(|opt| Ok::<_, std::convert::Infallible>(opt.value().to_owned()))
            .map_err(format_error)?;
        let commit_fd = noargs::opt("commit-fd")
            .ty("FD")
            .take(&mut args)
            .then(|opt| opt.value().parse::<i32>().map_err(|_| "must be an integer"))
            .map_err(format_error)?;
        let executable_fd = noargs::opt("executable-fd")
            .ty("FD")
            .take(&mut args)
            .then(|opt| opt.value().parse::<i32>().map_err(|_| "must be an integer"))
            .map_err(format_error)?;
        let installed_locator = noargs::opt("installed-locator")
            .ty("PATH")
            .take(&mut args)
            .then(|opt| Ok::<_, std::convert::Infallible>(PathBuf::from(opt.value())))
            .map_err(format_error)?;
        return finish(
            args,
            Command::UpgradeCoordinator {
                transaction_id,
                commit_fd,
                executable_fd,
                installed_locator,
            },
        );
    }
    if noargs::cmd("session")
        .doc("Manage sessions owned by the local Temote supervisor")
        .take(&mut args)
        .is_present()
    {
        let command = parse_session(&mut args).map_err(format_error)?;
        return finish(args, Command::Session { command });
    }
    if noargs::cmd("task")
        .doc("Operate retained delegated tasks in a local session (see task --help)")
        .take(&mut args)
        .is_present()
    {
        return Ok(ParseOutcome::Print(task_cli::USAGE.to_owned()));
    }
    if noargs::cmd("mcp")
        .doc("Run the session-independent MCP server over stdin/stdout")
        .take(&mut args)
        .is_present()
    {
        return finish(args, Command::Mcp);
    }
    if noargs::cmd("codex")
        .doc("Install and diagnose the local Codex plugin integration")
        .take(&mut args)
        .is_present()
    {
        return Ok(ParseOutcome::Print(codex::usage()));
    }
    if noargs::cmd("delegate")
        .doc("Run one bounded non-interactive delegation request (legacy fallback; the server-backed task tools are primary)")
        .take(&mut args)
        .is_present()
    {
        return Ok(ParseOutcome::Print(codex::delegate_usage()));
    }
    #[cfg(feature = "network")]
    if noargs::cmd("serve")
        .doc("Run the MCP server over HTTP using the selected authentication profile")
        .take(&mut args)
        .is_present()
    {
        let command = parse_serve(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    #[cfg(all(feature = "network", unix))]
    if noargs::cmd("up")
        .doc("Start the HTTP server and selected ingress as one foreground supervisor")
        .take(&mut args)
        .is_present()
    {
        let command = parse_up(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    #[cfg(all(feature = "network", unix))]
    if noargs::cmd("down")
        .doc("Stop the foreground supervisor started by temote-mcp up")
        .take(&mut args)
        .is_present()
    {
        return finish(args, Command::Down);
    }
    #[cfg(all(feature = "network", unix))]
    if noargs::cmd("migrate")
        .doc("Migrate legacy runtime ownership and checkout-local Cloudflare configuration")
        .take(&mut args)
        .is_present()
    {
        let dry_run = noargs::flag("dry-run")
            .doc("Report migration without changing files or processes")
            .take(&mut args)
            .is_present();
        return finish(args, Command::Migrate { dry_run });
    }
    #[cfg(feature = "network")]
    if noargs::cmd("openai")
        .doc("Manage OpenAI Secure MCP Tunnel setup")
        .take(&mut args)
        .is_present()
    {
        let command = parse_openai(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    #[cfg(feature = "network")]
    if noargs::cmd("fabric")
        .doc("Connect this Host to Temote Fabric or run its event sender")
        .take(&mut args)
        .is_present()
    {
        let command = parse_fabric(&mut args).map_err(format_error)?;
        return finish(args, command);
    }
    #[cfg(feature = "network")]
    if noargs::cmd("gateway-agent")
        .doc("Connect an active local session to a Cloudflare gateway using outbound long polling")
        .take(&mut args)
        .is_present()
    {
        let command = parse_gateway_agent(&mut args).map_err(format_error)?;
        return finish(args, command);
    }

    match args.finish().map_err(format_error)? {
        Some(help) => Ok(ParseOutcome::Print(help)),
        None => unreachable!("a command or help should have been selected"),
    }
}

#[cfg(feature = "network")]
fn parse_fabric(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    if noargs::cmd("status")
        .doc("Show bounded local Link and remote Fabric health")
        .take(args)
        .is_present()
    {
        return Ok(Command::FabricStatus);
    }
    if noargs::cmd("connect")
        .doc("Connect with a configured static Host token, or enroll with browser OAuth")
        .take(args)
        .is_present()
    {
        return parse_fabric_link(args);
    }
    if noargs::cmd("logout")
        .doc("Revoke this Host's browser grant and remove its secure credentials")
        .take(args)
        .is_present()
    {
        return parse_fabric_browser(args, true);
    }
    if noargs::cmd("link")
        .doc("Alias for connect: use a configured static Host token or browser enrollment")
        .take(args)
        .is_present()
    {
        return parse_fabric_link(args);
    }
    if noargs::cmd("events-sender")
        .doc("Run the dedicated loopback HTTPS webhook sender behind Access/Tunnel")
        .take(args)
        .is_present()
    {
        let addr = noargs::opt("addr")
            .ty("ADDR")
            .default("127.0.0.1:4211")
            .doc("Loopback address for the Access-protected Tunnel origin")
            .take(args)
            .then(|opt| SocketAddr::from_str(opt.value()))?;
        if !addr.ip().is_loopback() {
            return Err(noargs::Error::other(
                args,
                "event sender must bind a loopback address",
            ));
        }
        return Ok(Command::EventsSender { addr });
    }
    if args.metadata().help_mode {
        return Ok(Command::FabricStatus);
    }
    Err(noargs::Error::other(
        args,
        "Fabric command is not specified (expected 'connect', 'link', 'status' or 'events-sender')",
    ))
}

#[cfg(feature = "network")]
fn parse_fabric_link(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    let host_token = string_opt_env(
        args,
        "host-token",
        Some("TEMOTE_MCP_GATEWAY_HOST_TOKEN"),
        "Static Host token; when configured, connect/link use the existing static Link mode",
    )?;
    let access_client_id = string_opt_env(
        args,
        "access-client-id",
        Some("TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID"),
        "Optional Cloudflare Access service-token client ID",
    )?;
    let access_client_secret = string_opt_env(
        args,
        "access-client-secret",
        Some("TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET"),
        "Optional Cloudflare Access service-token client secret",
    )?;
    if let Some(host_token) = host_token {
        if host_token.trim().is_empty() {
            return Err(noargs::Error::other(
                args,
                "static Host token must not be empty",
            ));
        }
        return parse_gateway_agent_with_credentials(
            args,
            host_token,
            access_client_id,
            access_client_secret,
        );
    }
    if access_client_id.is_some() || access_client_secret.is_some() {
        return Err(noargs::Error::other(
            args,
            "Cloudflare Access service-token credentials require a static Host token",
        ));
    }
    parse_fabric_browser(args, false)
}

#[cfg(feature = "network")]
fn parse_fabric_browser(args: &mut noargs::RawArgs, logout: bool) -> noargs::Result<Command> {
    let gateway_url = required_string_opt(
        args,
        "gateway-url",
        Some("TEMOTE_MCP_GATEWAY_URL"),
        "Temote Fabric Worker HTTPS origin",
        "https://fabric.example.com",
    )?;
    let issuer = required_string_opt(
        args,
        "oauth-issuer",
        Some("TEMOTE_MCP_FABRIC_OAUTH_ISSUER"),
        "Pinned Cloudflare Managed OAuth issuer origin",
        "https://login.example.com",
    )?;
    let client_id = required_string_opt(
        args,
        "oauth-client-id",
        Some("TEMOTE_MCP_FABRIC_OAUTH_CLIENT_ID"),
        "Public OAuth client id registered for the Fabric resource",
        "<client-id>",
    )?;
    let host_id = string_opt_env(
        args,
        "host-id",
        Some("TEMOTE_MCP_GATEWAY_HOST_ID"),
        "Optional expected local Host identity; the running Supervisor remains authoritative",
    )?;
    let gateway_origin = url::Url::parse(&gateway_url)
        .map_err(|_| noargs::Error::other(args, "Fabric gateway URL is invalid"))?
        .origin()
        .ascii_serialization();
    let issuer_origin = url::Url::parse(&issuer)
        .map_err(|_| noargs::Error::other(args, "OAuth issuer URL is invalid"))?
        .origin()
        .ascii_serialization();
    let resource = string_opt_env(
        args,
        "oauth-resource",
        Some("TEMOTE_MCP_FABRIC_OAUTH_RESOURCE"),
        "RFC 8707 resource indicator (defaults to the configured Fabric origin)",
    )?
    .unwrap_or_else(|| gateway_origin.clone());
    let options = fabric_browser::BrowserConnectOptions {
        gateway_url,
        host_id,
        issuer,
        client_id,
        permitted_origins: vec![gateway_origin, issuer_origin],
        resource,
    };
    Ok(if logout {
        Command::FabricLogout { options }
    } else {
        Command::FabricConnect { options }
    })
}

fn parse_activity(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    let tail = noargs::opt("tail")
        .ty("COUNT")
        .doc("Replay the latest 0 to 1024 matching events (default: 100)")
        .default("100")
        .take(args)
        .then(|opt| opt.value().parse::<usize>())?;
    if tail > 1024 {
        return Err(noargs::Error::other(
            args,
            "activity tail must be an integer from 0 to 1024",
        ));
    }
    let follow = !noargs::flag("no-follow")
        .doc("Print the replay and exit after activity_end")
        .take(args)
        .is_present();
    let session_arg = noargs::arg("[SESSION_ID]").doc("Optional exact session ID filter");
    let next = args
        .remaining_args()
        .next()
        .map(|(_, value)| value.to_owned());
    let session_id = match next.as_deref() {
        Some("--") => {
            let marker = session_arg.take(args);
            debug_assert_eq!(marker.value(), "--");
            session_arg
                .take(args)
                .present()
                .map(|arg| arg.value().to_owned())
        }
        Some(value) if value.starts_with('-') => None,
        Some(_) => session_arg
            .take(args)
            .present()
            .map(|arg| arg.value().to_owned()),
        None => None,
    };
    if session_id
        .as_deref()
        .is_some_and(|session_id| config::validate_session_id(session_id).is_err())
    {
        return Err(noargs::Error::other(args, "invalid activity session ID"));
    }
    Ok(Command::Activity {
        session_id,
        tail,
        follow,
    })
}

fn parse_observation(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    #[cfg(unix)]
    if noargs::cmd("prompt-listen")
        .doc("Run the owner-only versioned local prompt ingress socket")
        .take(args)
        .is_present()
    {
        return Ok(Command::PromptIngress);
    }
    #[cfg(unix)]
    if noargs::cmd("prompt-link")
        .doc("Correlate an ingress record from a retained canonical Codex task")
        .take(args)
        .is_present()
    {
        let session_id = required_observation_session(args)?;
        let prompt_id = noargs::opt("prompt-id")
            .ty("UUID")
            .take(args)
            .present()
            .map(|v| v.value().parse::<uuid::Uuid>())
            .transpose()?
            .ok_or_else(|| noargs::Error::other(args, "--prompt-id is required"))?;
        let task_id = noargs::opt("task-id")
            .ty("UUID")
            .take(args)
            .present()
            .map(|v| v.value().parse::<uuid::Uuid>())
            .transpose()?
            .ok_or_else(|| noargs::Error::other(args, "--task-id is required"))?;
        return Ok(Command::PromptLink {
            prompt_id,
            session_id,
            task_id,
        });
    }
    let command = if noargs::cmd("list")
        .doc("List observation journal records for one session")
        .take(args)
        .is_present()
    {
        let kind = noargs::opt("kind")
            .ty("KIND")
            .doc("Filter to one observation kind")
            .take(args)
            .present()
            .map(|opt| opt.value().to_owned());
        let kind = match kind.as_deref() {
            Some(value) => Some(
                observation::ObservationKind::parse(value).ok_or_else(|| {
                    noargs::Error::other(
                        args,
                        format!(
                            "unknown observation kind {value:?} (instruction, operation_accepted, execution_state, evidence, verification, delivery, reconciliation)"
                        ),
                    )
                })?,
            ),
            None => None,
        };
        let task_id = noargs::opt("task")
            .ty("TASK_ID")
            .doc("Filter to one task")
            .take(args)
            .present()
            .map(|opt| opt.value().to_owned());
        if task_id
            .as_deref()
            .is_some_and(|task| task.is_empty() || task.len() > 256)
        {
            return Err(noargs::Error::other(args, "--task must be a task ID"));
        }
        let after_revision = noargs::opt("after-revision")
            .ty("REVISION")
            .doc("Only records newer than this journal revision")
            .take(args)
            .present()
            .map(|opt| opt.value().parse::<u64>())
            .transpose()?;
        let limit = noargs::opt("limit")
            .ty("COUNT")
            .doc("Return at most this many newest records (default: 64)")
            .default("64")
            .take(args)
            .then(|opt| opt.value().parse::<usize>())?;
        if limit > 256 {
            return Err(noargs::Error::other(
                args,
                "observation limit must be an integer from 0 to 256",
            ));
        }
        let include_content = noargs::flag("include-content")
            .doc("Inline bounded observation content bodies")
            .take(args)
            .is_present();
        let session_id = required_observation_session(args)?;
        observation::cli::ObservationCommand::List {
            session_id,
            kind,
            task_id,
            after_revision,
            limit,
            include_content,
        }
    } else if noargs::cmd("get")
        .doc("Print one observation journal record in full")
        .take(args)
        .is_present()
    {
        let session_id = required_observation_session(args)?;
        let observation_id = noargs::arg("<OBSERVATION_ID>")
            .doc("Observation ID")
            .take(args)
            .then(|arg| {
                arg.value()
                    .parse::<uuid::Uuid>()
                    .map_err(|error| anyhow::anyhow!("invalid observation ID: {error}"))
            })?;
        observation::cli::ObservationCommand::Get {
            session_id,
            observation_id,
        }
    } else if noargs::cmd("status")
        .doc("Show journal counters and degradation flags for one session")
        .take(args)
        .is_present()
    {
        let session_id = required_observation_session(args)?;
        observation::cli::ObservationCommand::Status { session_id }
    } else {
        return Err(noargs::Error::other(
            args,
            "expected one of: list, get, status",
        ));
    };
    Ok(Command::Observation { command })
}

#[cfg(unix)]
fn parse_friction(args: &mut noargs::RawArgs) -> noargs::Result<FrictionCommand> {
    let local = noargs::flag("local")
        .doc("Explicit owner-local command boundary")
        .take(args)
        .is_present();
    if noargs::cmd("scan-many")
        .doc("Consume bounded independent sources")
        .take(args)
        .is_present()
    {
        let spec = noargs::opt("session-id").ty("ID");
        let mut session_ids = Vec::new();
        while let Some(value) = spec.take(args).present().map(|v| v.value().to_owned()) {
            if config::validate_session_id(&value).is_err() || session_ids.contains(&value) {
                return Err(noargs::Error::other(
                    args,
                    "invalid or duplicate friction source",
                ));
            }
            session_ids.push(value);
        }
        if session_ids.is_empty() || session_ids.len() > 16 || !local {
            return Err(noargs::Error::other(
                args,
                "scan-many requires --local and 1..=16 distinct --session-id values",
            ));
        }
        let consumer_id = noargs::opt("consumer-id")
            .ty("ID")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--consumer-id is required"))?;
        let generation = noargs::opt("generation")
            .ty("N")
            .take(args)
            .present()
            .map(|v| v.value().parse::<u64>())
            .transpose()?
            .ok_or_else(|| noargs::Error::other(args, "--generation is required"))?;
        return Ok(FrictionCommand::ScanMany {
            session_ids,
            consumer_id,
            generation,
        });
    }
    if noargs::cmd("scan")
        .doc("Consume one fenced bounded episode batch")
        .take(args)
        .is_present()
    {
        let session_id = required_observation_session(args)?;
        let consumer_id = noargs::opt("consumer-id")
            .ty("ID")
            .doc("Stable worker identity")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--consumer-id is required"))?;
        let generation = noargs::opt("generation")
            .ty("N")
            .doc("Positive worker lease generation")
            .take(args)
            .present()
            .map(|v| v.value().parse::<u64>())
            .transpose()?
            .ok_or_else(|| noargs::Error::other(args, "--generation is required"))?;
        if !local {
            return Err(noargs::Error::other(
                args,
                "friction commands require --local",
            ));
        }
        return Ok(FrictionCommand::Scan {
            session_id,
            consumer_id,
            generation,
        });
    }
    if noargs::cmd("preview")
        .doc("Read bounded structural candidate metadata")
        .take(args)
        .is_present()
    {
        let fingerprint = noargs::arg("<FINGERPRINT>").take(args).value().to_owned();
        if !local {
            return Err(noargs::Error::other(
                args,
                "friction commands require --local",
            ));
        }
        return Ok(FrictionCommand::Preview { fingerprint });
    }
    if noargs::cmd("known-issue")
        .doc("Attach a verified scoped local issue identity")
        .take(args)
        .is_present()
    {
        let fingerprint = noargs::arg("<FINGERPRINT>").take(args).value().to_owned();
        let issue_ref = noargs::opt("issue-ref")
            .ty("PATH")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--issue-ref is required"))?;
        let consumer_id = noargs::opt("consumer-id")
            .ty("ID")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--consumer-id is required"))?;
        let generation = noargs::opt("generation")
            .ty("N")
            .take(args)
            .present()
            .map(|v| v.value().parse::<u64>())
            .transpose()?
            .ok_or_else(|| noargs::Error::other(args, "--generation is required"))?;
        if !local {
            return Err(noargs::Error::other(
                args,
                "friction commands require --local",
            ));
        }
        return Ok(FrictionCommand::KnownIssue {
            fingerprint,
            issue_ref,
            consumer_id,
            generation,
        });
    }
    if noargs::cmd("reconcile-pr")
        .doc("Observe the exact Temote issue and open PR through a read-only delegated task")
        .take(args)
        .is_present()
    {
        let fingerprint = noargs::arg("<FINGERPRINT>").take(args).value().to_owned();
        if !local {
            return Err(noargs::Error::other(
                args,
                "friction commands require --local",
            ));
        }
        return Ok(FrictionCommand::ReconcilePr { fingerprint });
    }
    if noargs::cmd("record-pr")
        .doc("Record an operator-attested Temote PR URL")
        .take(args)
        .is_present()
    {
        let fingerprint = noargs::arg("<FINGERPRINT>").take(args).value().to_owned();
        let pr_url = noargs::opt("pr-url")
            .ty("URL")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--pr-url is required"))?;
        if !noargs::flag("operator-attested").take(args).is_present() {
            return Err(noargs::Error::other(
                args,
                "--operator-attested is required",
            ));
        }
        if !local {
            return Err(noargs::Error::other(
                args,
                "friction commands require --local",
            ));
        }
        return Ok(FrictionCommand::RecordPr {
            fingerprint,
            pr_url,
        });
    }
    if noargs::cmd("publish")
        .doc("Authorize one durable typed Temote publication task")
        .take(args)
        .is_present()
    {
        let fingerprint = noargs::arg("<FINGERPRINT>").take(args).value().to_owned();
        let publication_session_id = noargs::opt("publication-session-id")
            .ty("ID")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--publication-session-id is required"))?;
        if config::validate_session_id(&publication_session_id).is_err() {
            return Err(noargs::Error::other(args, "invalid publication session ID"));
        }
        let temote_repo_root = noargs::opt("temote-repo-root")
            .ty("PATH")
            .take(args)
            .present()
            .map(|v| PathBuf::from(v.value()))
            .ok_or_else(|| noargs::Error::other(args, "--temote-repo-root is required"))?;
        let model = noargs::opt("model")
            .ty("MODEL")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--model is required"))?;
        let effort = noargs::opt("effort")
            .ty("EFFORT")
            .take(args)
            .present()
            .map(|v| v.value().to_owned())
            .ok_or_else(|| noargs::Error::other(args, "--effort is required"))?;
        let expires_at = noargs::opt("authorization-expires-at")
            .ty("UNIX_SECONDS")
            .take(args)
            .present()
            .map(|v| v.value().parse::<u64>())
            .transpose()?
            .ok_or_else(|| noargs::Error::other(args, "--authorization-expires-at is required"))?;
        let authorization = crate::friction::publisher::PublicationAuthorization {
            export_opt_in: noargs::flag("export-opt-in").take(args).is_present(),
            redaction_approved: noargs::flag("redaction-approved").take(args).is_present(),
            temote_repo_write: noargs::flag("temote-repo-write").take(args).is_present(),
            expires_at,
        };
        if !local {
            return Err(noargs::Error::other(
                args,
                "friction commands require --local",
            ));
        }
        return Ok(FrictionCommand::Publish {
            fingerprint,
            publication_session_id,
            temote_repo_root,
            model,
            effort,
            authorization,
        });
    }
    Err(noargs::Error::other(
        args,
        "expected one of: scan, preview, known-issue, publish, record-pr, reconcile-pr",
    ))
}

fn required_observation_session(args: &mut noargs::RawArgs) -> noargs::Result<String> {
    let session_id = noargs::arg("<SESSION_ID>")
        .doc("Session ID")
        .take(args)
        .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
    if config::validate_session_id(&session_id).is_err() {
        return Err(noargs::Error::other(args, "invalid session ID"));
    }
    Ok(session_id)
}

fn parse_grant_request(
    args: &mut noargs::RawArgs,
    allow_directories: bool,
) -> noargs::Result<config::SessionGrantRequest> {
    let mut request = config::SessionGrantRequest::default();
    let listen_port = noargs::opt("listen-port")
        .ty("PORT")
        .doc("TCP port a sandboxed command may bind and listen on; repeatable. On macOS the Seatbelt sandbox cannot scope a bind to loopback, so the port becomes bindable on all interfaces");
    while let Some(value) = listen_port
        .take(args)
        .present()
        .map(|opt| opt.value().to_owned())
    {
        let port = value.parse::<u16>().map_err(|_| {
            noargs::Error::other(args, "--listen-port must be a TCP port (0-65535)")
        })?;
        request.listen_ports.push(port);
    }
    let env_prefix = noargs::opt("dev-tool-env-prefix")
        .ty("PREFIX")
        .doc("Environment-variable name prefix dev_tool_run may accept from the caller (for example MADOBE_ or CARGO_); repeatable");
    while let Some(prefix) = env_prefix
        .take(args)
        .present()
        .map(|opt| opt.value().to_owned())
    {
        request.dev_tool_env_prefixes.push(prefix);
    }
    request.ambient_git_credentials = noargs::flag("ambient-git-credentials")
        .doc("Let network Git operations and the GitHub tools fall back to the ambient host credentials when no managed repository credential mapping is configured")
        .take(args)
        .is_present();
    if allow_directories {
        let directory = noargs::opt("directory")
            .ty("PATH")
            .doc("Additional permitted directory for the session; repeatable");
        while let Some(path) = directory
            .take(args)
            .present()
            .map(|opt| PathBuf::from(opt.value()))
        {
            request.directories.push(path);
        }
    }
    request
        .validate()
        .map_err(|error| noargs::Error::other(args, format!("{error:#}")))?;
    if request.is_effectively_empty() {
        return Err(noargs::Error::other(
            args,
            "grant request is empty (pass --listen-port, --dev-tool-env-prefix, --ambient-git-credentials, or --directory)",
        ));
    }
    Ok(request)
}

fn parse_session(args: &mut noargs::RawArgs) -> noargs::Result<SessionCommand> {
    if noargs::cmd("start")
        .doc("Start a supervisor-owned session under a configured named root")
        .take(args)
        .is_present()
    {
        let source = noargs::opt("source")
            .ty("REPOSITORY")
            .doc("Repository source such as owner/repository")
            .take(args)
            .present()
            .map(|opt| opt.value().to_owned());
        let operation_id = noargs::opt("operation-id")
            .ty("UUID")
            .doc("Caller-generated managed start retry key")
            .take(args)
            .present()
            .map(|opt| opt.value().to_owned());
        let base = noargs::opt("base")
            .ty("REF")
            .doc("Repository base ref (default: main)")
            .take(args)
            .present()
            .map(|opt| opt.value().to_owned());
        let vcs = noargs::opt("vcs")
            .ty("BACKEND")
            .doc("auto, jujutsu, or git")
            .take(args)
            .present()
            .map(|opt| opt.value().to_owned());
        let path = noargs::opt("path")
            .ty("PATH")
            .doc("Named-root-relative path such as src/my-project")
            .take(args)
            .present()
            .map(|opt| opt.value().to_owned());
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .present()
            .map(|arg| arg.value().to_owned());
        if let Some(source) = source {
            if path.is_some() || session_id.is_some() {
                return Err(noargs::Error::other(
                    args,
                    "--source cannot be combined with --path or SESSION_ID",
                ));
            }
            let operation_id = operation_id
                .ok_or_else(|| noargs::Error::other(args, "--source requires --operation-id"))?;
            return Ok(SessionCommand::StartManaged {
                source,
                operation_id,
                base,
                vcs: vcs.unwrap_or_else(|| "auto".to_owned()),
            });
        }
        if operation_id.is_some() || base.is_some() || vcs.is_some() {
            return Err(noargs::Error::other(
                args,
                "--operation-id, --base, and --vcs require --source",
            ));
        }
        let session_id =
            session_id.ok_or_else(|| noargs::Error::other(args, "--path requires SESSION_ID"))?;
        let path = path.ok_or_else(|| noargs::Error::other(args, "SESSION_ID requires --path"))?;
        return Ok(SessionCommand::Start { session_id, path });
    }
    if noargs::cmd("list")
        .doc("List active, stopped, and crashed sessions")
        .take(args)
        .is_present()
    {
        return Ok(SessionCommand::List);
    }
    if noargs::cmd("info")
        .doc("Show durable lifecycle details for one session")
        .take(args)
        .is_present()
    {
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
        return Ok(SessionCommand::Info { session_id });
    }
    if noargs::cmd("stop")
        .doc("Gracefully stop a supervisor-owned session")
        .take(args)
        .is_present()
    {
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
        return Ok(SessionCommand::Stop { session_id });
    }
    if noargs::cmd("forget")
        .doc("Remove durable metadata for one terminal, non-live session (stop keeps metadata)")
        .take(args)
        .is_present()
    {
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
        return Ok(SessionCommand::Forget { session_id });
    }
    if noargs::cmd("gc")
        .doc("Plan or apply bounded maintenance GC for reviewed orphan session metadata (dry-run by default)")
        .take(args)
        .is_present()
    {
        let apply = noargs::flag("apply")
            .doc("Delete the planned orphan halves instead of only reporting them")
            .take(args)
            .is_present();
        let limit = noargs::opt("limit")
            .ty("N")
            .doc("Maximum number of orphan entries to plan or delete (1-1000, default 100)")
            .default("100")
            .take(args)
            .then(|opt| opt.value().parse::<usize>())?;
        if !(1..=1000).contains(&limit) {
            return Err(noargs::Error::other(
                args,
                "session gc limit must be an integer from 1 to 1000",
            ));
        }
        return Ok(SessionCommand::Gc { apply, limit });
    }
    if noargs::cmd("restart")
        .doc("Restart a stopped, crashed, or active supervisor-owned session")
        .take(args)
        .is_present()
    {
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
        return Ok(SessionCommand::Restart { session_id });
    }
    if noargs::cmd("restart-policy")
        .doc("Set the automatic restart policy for a supervisor-owned session")
        .take(args)
        .is_present()
    {
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
        let policy = noargs::arg("<POLICY>")
            .doc("Restart policy: never or on-failure")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
        if !matches!(policy.as_str(), "never" | "on-failure") {
            return Err(noargs::Error::other(
                args,
                "restart policy must be never or on-failure",
            ));
        }
        return Ok(SessionCommand::RestartPolicy { session_id, policy });
    }
    if noargs::cmd("permission")
        .doc("Inspect or change permissions for a supervisor-owned session")
        .take(args)
        .is_present()
    {
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
        let command = if noargs::cmd("status")
            .doc("Show the persisted permission mode and allowed directories")
            .take(args)
            .is_present()
        {
            SessionPermissionCommand::Status
        } else if noargs::cmd("ask")
            .doc("Use the normal sandbox and local approval policy")
            .take(args)
            .is_present()
        {
            SessionPermissionCommand::Ask
        } else if noargs::cmd("agent")
            .doc("Use the sandboxed, approval-free agent policy")
            .take(args)
            .is_present()
        {
            SessionPermissionCommand::Agent
        } else if noargs::cmd("yolo")
            .doc("Explicitly use unrestricted local execution for this session")
            .take(args)
            .is_present()
        {
            SessionPermissionCommand::Yolo
        } else if noargs::cmd("allow")
            .doc("Add a canonical directory to the session sandbox roots")
            .take(args)
            .is_present()
        {
            let path = noargs::arg("<DIRECTORY>")
                .doc("Directory to allow")
                .take(args)
                .then(|arg| Ok::<_, std::convert::Infallible>(PathBuf::from(arg.value())))?;
            SessionPermissionCommand::Allow { path }
        } else if noargs::cmd("revoke")
            .doc("Remove a canonical directory from the session sandbox roots")
            .take(args)
            .is_present()
        {
            let path = noargs::arg("<DIRECTORY>")
                .doc("Directory to revoke")
                .take(args)
                .then(|arg| Ok::<_, std::convert::Infallible>(PathBuf::from(arg.value())))?;
            SessionPermissionCommand::Revoke { path }
        } else if noargs::cmd("grant")
            .doc("Persist additive capability grants approved by the host")
            .take(args)
            .is_present()
        {
            SessionPermissionCommand::Grant {
                request: parse_grant_request(args, true)?,
            }
        } else if noargs::cmd("ungrant")
            .doc("Remove capability grants previously approved on the session")
            .take(args)
            .is_present()
        {
            SessionPermissionCommand::Ungrant {
                request: parse_grant_request(args, false)?,
            }
        } else if args.metadata().help_mode {
            SessionPermissionCommand::Status
        } else {
            return Err(noargs::Error::other(
                args,
                "permission command is not specified (expected status, ask, agent, yolo, allow, revoke, grant, or ungrant)",
            ));
        };
        return Ok(SessionCommand::Permission {
            session_id,
            command,
        });
    }
    if noargs::cmd("console")
        .doc("Attach a reconnectable local approval console")
        .take(args)
        .is_present()
    {
        return Ok(SessionCommand::Console);
    }
    if args.metadata().help_mode {
        return Ok(SessionCommand::List);
    }
    Err(noargs::Error::other(
        args,
        "session command is not specified (expected start, list, info, stop, forget, gc, restart, restart-policy, permission, or console)",
    ))
}

fn parse_doctor(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    let profile = noargs::opt("profile")
        .ty("PROFILE")
        .doc("Production ingress/auth profile: cloudflare, tailscale, or openai")
        .take(args)
        .present_and_then(|opt| opt.value().parse::<profile::Profile>())?;
    let cloudflare = noargs::flag("cloudflare")
        .doc("Also query the Cloudflare API for the configured Tunnel status")
        .take(args)
        .is_present();
    let tunnel_token_file = noargs::opt("tunnel-token-file")
        .ty("PATH")
        .env("TUNNEL_TOKEN_FILE")
        .doc("Cloudflare Tunnel token file")
        .take(args)
        .present()
        .map(|opt| PathBuf::from(opt.value()));
    Ok(Command::Doctor {
        profile,
        cloudflare,
        tunnel_token_file,
    })
}

fn parse_start(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    // noargs positional arguments intentionally consume the first remaining raw
    // argument, including dash-prefixed values. Consume known flags first so
    // `start --yolo` keeps clap-compatible meaning instead of treating the flag
    // as a session ID.
    let yolo = noargs::flag("yolo")
        .doc("Disable local approvals and run tools with the full permissions of this user")
        .take(args)
        .is_present();
    let session_arg =
        noargs::arg("[SESSION_ID]").doc("Session ID to use instead of generating a UUID");
    let next = args
        .remaining_args()
        .next()
        .map(|(_, value)| value.to_owned());
    let session_id = match next.as_deref() {
        Some("--") => {
            let marker = session_arg.take(args);
            debug_assert_eq!(marker.value(), "--");
            session_arg
                .take(args)
                .present()
                .map(|arg| arg.value().to_owned())
        }
        Some(value) if value.starts_with('-') => None,
        Some(_) => session_arg
            .take(args)
            .present()
            .map(|arg| arg.value().to_owned()),
        None => None,
    };
    Ok(Command::Start { session_id, yolo })
}

#[cfg(feature = "network")]
fn parse_profile(opt: noargs::Opt) -> Result<profile::Profile, String> {
    profile::Profile::from_str(opt.value())
}

#[cfg(feature = "network")]
fn parse_socket_addr(opt: noargs::Opt) -> Result<SocketAddr, String> {
    opt.value()
        .parse()
        .map_err(|error| format!("invalid socket address: {error}"))
}

#[cfg(feature = "network")]
fn parse_serve(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    let profile = noargs::opt("profile")
        .ty("PROFILE")
        .doc("Production ingress/auth profile: cloudflare, tailscale, or openai")
        .default("cloudflare")
        .take(args)
        .then(parse_profile)?;
    let public_url = string_opt(
        args,
        "public-url",
        "Public HTTPS base URL clients reach this server through",
    )?;
    let addr = noargs::opt("addr")
        .ty("ADDR")
        .doc("Local address to listen on")
        .default("127.0.0.1:8791")
        .take(args)
        .then(parse_socket_addr)?;
    let tunnel_token_file = path_opt(
        args,
        "tunnel-token-file",
        None,
        "Run cloudflared using this token file",
    )?;
    Ok(Command::Serve {
        profile,
        public_url,
        addr,
        tunnel_token_file,
    })
}

#[cfg(all(feature = "network", unix))]
fn parse_up(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    let profile = noargs::opt("profile")
        .ty("PROFILE")
        .doc("Production ingress/auth profile: cloudflare, tailscale, or openai")
        .default("cloudflare")
        .take(args)
        .then(parse_profile)?;
    let public_url = string_opt(
        args,
        "public-url",
        "Public HTTPS base URL clients reach this server through",
    )?;
    let addr = noargs::opt("addr")
        .ty("ADDR")
        .doc("Local address to listen on")
        .default("127.0.0.1:8791")
        .take(args)
        .then(parse_socket_addr)?;
    let tunnel_token_file = path_opt(
        args,
        "tunnel-token-file",
        Some("TUNNEL_TOKEN_FILE"),
        "Cloudflare Tunnel token file",
    )?;
    Ok(Command::Up {
        profile,
        public_url,
        addr,
        tunnel_token_file,
    })
}

#[cfg(feature = "network")]
fn parse_openai(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    let setup = noargs::cmd("setup")
        .doc("Create an OpenAI Secure MCP Tunnel through the Tunnel Management API")
        .take(args);
    if setup.is_present() {
        let name = noargs::opt("name")
            .ty("NAME")
            .doc("Operator-visible tunnel name")
            .default("Temote MCP")
            .take(args)
            .then(|opt| Ok::<_, std::convert::Infallible>(opt.value().to_owned()))?;
        let description = noargs::opt("description")
            .ty("TEXT")
            .doc("Operator-visible tunnel description")
            .default("Routes OpenAI Secure MCP Tunnel traffic to Temote MCP")
            .take(args)
            .then(|opt| Ok::<_, std::convert::Infallible>(opt.value().to_owned()))?;
        let organization_ids = repeated_string_opt(
            args,
            "organization-id",
            "Organization scope to attach; may be repeated",
        )?;
        let workspace_ids = repeated_string_opt(
            args,
            "workspace-id",
            "ChatGPT workspace scope to attach; may be repeated",
        )?;
        let config_file = path_opt(
            args,
            "config-file",
            None,
            "Override the local tunnel ID config file",
        )?;
        let force = noargs::flag("force")
            .doc("Create a new tunnel and replace an existing saved tunnel ID")
            .take(args)
            .is_present();
        return Ok(Command::Openai {
            command: OpenaiCommand::Setup {
                name,
                description,
                organization_ids,
                workspace_ids,
                config_file,
                force,
            },
        });
    }

    if args.metadata().help_mode {
        return Ok(Command::Openai {
            command: OpenaiCommand::Setup {
                name: "Temote MCP".to_owned(),
                description: "Routes OpenAI Secure MCP Tunnel traffic to Temote MCP".to_owned(),
                organization_ids: Vec::new(),
                workspace_ids: Vec::new(),
                config_file: None,
                force: false,
            },
        });
    }

    Err(noargs::Error::other(
        args,
        "OpenAI command is not specified (expected 'setup')",
    ))
}

#[cfg(feature = "network")]
fn parse_gateway_agent(args: &mut noargs::RawArgs) -> noargs::Result<Command> {
    let host_token = required_string_opt(
        args,
        "host-token",
        Some("TEMOTE_MCP_GATEWAY_HOST_TOKEN"),
        "Host credential; host mode binds it through HOST_TOKENS_JSON, legacy session mode uses HOST_TOKEN",
        "<secret>",
    )?;
    let access_client_id = string_opt_env(
        args,
        "access-client-id",
        Some("TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID"),
        "Optional Cloudflare Access service-token client ID",
    )?;
    let access_client_secret = string_opt_env(
        args,
        "access-client-secret",
        Some("TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET"),
        "Optional Cloudflare Access service-token client secret",
    )?;
    parse_gateway_agent_with_credentials(args, host_token, access_client_id, access_client_secret)
}

#[cfg(feature = "network")]
fn parse_gateway_agent_with_credentials(
    args: &mut noargs::RawArgs,
    host_token: String,
    access_client_id: Option<String>,
    access_client_secret: Option<String>,
) -> noargs::Result<Command> {
    let gateway_url = required_string_opt(
        args,
        "gateway-url",
        Some("TEMOTE_MCP_GATEWAY_URL"),
        "Cloudflare Worker origin, without a path",
        "https://gateway.example.com",
    )?;
    let session_id = string_opt_env(
        args,
        "session-id",
        None,
        "Legacy mode: active temote-mcp session to publish through the gateway",
    )?;
    let host_id = string_opt_env(
        args,
        "host-id",
        Some("TEMOTE_MCP_GATEWAY_HOST_ID"),
        "Federation mode: stable non-secret host ID representing the local supervisor",
    )?;
    if session_id.is_some() == host_id.is_some() {
        return Err(noargs::Error::other(
            args,
            "gateway-agent requires exactly one of --host-id or --session-id",
        ));
    }
    if validate_access_service_pair(access_client_id.as_deref(), access_client_secret.as_deref())
        .is_err()
    {
        return Err(noargs::Error::other(
            args,
            "Access service-token client ID and secret must be configured together",
        ));
    }
    let platform = noargs::opt("platform")
        .ty("PLATFORM")
        .doc("Host platform: auto, macos, linux, wsl2, or windows")
        .default("auto")
        .take(args)
        .then(|opt| gateway::Platform::from_str(opt.value()))?;
    let reconnect_delay_seconds = noargs::opt("reconnect-delay-seconds")
        .ty("SECONDS")
        .doc("Delay before reconnecting after a disconnect or generation replacement")
        .default("2")
        .take(args)
        .then(|opt| opt.value().parse::<u64>())?;
    Ok(Command::GatewayAgent {
        gateway_url,
        session_id,
        host_id,
        host_token,
        access_client_id,
        access_client_secret,
        platform,
        reconnect_delay_seconds,
    })
}

#[cfg(feature = "network")]
fn validate_access_service_pair(id: Option<&str>, secret: Option<&str>) -> Result<(), ()> {
    match (id, secret) {
        (None, None) => Ok(()),
        (Some(id), Some(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => Ok(()),
        _ => Err(()),
    }
}

#[cfg(feature = "network")]
fn required_string_opt(
    args: &mut noargs::RawArgs,
    name: &'static str,
    env: Option<&'static str>,
    doc: &'static str,
    example: &'static str,
) -> noargs::Result<String> {
    let mut spec = noargs::opt(name).ty("VALUE").doc(doc).example(example);
    if let Some(env) = env {
        spec = spec.env(env);
    }
    take_compatible_option(args, spec, env)
        .then(|opt| Ok::<_, std::convert::Infallible>(opt.value().to_owned()))
}

#[cfg(feature = "network")]
fn string_opt(
    args: &mut noargs::RawArgs,
    name: &'static str,
    doc: &'static str,
) -> noargs::Result<Option<String>> {
    string_opt_env(args, name, None, doc)
}

#[cfg(feature = "network")]
fn string_opt_env(
    args: &mut noargs::RawArgs,
    name: &'static str,
    env: Option<&'static str>,
    doc: &'static str,
) -> noargs::Result<Option<String>> {
    let mut spec = noargs::opt(name).ty("VALUE").doc(doc);
    if let Some(env) = env {
        spec = spec.env(env);
    }
    Ok(take_compatible_option(args, spec, env)
        .present()
        .map(|opt| opt.value().to_owned()))
}

#[cfg(feature = "network")]
fn path_opt(
    args: &mut noargs::RawArgs,
    name: &'static str,
    env: Option<&'static str>,
    doc: &'static str,
) -> noargs::Result<Option<PathBuf>> {
    let mut spec = noargs::opt(name).ty("PATH").doc(doc);
    if let Some(env) = env {
        spec = spec.env(env);
    }
    Ok(take_compatible_option(args, spec, env)
        .present()
        .map(|opt| PathBuf::from(opt.value())))
}

#[cfg(feature = "network")]
fn take_compatible_option(
    args: &mut noargs::RawArgs,
    spec: noargs::OptSpec,
    env: Option<&str>,
) -> noargs::Opt {
    let option = spec.take(args);
    // Explicit CLI input (including a missing value) always wins. Never put
    // an injected credential into help text, argv, or the process environment.
    if !args.metadata().help_mode
        && matches!(option, noargs::Opt::Env { .. } | noargs::Opt::None { .. })
        && let Some(value) = env.and_then(temote_mcp::environment::var_os)
    {
        return match value.into_string().ok().filter(|value| !value.is_empty()) {
            Some(value) => noargs::Opt::Env {
                spec: option.spec(),
                metadata: args.metadata(),
                value,
            },
            None => noargs::Opt::None {
                spec: option.spec(),
            },
        };
    }
    option
}

#[cfg(feature = "network")]
fn repeated_string_opt(
    args: &mut noargs::RawArgs,
    name: &'static str,
    doc: &'static str,
) -> noargs::Result<Vec<String>> {
    let spec = noargs::opt(name).ty("ID").doc(doc);
    let mut values = Vec::new();
    while let Some(value) = spec.take(args).present().map(|opt| opt.value().to_owned()) {
        values.push(value);
    }
    Ok(values)
}

fn finish(args: noargs::RawArgs, command: Command) -> Result<ParseOutcome, String> {
    match args.finish().map_err(format_error)? {
        Some(help) => Ok(ParseOutcome::Print(help)),
        None => Ok(ParseOutcome::run(Cli {
            command: Some(command),
        })),
    }
}

fn format_error(error: noargs::Error) -> String {
    format!("{error:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> impl Iterator<Item = String> {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn command(values: &[&str]) -> Command {
        match parse(argv(values)).unwrap() {
            ParseOutcome::Run(cli) => cli.command.unwrap(),
            ParseOutcome::Print(text) => panic!("unexpected output: {text}"),
        }
    }

    #[test]
    fn no_command_keeps_start_as_default() {
        assert!(matches!(
            command(&["temote-mcp"]),
            Command::Start {
                session_id: None,
                yolo: false
            }
        ));
    }

    #[cfg(feature = "network")]
    #[test]
    fn fabric_connect_and_link_select_the_approved_static_or_browser_lifecycle() {
        for prefix in [
            vec!["temote", "fabric", "link"],
            vec!["temote", "fabric", "connect"],
        ] {
            let mut args = prefix;
            args.extend([
                "--gateway-url",
                "https://fabric.example",
                "--host-id",
                "test-host",
                "--host-token",
                "test-token",
            ]);
            assert!(
                matches!(command(&args), Command::GatewayAgent { host_id:Some(id), .. } if id == "test-host")
            );
        }
        for prefix in [
            vec!["temote", "fabric", "connect"],
            vec!["temote", "fabric", "link"],
        ] {
            let mut args = prefix;
            args.extend([
                "--gateway-url",
                "https://fabric.example",
                "--oauth-issuer",
                "https://login.example",
                "--oauth-client-id",
                "public-client",
                "--host-id",
                "test-host",
            ]);
            assert!(matches!(
                command(&args),
                Command::FabricConnect { options }
                    if options.host_id.as_deref() == Some("test-host")
                        && options.resource == "https://fabric.example"
            ));
        }
        assert!(
            parse(argv(&[
                "temote",
                "fabric",
                "connect",
                "--gateway-url",
                "https://fabric.example",
                "--access-client-id",
                "service-id",
                "--access-client-secret",
                "service-secret",
            ]))
            .is_err()
        );
        assert!(
            parse(argv(&[
                "temote",
                "fabric",
                "link",
                "--gateway-url",
                "https://fabric.example",
                "--oauth-issuer",
                "https://login.example",
                "--oauth-client-id",
                "public-client",
                "--access-client-id",
                "service-id",
            ]))
            .is_err()
        );
        assert!(matches!(
            command(&[
                "temote", "fabric", "logout",
                "--gateway-url", "https://fabric.example",
                "--oauth-issuer", "https://login.example",
                "--oauth-client-id", "public-client",
                "--host-id", "test-host",
            ]),
            Command::FabricLogout { options } if options.host_id.as_deref() == Some("test-host")
        ));
        assert!(matches!(
            command(&["temote", "fabric", "status"]),
            Command::FabricStatus
        ));
        assert!(
            matches!(command(&["temote", "fabric", "events-sender"]), Command::EventsSender { addr } if addr.ip().is_loopback())
        );
        assert!(
            parse(argv(&[
                "temote",
                "fabric",
                "events-sender",
                "--addr",
                "0.0.0.0:4211"
            ]))
            .is_err()
        );
        assert!(matches!(
            parse(argv(&["temote", "fabric", "--help"])).unwrap(),
            ParseOutcome::Print(_)
        ));
    }

    #[test]
    fn start_parses_optional_id_and_yolo() {
        assert!(matches!(
            command(&["temote-mcp", "start", "work", "--yolo"]),
            Command::Start {
                session_id: Some(id),
                yolo: true
            } if id == "work"
        ));
    }

    #[test]
    fn start_flag_without_session_id_is_not_consumed_as_positional() {
        assert!(matches!(
            command(&["temote-mcp", "start", "--yolo"]),
            Command::Start {
                session_id: None,
                yolo: true
            }
        ));
    }

    #[test]
    fn start_rejects_unknown_options_instead_of_treating_them_as_session_ids() {
        assert!(parse(argv(&["temote-mcp", "start", "--wat"])).is_err());
        assert!(matches!(
            command(&["temote-mcp", "start", "--", "-dash-id"]),
            Command::Start {
                session_id: Some(id),
                yolo: false
            } if id == "-dash-id"
        ));
    }

    #[test]
    fn activity_cli_defaults_and_explicit_options_are_exact() {
        assert!(matches!(
            command(&["temote-mcp", "activity"]),
            Command::Activity {
                session_id: None,
                tail: 100,
                follow: true,
            }
        ));
        assert!(matches!(
            command(&[
                "temote-mcp",
                "activity",
                "sf",
                "--tail",
                "0",
                "--no-follow",
            ]),
            Command::Activity {
                session_id: Some(session_id),
                tail: 0,
                follow: false,
            } if session_id == "sf"
        ));
        assert!(matches!(
            command(&["temote-mcp", "activity", "--tail", "1024"]),
            Command::Activity {
                session_id: None,
                tail: 1024,
                follow: true,
            }
        ));
    }

    #[test]
    fn activity_cli_rejects_invalid_tail_session_and_extra_arguments() {
        for values in [
            vec!["temote-mcp", "activity", "--tail", "-1"],
            vec!["temote-mcp", "activity", "--tail", "1.5"],
            vec!["temote-mcp", "activity", "--tail", "1025"],
            vec!["temote-mcp", "activity", "--tail"],
            vec!["temote-mcp", "activity", "--unknown"],
            vec!["temote-mcp", "activity", "bad/session"],
            vec!["temote-mcp", "activity", "one", "two"],
        ] {
            assert!(parse(argv(&values)).is_err(), "accepted {values:?}");
        }
        assert!(
            parse(argv(&[
                "temote-mcp",
                "activity",
                "--tail",
                "184467440737095516160",
            ]))
            .is_err()
        );
    }

    #[test]
    fn session_forget_is_available_and_distinct_from_stop() {
        assert!(matches!(
            command(&["temote-mcp", "session", "forget", "my-session"]),
            Command::Session {
                command: SessionCommand::Forget { session_id },
            } if session_id == "my-session"
        ));
        let ParseOutcome::Print(help) = parse(argv(&["temote-mcp", "session", "--help"])).unwrap()
        else {
            panic!("expected forget help");
        };
        assert!(help.contains("forget"));
        assert!(help.contains("stop keeps metadata"));
    }

    #[test]
    fn session_start_requires_source_or_path_and_managed_retry_key() {
        assert!(matches!(
            command(&["temote-mcp", "session", "start", "--source", "owner/repo", "--operation-id", "67bcaaae-6e3f-492e-a2a7-606203976584"]),
            Command::Session { command: SessionCommand::StartManaged { source, operation_id, base: None, vcs } }
                if source == "owner/repo" && operation_id == "67bcaaae-6e3f-492e-a2a7-606203976584" && vcs == "auto"
        ));
        assert!(matches!(
            command(&["temote-mcp", "session", "start", "existing", "--path", "src/repo"]),
            Command::Session { command: SessionCommand::Start { session_id, path } }
                if session_id == "existing" && path == "src/repo"
        ));
        for args in [
            vec!["temote-mcp", "session", "start", "--source", "owner/repo"],
            vec![
                "temote-mcp",
                "session",
                "start",
                "--source",
                "owner/repo",
                "--path",
                "src/repo",
                "--operation-id",
                "67bcaaae-6e3f-492e-a2a7-606203976584",
            ],
            vec![
                "temote-mcp",
                "session",
                "start",
                "--operation-id",
                "67bcaaae-6e3f-492e-a2a7-606203976584",
            ],
        ] {
            assert!(parse(argv(&args)).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn session_gc_defaults_to_dry_run_and_bounds_the_limit() {
        assert!(matches!(
            command(&["temote-mcp", "session", "gc"]),
            Command::Session {
                command: SessionCommand::Gc {
                    apply: false,
                    limit: 100,
                },
            }
        ));
        assert!(matches!(
            command(&["temote-mcp", "session", "gc", "--apply", "--limit", "5"]),
            Command::Session {
                command: SessionCommand::Gc {
                    apply: true,
                    limit: 5,
                },
            }
        ));
        for values in [
            vec!["temote-mcp", "session", "gc", "--limit", "0"],
            vec!["temote-mcp", "session", "gc", "--limit", "1001"],
            vec!["temote-mcp", "session", "gc", "--limit", "not-a-number"],
        ] {
            assert!(parse(argv(&values)).is_err(), "accepted {values:?}");
        }
    }

    #[test]
    fn root_help_and_version_are_generated() {
        let ParseOutcome::Print(help) = parse(argv(&["temote-mcp", "--help"])).unwrap() else {
            panic!("expected help");
        };
        assert!(help.contains("Usage: temote [OPTIONS]"), "{help}");
        assert!(help.contains("doctor"));
        assert!(help.contains("start"));
        assert!(help.contains("mcp"));
        assert!(help.contains("codex"));
        assert!(help.contains("upgrade"));
        assert!(help.contains("activity"));
        assert!(help.contains("--version"));

        let ParseOutcome::Print(start_help) =
            parse(argv(&["temote-mcp", "start", "--help"])).unwrap()
        else {
            panic!("expected start help");
        };
        assert!(!start_help.contains("--version"));

        let ParseOutcome::Print(version) = parse(argv(&["temote-mcp", "-V"])).unwrap() else {
            panic!("expected version");
        };
        assert_eq!(version, format!("temote {}\n", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn codex_help_is_available_from_parser_surface() {
        let ParseOutcome::Print(help) = parse(argv(&["temote-mcp", "codex"])).unwrap() else {
            panic!("expected codex help");
        };
        assert!(help.contains("codex plugin install"));
        assert!(help.contains("codex diagnose"));
    }

    #[test]
    fn upgrade_and_supervisor_handoff_options_parse() {
        assert!(matches!(
            command(&["temote-mcp", "upgrade", "--dry-run"]),
            Command::Upgrade {
                dry_run: true,
                force: false,
            }
        ));
        assert!(matches!(
            command(&["temote-mcp", "upgrade", "--force"]),
            Command::Upgrade {
                dry_run: false,
                force: true,
            }
        ));
        assert!(matches!(
            command(&["temote-mcp", "supervisor", "--capabilities"]),
            Command::Supervisor {
                restore_plan: None,
                capabilities: true,
            }
        ));
        assert!(matches!(
            command(&[
                "temote-mcp",
                "supervisor",
                "--restore-plan",
                "/tmp/plan.json",
            ]),
            Command::Supervisor {
                restore_plan: Some(path),
                capabilities: false,
            } if path == std::path::Path::new("/tmp/plan.json")
        ));
    }

    #[test]
    fn invalid_command_and_profile_fail_closed() {
        assert!(parse(argv(&["temote-mcp", "wat"])).is_err());
        assert!(parse(argv(&["temote-mcp", "doctor", "--profile", "wat"])).is_err());
    }

    #[test]
    fn session_permission_commands_parse_explicitly() {
        assert!(matches!(
            command(&["temote-mcp", "session", "permission", "work", "status"]),
            Command::Session {
                command: SessionCommand::Permission {
                    session_id,
                    command: SessionPermissionCommand::Status,
                }
            } if session_id == "work"
        ));
        assert!(matches!(
            command(&[
                "temote-mcp",
                "session",
                "permission",
                "work",
                "allow",
                "/tmp/example",
            ]),
            Command::Session {
                command: SessionCommand::Permission {
                    session_id,
                    command: SessionPermissionCommand::Allow { path },
                }
            } if session_id == "work" && path == std::path::Path::new("/tmp/example")
        ));
        assert!(matches!(
            command(&["temote-mcp", "session", "permission", "work", "agent"]),
            Command::Session {
                command: SessionCommand::Permission {
                    session_id,
                    command: SessionPermissionCommand::Agent,
                }
            } if session_id == "work"
        ));
        assert!(matches!(
            command(&["temote-mcp", "session", "permission", "work", "yolo"]),
            Command::Session {
                command: SessionCommand::Permission {
                    session_id,
                    command: SessionPermissionCommand::Yolo,
                }
            } if session_id == "work"
        ));
    }

    #[cfg(feature = "network")]
    #[test]
    fn network_defaults_match_previous_cli_surface() {
        match command(&["temote-mcp", "serve"]) {
            Command::Serve {
                profile,
                addr,
                public_url,
                tunnel_token_file,
            } => {
                assert_eq!(profile, profile::Profile::Cloudflare);
                assert_eq!(addr, "127.0.0.1:8791".parse().unwrap());
                assert!(public_url.is_none());
                assert!(tunnel_token_file.is_none());
            }
            _ => panic!("expected serve"),
        }
    }

    #[cfg(feature = "network")]
    #[test]
    fn openai_help_lists_setup_without_requiring_it() {
        let ParseOutcome::Print(help) = parse(argv(&["temote-mcp", "openai", "--help"])).unwrap()
        else {
            panic!("expected help");
        };
        assert!(help.contains("setup"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn repeated_openai_scopes_preserve_order() {
        match command(&[
            "temote-mcp",
            "openai",
            "setup",
            "--organization-id",
            "o1",
            "--workspace-id=w1",
            "--organization-id=o2",
        ]) {
            Command::Openai {
                command:
                    OpenaiCommand::Setup {
                        organization_ids,
                        workspace_ids,
                        ..
                    },
            } => {
                assert_eq!(organization_ids, ["o1", "o2"]);
                assert_eq!(workspace_ids, ["w1"]);
            }
            _ => panic!("expected openai setup"),
        }
    }

    #[cfg(feature = "network")]
    #[test]
    fn gateway_agent_requires_credentials_and_parses_platform() {
        assert!(parse(argv(&["temote-mcp", "gateway-agent"])).is_err());
        match command(&[
            "temote-mcp",
            "gateway-agent",
            "--gateway-url",
            "https://example.test",
            "--host-id",
            "linux-main",
            "--host-token",
            "secret",
            "--platform",
            "linux",
            "--reconnect-delay-seconds",
            "7",
        ]) {
            Command::GatewayAgent {
                host_id,
                session_id,
                platform,
                reconnect_delay_seconds,
                ..
            } => {
                assert_eq!(host_id.as_deref(), Some("linux-main"));
                assert_eq!(session_id, None);
                assert_eq!(platform, gateway::Platform::Linux);
                assert_eq!(reconnect_delay_seconds, 7);
            }
            _ => panic!("expected gateway-agent"),
        }
        match command(&[
            "temote-mcp",
            "gateway-agent",
            "--gateway-url",
            "https://example.test",
            "--session-id",
            "legacy-session",
            "--host-token",
            "secret",
        ]) {
            Command::GatewayAgent {
                host_id,
                session_id,
                ..
            } => {
                assert_eq!(host_id, None);
                assert_eq!(session_id.as_deref(), Some("legacy-session"));
            }
            _ => panic!("expected legacy gateway-agent"),
        }
        assert!(
            parse(argv(&[
                "temote-mcp",
                "gateway-agent",
                "--gateway-url",
                "https://example.test",
                "--host-id",
                "linux-main",
                "--session-id",
                "legacy-session",
                "--host-token",
                "secret",
            ]))
            .is_err()
        );
    }
}
