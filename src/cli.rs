use std::path::PathBuf;
#[cfg(feature = "network")]
use std::{net::SocketAddr, str::FromStr};

#[cfg(feature = "network")]
use crate::gateway;
use crate::profile;

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
    Supervisor,
    Session {
        command: SessionCommand,
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
        session_id: String,
        host_token: String,
        access_client_id: Option<String>,
        access_client_secret: Option<String>,
        platform: gateway::Platform,
        reconnect_delay_seconds: u64,
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
    Start { session_id: String, path: String },
    List,
    Info { session_id: String },
    Stop { session_id: String },
    Restart { session_id: String },
    Console,
}

pub enum ParseOutcome {
    Run(Cli),
    Print(String),
}

pub fn parse_env() -> Result<ParseOutcome, String> {
    parse(std::env::args())
}

fn parse<I>(raw: I) -> Result<ParseOutcome, String>
where
    I: Iterator<Item = String>,
{
    let raw = raw.collect::<Vec<_>>();
    if matches!(raw.get(1).map(String::as_str), Some("--version" | "-V")) {
        return Ok(ParseOutcome::Print(format!(
            "{} {}\n",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        )));
    }

    let mut args = noargs::RawArgs::new(raw.into_iter());
    args.metadata_mut().app_name = env!("CARGO_PKG_NAME");
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
        return Ok(ParseOutcome::Run(Cli {
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
        return finish(args, Command::Supervisor);
    }
    if noargs::cmd("session")
        .doc("Manage sessions owned by the local Temote supervisor")
        .take(&mut args)
        .is_present()
    {
        let command = parse_session(&mut args).map_err(format_error)?;
        return finish(args, Command::Session { command });
    }
    if noargs::cmd("mcp")
        .doc("Run the session-independent MCP server over stdin/stdout")
        .take(&mut args)
        .is_present()
    {
        return finish(args, Command::Mcp);
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

fn parse_session(args: &mut noargs::RawArgs) -> noargs::Result<SessionCommand> {
    if noargs::cmd("start")
        .doc("Start a supervisor-owned session under a configured named root")
        .take(args)
        .is_present()
    {
        let path = noargs::opt("path")
            .ty("PATH")
            .doc("Named-root-relative path such as src/my-project")
            .take(args)
            .then(|opt| Ok::<_, std::convert::Infallible>(opt.value().to_owned()))?;
        let session_id = noargs::arg("<SESSION_ID>")
            .doc("Session ID")
            .take(args)
            .then(|arg| Ok::<_, std::convert::Infallible>(arg.value().to_owned()))?;
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
        "session command is not specified (expected start, list, info, stop, restart, or console)",
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
    let gateway_url = required_string_opt(
        args,
        "gateway-url",
        Some("TEMOTE_MCP_GATEWAY_URL"),
        "Cloudflare Worker origin, without a path",
        "https://gateway.example.com",
    )?;
    let session_id = required_string_opt(
        args,
        "session-id",
        None,
        "Active temote-mcp session to publish through the gateway",
        "my-session",
    )?;
    let host_token = required_string_opt(
        args,
        "host-token",
        Some("TEMOTE_MCP_GATEWAY_HOST_TOKEN"),
        "Shared host credential stored as the Worker's HOST_TOKEN secret",
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
        host_token,
        access_client_id,
        access_client_secret,
        platform,
        reconnect_delay_seconds,
    })
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
    spec.take(args)
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
    Ok(spec.take(args).present().map(|opt| opt.value().to_owned()))
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
    Ok(spec
        .take(args)
        .present()
        .map(|opt| PathBuf::from(opt.value())))
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
        None => Ok(ParseOutcome::Run(Cli {
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
    fn root_help_and_version_are_generated() {
        let ParseOutcome::Print(help) = parse(argv(&["temote-mcp", "--help"])).unwrap() else {
            panic!("expected help");
        };
        assert!(help.contains("doctor"));
        assert!(help.contains("start"));
        assert!(help.contains("mcp"));
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
        assert!(version.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn invalid_command_and_profile_fail_closed() {
        assert!(parse(argv(&["temote-mcp", "wat"])).is_err());
        assert!(parse(argv(&["temote-mcp", "doctor", "--profile", "wat"])).is_err());
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
            "--session-id",
            "s",
            "--host-token",
            "secret",
            "--platform",
            "linux",
            "--reconnect-delay-seconds",
            "7",
        ]) {
            Command::GatewayAgent {
                platform,
                reconnect_delay_seconds,
                ..
            } => {
                assert_eq!(platform, gateway::Platform::Linux);
                assert_eq!(reconnect_delay_seconds, 7);
            }
            _ => panic!("expected gateway-agent"),
        }
    }
}
