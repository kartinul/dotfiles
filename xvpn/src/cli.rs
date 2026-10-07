//! The command-line surface, and the dispatch for it.
//!
//! This is the only file with real documentation, because `xvpn --help` and
//! this file are the same thing: everything here is what a user reads.

use clap::{Parser, Subcommand, ValueEnum};

use crate::{commands, config};

#[derive(Parser)]
#[command(name = "xvpn", version, about = "xray VPN manager")]
pub struct Cli {
    /// Index into the profile list (0 = off, 1 = first profile, etc.)
    #[arg(value_parser, default_value_t = 0)]
    pub index: u16,

    /// Command to run with its arguments (everything after the index)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Run the supervisor in the foreground (used by launchd/systemd)
    #[arg(long, hide = true)]
    pub supervise: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Route traffic: on | default | off
    Set {
        #[arg(value_enum)]
        mode: Mode,
    },
    /// Manage the apps routed in default mode
    Apps {
        #[arg(value_enum, default_value_t = AppsAction::Show)]
        action: AppsAction,
        /// App bundle name, required for set and remove
        app: Option<String>,
    },
    /// Manage the sites routed in default mode
    Sites {
        #[arg(value_enum, default_value_t = SitesAction::Show)]
        action: SitesAction,
        /// Domain, required for set and remove. Covers subdomains too.
        site: Option<String>,
    },
    /// Import a vless:// link into the active profile
    Import {
        /// vless:// link (prompts if omitted)
        link: Option<String>,
        /// Overwrite the profile if it already exists
        #[arg(long)]
        force: bool,
    },
    /// Manage named VPN profiles
    #[command(subcommand)]
    Profile(ProfileCmd),

    // Reachable, but out of the base listing.
    /// Use selective routing on the network you are on now
    #[command(hide = true)]
    Use,
    /// Stop using selective routing on the current network
    #[command(hide = true)]
    Forget {
        /// Resolver to forget; defaults to the current network's
        dns: Option<String>,
    },
    /// Alias for `set on`
    #[command(hide = true)]
    On,
    /// Alias for `set default`
    #[command(hide = true)]
    Default,
    /// Alias for `set off`
    #[command(hide = true)]
    Off,
    /// Alias for bare `xvpn`
    #[command(hide = true)]
    Status,
    /// Alias for `apps set`
    #[command(hide = true)]
    Add { app: String },
    /// Alias for `apps remove`
    #[command(hide = true)]
    Remove { app: String },

    /// Curl ifconfig.me through the running xray proxy
    Check,
    /// Rewrite sing-box configs onto the current schema
    Repair,
    /// Delete generated configs and restore stock defaults
    Reset,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Route this machine's traffic through the VPN
    On,
    /// Route only listed apps and tools through the VPN
    Default,
    /// Route nothing
    Off,
}

impl Mode {
    /// The literal written to the `mode` file, which the supervisor reads.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::On => "on",
            Mode::Default => "default",
            Mode::Off => "off",
        }
    }
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppsAction {
    /// List the routed apps
    Show,
    /// Route an app through the VPN
    #[value(alias = "add")]
    Set,
    /// Stop routing an app
    #[value(alias = "rm", alias = "delete")]
    Remove,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SitesAction {
    /// List the routed sites
    Show,
    /// Route a domain through the VPN
    #[value(alias = "add")]
    Set,
    /// Stop routing a domain
    #[value(alias = "rm", alias = "delete")]
    Remove,
}

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// Store a vless:// link under a name
    Add {
        name: String,
        /// vless:// link (prompts if omitted)
        link: Option<String>,
        /// Overwrite an existing profile
        #[arg(long)]
        force: bool,
    },
    /// List stored profiles
    List,
    /// Switch to a profile
    Use {
        /// Profile name, or its position from `profile list`
        name: String,
    },
    /// Delete a profile
    Remove {
        /// Profile name, or its position from `profile list`
        name: String,
    },
    /// Print a profile's config
    Show {
        /// Name or position; defaults to the active profile
        name: Option<String>,
    },
}

/// Checked before anything is written: `show` takes no name, `set`/`remove`
/// require one.
pub fn apps_args(
    action: AppsAction,
    app: Option<String>,
) -> Result<(AppsAction, Option<String>), String> {
    match (&action, &app) {
        (AppsAction::Show, None) => Ok((action, None)),
        (AppsAction::Show, Some(_)) => Err("`apps show` takes no app name".to_string()),
        (_, None) => Err(format!(
            "`apps {}` needs an app name (e.g. `xvpn apps {} Telegram`)",
            name_of(action),
            name_of(action)
        )),
        _ => Ok((action, app)),
    }
}

fn name_of(action: AppsAction) -> &'static str {
    match action {
        AppsAction::Show => "show",
        AppsAction::Set => "set",
        AppsAction::Remove => "remove",
    }
}

/// `sites_args` mirrors `apps_args`: checked before anything is written.
pub fn sites_args(
    action: SitesAction,
    site: Option<String>,
) -> Result<(SitesAction, Option<String>), String> {
    match (&action, &site) {
        (SitesAction::Show, None) => Ok((action, None)),
        (SitesAction::Show, Some(_)) => Err("`sites show` takes no domain".to_string()),
        (_, None) => Err(format!(
            "`sites {}` needs a domain (e.g. `xvpn sites {} netflix.com`)",
            site_action_name(action),
            site_action_name(action)
        )),
        _ => Ok((action, site)),
    }
}

fn site_action_name(action: SitesAction) -> &'static str {
    match action {
        SitesAction::Show => "show",
        SitesAction::Set => "set",
        SitesAction::Remove => "remove",
    }
}

pub fn dispatch(cli: Cli) {
    let root = config::root();

    if cli.supervise {
        commands::run(crate::supervisor::supervise(&root));
        return;
    }

    let result = match cli.command {
        None => {
            if cli.index > 0 || !cli.cmd.is_empty() {
                commands::run_app(&root, cli.index, &cli.cmd)
            } else {
                commands::status(&root)
            }
        }
        Some(Command::Status) => commands::status(&root),
        Some(Command::Set { mode }) => commands::set_mode(&root, mode),
        Some(Command::On) => commands::set_mode(&root, Mode::On),
        Some(Command::Default) => commands::set_mode(&root, Mode::Default),
        Some(Command::Off) => commands::set_mode(&root, Mode::Off),
        Some(Command::Apps { action, app }) => commands::apps(&root, action, app),
        Some(Command::Sites { action, site }) => commands::sites(&root, action, site),
        Some(Command::Add { app }) => commands::add_app(&root, &app),
        Some(Command::Remove { app }) => commands::remove_app(&root, &app),
        Some(Command::Import { link, force }) => commands::import(&root, link.as_deref(), force),
        Some(Command::Profile(cmd)) => commands::profile(&root, cmd),
        Some(Command::Use) => commands::use_network(&root, true, None),
        Some(Command::Forget { dns }) => commands::use_network(&root, false, dns.as_deref()),

        Some(Command::Check) => commands::check_vpn(&root),
        Some(Command::Repair) => commands::repair(&root),
        Some(Command::Reset) => commands::reset(&root),
    };

    commands::run(result);
}
