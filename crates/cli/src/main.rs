//! The `sotto` CLI — local, end-to-end-encrypted secret management.
//!
//! This binary is the IO layer: it parses arguments, resolves paths and config, prompts for the
//! master password (hidden, or `SOTTO_PASSWORD`), enforces TTY-safe output, and renders results.
//! All logic lives in the `sotto_cli` library.

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use clap::{CommandFactory, Parser, Subcommand};
use zeroize::{Zeroize, Zeroizing};

use sotto_cli::commands::App;
use sotto_cli::config::{self, Config};
use sotto_cli::dotenv;
use sotto_cli::error::{Error, Result};
use sotto_cli::export::{self, ExportFormat};
use sotto_cli::keychain::{Keychain, OsKeychain};
use sotto_cli::remote;
use sotto_cli::session;
use sotto_cli::store::Store;
use sotto_cli::vault::Vault;

/// How long an unlocked session lasts before the master password is needed again.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Keychain service name under which the secret key and session are stored.
const KEYCHAIN_SERVICE: &str = "sotto";

#[derive(Parser)]
#[command(
    name = "sotto",
    version,
    about = "End-to-end-encrypted secret management"
)]
struct Cli {
    /// Use this environment for this command (overrides the project's configured default).
    #[arg(long, global = true)]
    env: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set up the current directory as a Sotto project (creates an identity on first run).
    Init {
        /// Project name (defaults to the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Create the project inside an organization (shareable with that team).
        #[arg(long)]
        org: Option<String>,
    },
    /// Manage organizations (teams).
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },
    /// Share the active environment with an organization member (by their user id).
    Grant {
        /// The member's user id (from `sotto org invite` or `sotto org members`).
        user_id: String,
    },
    /// Rotate the active environment's vault key (re-key + re-grant its current members).
    Rotate,
    /// Manage machine tokens (CI / service access) for the active environment.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Clone a shared environment onto this device (run in the destination directory).
    Clone {
        /// The project id (told to you by whoever granted access).
        project_id: String,
        /// The environment id to clone.
        env_id: String,
        /// Local name for the cloned environment (defaults to its real name via the org key).
        #[arg(long = "as")]
        as_name: Option<String>,
        /// Local label for the project (defaults to "shared").
        #[arg(long)]
        name: Option<String>,
        /// The owning organization id (from the grantor), so later pushes match the server.
        #[arg(long)]
        org: Option<String>,
    },
    /// Log in to the sync server (opens a browser for OAuth).
    Login {
        /// The sync server URL (persisted; required on first login).
        #[arg(long)]
        server: Option<String>,
        /// The web app origin, for building share links (defaults to the server URL).
        #[arg(long)]
        web: Option<String>,
    },
    /// Log out of the sync server (clear the stored session token).
    Logout,
    /// Create a one-time / expiring share link for a secret and print it.
    Share {
        /// The secret name to share.
        name: String,
        /// How many times the link may be viewed before it burns.
        #[arg(long, default_value_t = 1)]
        views: i32,
        /// Link lifetime in seconds (default: no expiry).
        #[arg(long)]
        expire: Option<i64>,
        /// Protect the link with a passphrase (prompted; a second factor beyond the link).
        #[arg(long)]
        passphrase: bool,
    },
    /// Upload local changes for the active environment to the server.
    Push,
    /// Download the active environment's secrets from the server into the local store.
    Pull,
    /// Set up this device from the server using your Emergency Kit (run after `login`).
    Setup,
    /// DANGER: reset your account with fresh keys (for a lost Emergency Kit; run after `login`).
    /// Everything encrypted under the old keys becomes permanently unreadable; org admins must
    /// re-grant your shared environments.
    Reset {
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Unlock the store for this session.
    Unlock,
    /// Lock the store (clear the cached session).
    Lock,
    /// Show identity, session, and project status.
    Status {
        /// Output as a JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Set a secret. Reads the value from a hidden prompt unless --value/--stdin is given.
    Set {
        name: String,
        /// Provide the value inline (warning: visible in shell history).
        #[arg(long, conflicts_with = "stdin")]
        value: Option<String>,
        /// Read the value from standard input.
        #[arg(long)]
        stdin: bool,
    },
    /// Print a secret's value. Refuses to print to a terminal without --reveal.
    Get {
        name: String,
        /// Allow printing the secret to a terminal.
        #[arg(long)]
        reveal: bool,
    },
    /// List secret names in the active environment.
    Ls {
        /// Output as a JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Remove a secret.
    Rm { name: String },
    /// Show a secret's version history (from the server; run after `login`).
    History {
        name: String,
        /// Show each version's value, not just its number and size.
        #[arg(long)]
        reveal: bool,
    },
    /// Restore an old version of a secret as a new version (local until you `push`).
    Rollback {
        name: String,
        /// The version to restore (see `sotto history`).
        version: i64,
    },
    /// Run a command with the environment's secrets injected as environment variables.
    Run {
        /// The command and its arguments (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the environment's secrets in a chosen format (plaintext).
    Export {
        /// Output format.
        #[arg(long, value_enum, default_value_t = ExportFormat::Dotenv)]
        format: ExportFormat,
        /// Allow writing to a terminal.
        #[arg(long)]
        reveal: bool,
    },
    /// Import secrets from a .env file into the active environment.
    Import {
        /// Path to the .env file.
        file: PathBuf,
    },
    /// Manage environments.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Generate a shell completion script (to stdout).
    Completions {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum EnvCommand {
    /// List the project's environments (the active one is marked).
    Ls,
    /// Set the active environment for this project.
    Use { name: String },
    /// Compare two environments key by key (presence + "differs" markers).
    Diff {
        left: String,
        right: String,
        /// Show the differing values, not just markers.
        #[arg(long)]
        reveal: bool,
    },
    /// Copy secrets from one environment to another (promotion). Dry-run by default;
    /// adds and updates only — never deletes destination keys.
    Copy {
        src: String,
        dst: String,
        /// Actually apply the copy (otherwise only the plan is printed).
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Create a machine token for the active environment; prints the SOTTO_TOKEN once.
    Create {
        /// Human label for the token ("github-actions").
        #[arg(long, default_value = "ci")]
        name: String,
    },
    /// List the active environment's machine tokens.
    Ls,
    /// Revoke a machine token (its access dies immediately; also run `sotto rotate` to re-key).
    Revoke { token_id: String },
}

#[derive(Subcommand)]
enum OrgCommand {
    /// Create an organization; prints its id.
    Create { name: String },
    /// List your organizations.
    Ls,
    /// Invite an existing Sotto user into an org by email; prints their user id.
    Invite { org_id: String, email: String },
    /// List an org's members and their ids.
    Members { org_id: String },
    /// Remove a member and rotate every environment they could access.
    Remove { org_id: String, user_id: String },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(e.exit_code());
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Completions need neither the store nor the keychain — handle before touching either.
    if let Command::Completions { shell } = &cli.command {
        clap_complete::generate(*shell, &mut Cli::command(), "sotto", &mut io::stdout());
        return Ok(());
    }

    // Machine mode: with SOTTO_TOKEN set, `run`/`export` decrypt entirely in memory — no store,
    // keychain, config, or password. This is the CI path.
    if let Ok(token) = std::env::var("SOTTO_TOKEN") {
        match &cli.command {
            Command::Run { args } => return machine_run(&token, args.clone()),
            Command::Export { format, reveal } => return machine_export(&token, *format, *reveal),
            _ => {} // every other command proceeds as a normal session
        }
    }

    let store_path = sotto_cli::paths::store_path()?;
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Io(format!("creating {}: {e}", parent.display())))?;
    }
    let store = Store::open(&store_path)?;
    let keychain = OsKeychain::new(KEYCHAIN_SERVICE);
    let app = App::new(&store, &keychain);
    let cwd = std::env::current_dir().map_err(|e| Error::Io(e.to_string()))?;

    match cli.command {
        Command::Init { name, org } => init(&store, &keychain, &cwd, name, org),
        Command::Org { command } => org_command(&store, &keychain, command),
        Command::Grant { user_id } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            grant_env(&store, &keychain, &config, &user_id)
        }
        Command::Rotate => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            rotate_active(&store, &keychain, &config)
        }
        Command::Token { command } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            token_command(&store, &keychain, &config, command)
        }
        Command::Clone {
            project_id,
            env_id,
            as_name,
            name,
            org,
        } => {
            ensure_unlocked(&store, &keychain)?;
            clone_env(
                &store,
                &keychain,
                &cwd,
                &project_id,
                &env_id,
                name.as_deref(),
                as_name.as_deref(),
                org.as_deref(),
            )
        }
        Command::Login { server, web } => login(&keychain, server.as_deref(), web.as_deref()),
        Command::Logout => {
            remote::auth::clear_session(&keychain)?;
            eprintln!("logged out");
            Ok(())
        }
        Command::Share {
            name,
            views,
            expire,
            passphrase,
        } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            share(&app, &keychain, &config, &name, views, expire, passphrase)
        }
        Command::Push => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            let master = session::current_master_key(&keychain)?.ok_or(Error::Locked)?;
            let client = sync_client(&keychain)?;
            let revision = remote::sync::push(&client, &store, master.as_bytes(), &config)?;
            eprintln!(
                "pushed {}/{} — revision {revision}",
                config.project, config.environment
            );
            Ok(())
        }
        Command::Pull => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            let client = sync_client(&keychain)?;
            let revision = remote::sync::pull(&client, &store, &config)?;
            eprintln!(
                "pulled {}/{} — revision {revision}",
                config.project, config.environment
            );
            Ok(())
        }
        Command::Setup => setup(&store, &keychain, &cwd),
        Command::Reset { yes } => reset(&store, &keychain, yes),
        Command::Unlock => {
            ensure_unlocked(&store, &keychain)?;
            eprintln!("unlocked");
            Ok(())
        }
        Command::Lock => {
            session::lock(&keychain)?;
            eprintln!("locked");
            Ok(())
        }
        Command::Status { json } => status(&app, &cwd, json),
        Command::Set { name, value, stdin } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            let mut value = read_value(value, stdin)?;
            let result = app.set(&config, &name, &value);
            value.zeroize();
            result?;
            eprintln!("set {name} ({}/{})", config.project, config.environment);
            Ok(())
        }
        Command::Get { name, reveal } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            let mut value = app.get(&config, &name)?;
            let result = write_value(&value, reveal);
            value.zeroize();
            result
        }
        Command::Ls { json } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            let names = app.list(&config)?;
            if json {
                println!("{}", to_json(&names)?);
            } else {
                for name in names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        Command::Rm { name } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            app.remove(&config, &name)?;
            eprintln!("removed {name}");
            Ok(())
        }
        Command::History { name, reveal } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            history(&store, &keychain, &config, &name, reveal)
        }
        Command::Rollback { name, version } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            rollback(&store, &keychain, &config, &name, version)
        }
        Command::Run { args } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            run_injected(&app, &config, args)
        }
        Command::Export { format, reveal } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            export_secrets(&app, &config, format, reveal)
        }
        Command::Import { file } => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            import_dotenv(&app, &config, &file)
        }
        Command::Env { command } => match command {
            EnvCommand::Ls => {
                let config = effective_config(&cwd, cli.env.as_deref())?;
                for env in app.env_list(&config)? {
                    let marker = if env == config.environment { "*" } else { " " };
                    println!("{marker} {env}");
                }
                Ok(())
            }
            EnvCommand::Use { name } => env_use(&store, &cwd, &name),
            EnvCommand::Diff {
                left,
                right,
                reveal,
            } => {
                let config = effective_config(&cwd, cli.env.as_deref())?;
                ensure_unlocked(&store, &keychain)?;
                env_diff(&app, &config, &left, &right, reveal)
            }
            EnvCommand::Copy { src, dst, confirm } => {
                let config = effective_config(&cwd, cli.env.as_deref())?;
                ensure_unlocked(&store, &keychain)?;
                env_copy(&app, &config, &src, &dst, confirm)
            }
        },
        Command::Completions { .. } => unreachable!("completions are handled before store init"),
    }
}

fn init(
    store: &Store,
    keychain: &dyn Keychain,
    cwd: &Path,
    name: Option<String>,
    org: Option<String>,
) -> Result<()> {
    if cwd.join(config::CONFIG_FILE).exists() {
        return Err(Error::Input(format!(
            "{} already exists in this directory",
            config::CONFIG_FILE
        )));
    }

    if store.get_identity()?.is_none() {
        let mut password = read_new_password()?;
        let kit = session::init(store, keychain, &password, SESSION_TTL);
        password.zeroize();
        let kit = kit?;
        eprintln!();
        eprintln!("  Save your Emergency Kit — these cannot be recovered:");
        eprintln!("    Secret Key:   {}", kit.secret_key);
        eprintln!("    Recovery Key: {}", kit.recovery_key);
        eprintln!();
    } else {
        ensure_unlocked(store, keychain)?;
    }

    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    let keypair = session::account_keypair(store, &master)?;
    let project_name = name.unwrap_or_else(|| {
        cwd.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string())
    });
    let project = Vault::create_project(store, &keypair, &project_name)?;
    let config = Config {
        project_id: project.id,
        project: project_name,
        environment: "dev".to_string(),
        org_id: org,
    };
    config.save_to(cwd)?;
    match &config.org_id {
        Some(org) => eprintln!(
            "initialized `{}` ({}) in organization {org}",
            config.project, config.environment
        ),
        None => eprintln!("initialized `{}` ({})", config.project, config.environment),
    }
    Ok(())
}

/// Organization management: create/list orgs, invite members, list members.
fn org_command(store: &Store, keychain: &dyn Keychain, command: OrgCommand) -> Result<()> {
    let client = sync_client(keychain)?;
    match command {
        OrgCommand::Create { name } => {
            ensure_unlocked(store, keychain)?;
            let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
            let keypair = session::account_keypair(store, &master)?;
            let id = remote::team::create_org(&client, &keypair, &name)?;
            eprintln!("created organization `{name}`");
            println!("{id}");
            Ok(())
        }
        OrgCommand::Ls => {
            ensure_unlocked(store, keychain)?;
            let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
            let keypair = session::account_keypair(store, &master)?;
            for org in remote::team::list_orgs(&client, &keypair)? {
                println!("{}  {}  ({})", org.id, org.name, org.role);
            }
            Ok(())
        }
        OrgCommand::Invite { org_id, email } => {
            // Inviting also grants the invitee the org key (sealed to their public key), so the
            // caller must be unlocked to open their own copy.
            ensure_unlocked(store, keychain)?;
            let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
            let keypair = session::account_keypair(store, &master)?;
            let invited = remote::team::invite(&client, &keypair, &org_id, &email)?;
            let keys = if invited.public_key.is_some() {
                "ready to receive shares"
            } else {
                "no account keys yet — they must log in and set up before you can share"
            };
            eprintln!("invited {email} — {keys}");
            println!("{}", invited.user_id);
            Ok(())
        }
        OrgCommand::Members { org_id } => {
            for m in remote::team::members(&client, &org_id)? {
                let keys = if m.public_key.is_some() {
                    "keys"
                } else {
                    "no-keys"
                };
                println!("{}  ({})  [{keys}]", m.user_id, m.role);
            }
            Ok(())
        }
        OrgCommand::Remove { org_id, user_id } => {
            ensure_unlocked(store, keychain)?;
            let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
            let keypair = session::account_keypair(store, &master)?;
            let report = remote::team::remove_member(&client, &keypair, &org_id, &user_id)?;
            eprintln!(
                "removed {user_id}; rotated {} environment(s)",
                report.rotated.len()
            );
            if !report.skipped.is_empty() {
                eprintln!(
                    "warning: {} environment(s) you can't open were not rotated — ask a member \
                     who holds them to run `sotto rotate`: {}",
                    report.skipped.len(),
                    report.skipped.join(", ")
                );
            }
            Ok(())
        }
    }
}

/// Machine-token management for the active environment.
fn token_command(
    store: &Store,
    keychain: &dyn Keychain,
    config: &Config,
    command: TokenCommand,
) -> Result<()> {
    let client = sync_client(keychain)?;
    let env = store
        .get_environment(&config.project_id, &config.environment)?
        .ok_or_else(|| Error::NotFound(format!("environment `{}`", config.environment)))?;
    match command {
        TokenCommand::Create { name } => {
            ensure_unlocked(store, keychain)?;
            let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
            let keypair = session::account_keypair(store, &master)?;
            let token =
                remote::team::create_machine_token(&client, store, &keypair, config, &name)?;
            eprintln!(
                "machine token `{name}` for {}/{} — save it now; it is never shown again:",
                config.project, config.environment
            );
            println!("{token}");
            Ok(())
        }
        TokenCommand::Ls => {
            for t in remote::SyncApi::list_machine_tokens(&client, &env.id)? {
                println!("{}  {}", t.token_id, t.name);
            }
            Ok(())
        }
        TokenCommand::Revoke { token_id } => {
            remote::SyncApi::revoke_machine_token(&client, &env.id, &token_id)?;
            eprintln!("revoked {token_id}; run `sotto rotate` to also re-key the environment");
            Ok(())
        }
    }
}

/// Rotate the active environment's vault key, then pull to adopt the new key locally.
fn rotate_active(store: &Store, keychain: &dyn Keychain, config: &Config) -> Result<()> {
    let org_id = config.org_id.as_deref().ok_or_else(|| {
        Error::Input("rotation only applies to environments in an organization".into())
    })?;
    let env = store
        .get_environment(&config.project_id, &config.environment)?
        .ok_or_else(|| Error::NotFound(format!("environment `{}`", config.environment)))?;
    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    let keypair = session::account_keypair(store, &master)?;
    let client = sync_client(keychain)?;
    match remote::team::rotate_env(&client, &keypair, org_id, &env.id, None)? {
        Some(rev) => {
            // Adopt the new key + rewrapped data keys into the local store.
            remote::sync::pull(&client, store, config)?;
            eprintln!(
                "rotated {}/{} — revision {rev}",
                config.project, config.environment
            );
            Ok(())
        }
        None => Err(Error::Input(
            "you don't have a grant to this environment, so you can't rotate it".into(),
        )),
    }
}

/// Share the active environment with an org member: reseal its vault key to them and upload it.
fn grant_env(store: &Store, keychain: &dyn Keychain, config: &Config, user_id: &str) -> Result<()> {
    let org_id = config.org_id.as_deref().ok_or_else(|| {
        Error::Input(
            "this project is not in an organization; create one with `sotto org create` and \
             `sotto init --org <id>`"
                .into(),
        )
    })?;
    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    let keypair = session::account_keypair(store, &master)?;
    let client = sync_client(keychain)?;
    let env_id = remote::team::share_env(&client, store, &keypair, org_id, user_id, config)?;
    eprintln!(
        "shared {}/{} with {user_id}; they can clone it with:",
        config.project, config.environment
    );
    println!("sotto clone {} {env_id} --org {org_id}", config.project_id);
    Ok(())
}

/// Clone a shared environment into the current directory: fetch our grant, reconstruct it, and pull.
#[allow(clippy::too_many_arguments)]
fn clone_env(
    store: &Store,
    keychain: &dyn Keychain,
    cwd: &Path,
    project_id: &str,
    env_id: &str,
    project_label: Option<&str>,
    env_label: Option<&str>,
    org_id: Option<&str>,
) -> Result<()> {
    if cwd.join(config::CONFIG_FILE).exists() {
        return Err(Error::Input(format!(
            "{} already exists in this directory",
            config::CONFIG_FILE
        )));
    }
    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    let keypair = session::account_keypair(store, &master)?;
    let client = sync_client(keychain)?;
    let config = remote::team::clone_env(
        &client,
        store,
        &keypair,
        project_id,
        env_id,
        project_label,
        env_label,
        org_id,
    )?;
    config.save_to(cwd)?;
    eprintln!("cloned into `{}` ({})", config.project, config.environment);
    Ok(())
}

/// Set up this device from the server: reconstruct the identity from the Emergency Kit + downloaded
/// account, recreate the project/environments, and pull the active environment's secrets.
fn setup(store: &Store, keychain: &dyn Keychain, cwd: &Path) -> Result<()> {
    if store.get_identity()?.is_some() {
        return Err(Error::AlreadyInitialized);
    }
    let (config, _dir) = Config::discover(cwd)?;
    let client = sync_client(keychain)?;
    let bundle = remote::SyncApi::get_account(&client)?.ok_or_else(|| {
        Error::Input(
            "no account on the server; run `sotto init` then `sotto push` on your first device"
                .into(),
        )
    })?;

    let mut secret_key = read_secret_key()?;
    let mut password = read_password("Master password: ")?;
    let result = remote::sync::restore_account(
        store,
        keychain,
        &bundle,
        &secret_key,
        &password,
        SESSION_TTL,
    );
    secret_key.zeroize();
    password.zeroize();
    result?;

    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    remote::sync::pull_environments(&client, store, master.as_bytes(), &config)?;
    let revision = remote::sync::pull(&client, store, &config)?;
    eprintln!(
        "set up {} ({}) from the server — revision {revision}",
        config.project, config.environment
    );
    Ok(())
}

/// Reset the account with fresh keys (the lost-Emergency-Kit path): generate a new identity
/// locally, replace the server-side account material, and print the new kit. Everything encrypted
/// under the old keys — local personal data included — becomes permanently unreadable.
fn reset(store: &Store, keychain: &dyn Keychain, yes: bool) -> Result<()> {
    // Require a working login first, so we don't destroy local state and then fail to upload.
    let client = sync_client(keychain)?;

    if !yes {
        eprintln!("This PERMANENTLY discards your account keys. Secrets encrypted under them");
        eprintln!("(including local personal projects) become unreadable, and org admins must");
        eprintln!("re-grant your shared environments.");
        eprint!("Type `reset` to continue: ");
        io::stderr().flush().ok();
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::Io(e.to_string()))?;
        if line.trim() != "reset" {
            return Err(Error::Input("reset aborted".into()));
        }
    }

    let mut password = read_new_password()?;
    let kit = session::reinit(store, keychain, &password, SESSION_TTL);
    password.zeroize();
    let kit = kit?;

    // Upload the fresh material; the server also drops our now-dead env grants.
    let material = sotto_cli::account::material(store)?;
    remote::SyncApi::reset_account(
        &client,
        &remote::api::AccountBundle {
            public_key: remote::api::b64encode(&material.public_key),
            enc_private_keys: remote::api::b64encode(&material.enc_private_keys),
            kdf_params: remote::api::b64encode(&material.kdf_params),
            recovery_blob: remote::api::b64encode(&material.recovery_blob),
        },
    )?;

    eprintln!();
    eprintln!("  Account reset. Save your NEW Emergency Kit — the old one is void:");
    eprintln!("    Secret Key:   {}", kit.secret_key);
    eprintln!("    Recovery Key: {}", kit.recovery_key);
    eprintln!();
    eprintln!("  Ask your org admins to re-share environments with you (`sotto grant`).");
    Ok(())
}

/// Read the secret key from `SOTTO_SECRET_KEY` or a hidden prompt, returning its decoded bytes.
fn read_secret_key() -> Result<Vec<u8>> {
    let input = if let Ok(value) = std::env::var("SOTTO_SECRET_KEY") {
        value
    } else {
        eprint!("Secret Key (SK1-…): ");
        io::stderr().flush().ok();
        rpassword::read_password().map_err(|e| Error::Io(e.to_string()))?
    };
    sotto_core::format::decode_key("SK", 1, input.trim())
        .map_err(|_| Error::Input("invalid Secret Key".into()))
}

/// Build an authenticated sync client from the configured server URL + stored session token.
fn sync_client(keychain: &dyn Keychain) -> Result<remote::HttpClient> {
    let config_path = sotto_cli::paths::config_path()?;
    let server = remote::config::server_url(None, &config_path)?;
    let token = remote::auth::current_session(keychain)?
        .ok_or_else(|| Error::Input("not logged in; run `sotto login`".into()))?;
    Ok(remote::HttpClient::new(server, token))
}

/// Log in to the sync server via the loopback OAuth flow, then persist the session + server URL.
fn login(
    keychain: &dyn Keychain,
    server_override: Option<&str>,
    web_override: Option<&str>,
) -> Result<()> {
    let config_path = sotto_cli::paths::config_path()?;
    let server = remote::config::server_url(server_override, &config_path)?;
    let token = remote::auth::authorize(&server)?;

    // Verify the session works before persisting anything.
    let client = remote::HttpClient::new(server.clone(), token.clone());
    let me = remote::SyncApi::me(&client)?;
    remote::auth::store_session(keychain, &token)?;

    // Preserve a previously configured web URL unless this login overrides it.
    let existing_web =
        remote::config::GlobalConfig::load_from(&config_path)?.and_then(|c| c.web_url);
    let web_url = web_override
        .map(|w| w.trim_end_matches('/').to_string())
        .or(existing_web);
    remote::config::GlobalConfig {
        server_url: server,
        web_url,
    }
    .save_to(&config_path)?;
    eprintln!("logged in (user {})", me.user_id);
    Ok(())
}

/// Seal a secret, upload it as a share link, and print the link (the fragment key never leaves).
fn share(
    app: &App,
    keychain: &dyn Keychain,
    config: &Config,
    name: &str,
    views: i32,
    expire: Option<i64>,
    passphrase: bool,
) -> Result<()> {
    let mut value = app.get(config, name)?;
    let client = sync_client(keychain)?;
    let web_base = remote::config::web_base(&sotto_cli::paths::config_path()?)?;

    let passphrase = if passphrase {
        Some(read_share_passphrase()?)
    } else {
        None
    };
    let opts = remote::share::ShareOptions {
        max_views: views,
        ttl_seconds: expire,
        passphrase,
    };
    let result = remote::share::create(&client, &web_base, &value, &opts);
    value.zeroize();
    if let Some(mut passphrase) = opts.passphrase {
        passphrase.zeroize();
    }
    let link = result?;

    eprintln!(
        "share link ({}/{}) — burns after {views} view(s):",
        config.project, config.environment
    );
    println!("{link}");
    Ok(())
}

/// Read a share passphrase from a hidden prompt (never `SOTTO_PASSWORD`, which is the master).
fn read_share_passphrase() -> Result<Vec<u8>> {
    eprint!("Passphrase for the link: ");
    io::stderr().flush().ok();
    rpassword::read_password()
        .map(String::into_bytes)
        .map_err(|e| Error::Io(e.to_string()))
}

fn status(app: &App, cwd: &Path, json: bool) -> Result<()> {
    // Only an actually-absent config is "no project"; a present-but-invalid or unreadable config
    // is a real error and must not be reported as "none".
    let config = match Config::discover(cwd) {
        Ok((c, _dir)) => Some(c),
        Err(Error::NoConfig(_)) => None,
        Err(e) => return Err(e),
    };
    let status = app.status(config.as_ref())?;
    if json {
        let project = status.project.as_ref().map(
            |(name, environment)| serde_json::json!({ "name": name, "environment": environment }),
        );
        let value = serde_json::json!({
            "initialized": status.initialized,
            "unlocked": status.unlocked,
            "project": project,
        });
        println!("{}", to_json(&value)?);
        return Ok(());
    }
    println!(
        "identity: {}",
        if status.initialized {
            "set up"
        } else {
            "not set up"
        }
    );
    println!(
        "session:  {}",
        if status.unlocked {
            "unlocked"
        } else {
            "locked"
        }
    );
    match status.project {
        Some((project, env)) => println!("project:  {project} ({env})"),
        None => println!("project:  none (no {} here)", config::CONFIG_FILE),
    }
    Ok(())
}

/// Show a secret's server-side version history, newest first.
fn history(
    store: &Store,
    keychain: &dyn Keychain,
    config: &Config,
    name: &str,
    reveal: bool,
) -> Result<()> {
    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    let keypair = session::account_keypair(store, &master)?;
    let client = sync_client(keychain)?;
    let versions = remote::sync::history(&client, store, &keypair, config, name)?;
    if reveal {
        eprintln!("warning: --reveal prints secret values in plaintext");
    }
    let count = versions.len();
    for mut v in versions {
        match (&v.value, reveal) {
            (Some(value), true) => {
                println!("v{}  {}", v.version, String::from_utf8_lossy(value))
            }
            (Some(value), false) => println!("v{}  ({} bytes)", v.version, value.len()),
            (None, _) => println!("v{}  (unreadable — run `sotto pull` first)", v.version),
        }
        // Zeroize each decrypted plaintext as soon as it's printed, so the whole history isn't left
        // resident in memory for the rest of the command.
        if let Some(value) = v.value.as_mut() {
            value.zeroize();
        }
    }
    eprintln!(
        "{count} version(s) of {name} ({}/{})",
        config.project, config.environment
    );
    Ok(())
}

/// Restore an old version of a secret as a new version (local until pushed).
fn rollback(
    store: &Store,
    keychain: &dyn Keychain,
    config: &Config,
    name: &str,
    version: i64,
) -> Result<()> {
    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    let keypair = session::account_keypair(store, &master)?;
    let client = sync_client(keychain)?;
    let len = remote::sync::rollback(&client, store, &keypair, config, name, version)?;
    eprintln!(
        "restored {name} to version {version} ({len} bytes) as a new version; run `sotto push` to sync"
    );
    Ok(())
}

/// Render an environment diff: one line per key with a presence/difference marker; values only
/// with `--reveal`.
fn env_diff(app: &App, config: &Config, left: &str, right: &str, reveal: bool) -> Result<()> {
    use sotto_cli::commands::DiffStatus;

    let diff = app.env_diff(config, left, right)?;
    if diff.is_empty() {
        eprintln!("both environments are empty");
        return Ok(());
    }
    if reveal {
        eprintln!("warning: --reveal prints secret values in plaintext");
    }
    let mut differing = 0usize;
    for entry in &diff {
        let marker = match entry.status {
            DiffStatus::Equal => "=",
            DiffStatus::Differs => "!",
            DiffStatus::OnlyLeft => "<",
            DiffStatus::OnlyRight => ">",
        };
        let detail = match entry.status {
            DiffStatus::Equal => String::new(),
            DiffStatus::Differs if reveal => format!(
                "  {} -> {}",
                display_secret(entry.left.as_deref().unwrap_or_default()),
                display_secret(entry.right.as_deref().unwrap_or_default()),
            ),
            DiffStatus::Differs => "  (differs)".into(),
            DiffStatus::OnlyLeft => format!("  (only in {left})"),
            DiffStatus::OnlyRight => format!("  (only in {right})"),
        };
        if entry.status != DiffStatus::Equal {
            differing += 1;
        }
        println!("{marker} {}{detail}", entry.name);
    }
    eprintln!(
        "{} key(s), {} difference(s) between {left} and {right}",
        diff.len(),
        differing
    );
    Ok(())
}

/// Render (and with --confirm, apply) an environment copy plan.
fn env_copy(app: &App, config: &Config, src: &str, dst: &str, confirm: bool) -> Result<()> {
    let plan = app.env_copy(config, src, dst, confirm)?;
    for name in &plan.create {
        println!("create {name}");
    }
    for name in &plan.update {
        println!("update {name}");
    }
    let summary = format!(
        "{} to create, {} to update, {} unchanged",
        plan.create.len(),
        plan.update.len(),
        plan.unchanged.len()
    );
    if confirm {
        eprintln!("copied {src} -> {dst}: {summary}");
        eprintln!("run `sotto push --env {dst}` to sync the changes");
    } else {
        eprintln!("dry-run {src} -> {dst}: {summary}");
        eprintln!("nothing written; re-run with --confirm to apply");
    }
    Ok(())
}

fn env_use(store: &Store, cwd: &Path, name: &str) -> Result<()> {
    let (mut config, dir) = Config::discover(cwd)?;
    if !store
        .list_environments(&config.project_id)?
        .iter()
        .any(|e| e == name)
    {
        return Err(Error::NotFound(format!("environment `{name}`")));
    }
    config.environment = name.to_string();
    config.save_to(&dir)?;
    eprintln!("active environment: {name}");
    Ok(())
}

fn import_dotenv(app: &App, config: &Config, file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| Error::Io(format!("reading {}: {e}", file.display())))?;
    let pairs = dotenv::parse(&text)?;
    let count = pairs.len();
    for (name, value) in pairs {
        app.set(config, &name, value.as_bytes())?;
    }
    eprintln!(
        "imported {count} secret(s) into {} ({})",
        config.project, config.environment
    );
    Ok(())
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| Error::Io(e.to_string()))
}

fn ensure_unlocked(store: &Store, keychain: &dyn Keychain) -> Result<()> {
    if session::current_master_key(keychain)?.is_some() {
        return Ok(());
    }
    if store.get_identity()?.is_none() {
        return Err(Error::NoIdentity);
    }
    let mut password = read_password("Master password: ")?;
    let result = session::unlock(store, keychain, &password, SESSION_TTL);
    password.zeroize();
    result
}

fn effective_config(cwd: &Path, env_override: Option<&str>) -> Result<Config> {
    let (mut config, _dir) = Config::discover(cwd)?;
    if let Some(env) = env_override {
        config.environment = env.to_string();
    }
    Ok(config)
}

/// Read a password: from `SOTTO_PASSWORD` if set, otherwise a hidden prompt on the terminal.
fn read_password(prompt: &str) -> Result<Vec<u8>> {
    if let Ok(password) = std::env::var("SOTTO_PASSWORD") {
        return Ok(password.into_bytes());
    }
    eprint!("{prompt}");
    io::stderr().flush().ok();
    rpassword::read_password()
        .map(String::into_bytes)
        .map_err(|e| Error::Io(e.to_string()))
}

/// Read a new password with confirmation (or `SOTTO_PASSWORD` for non-interactive setup).
fn read_new_password() -> Result<Vec<u8>> {
    if let Ok(password) = std::env::var("SOTTO_PASSWORD") {
        return Ok(password.into_bytes());
    }
    eprint!("Choose a master password: ");
    io::stderr().flush().ok();
    let first = Zeroizing::new(rpassword::read_password().map_err(|e| Error::Io(e.to_string()))?);
    eprint!("Confirm master password: ");
    io::stderr().flush().ok();
    let second = Zeroizing::new(rpassword::read_password().map_err(|e| Error::Io(e.to_string()))?);
    if first.as_str() != second.as_str() {
        return Err(Error::Input("passwords do not match".to_string()));
    }
    Ok(first.as_bytes().to_vec())
}

/// Read a secret value: inline `--value` (with a warning), `--stdin`, or a hidden prompt.
fn read_value(value: Option<String>, stdin: bool) -> Result<Vec<u8>> {
    if let Some(value) = value {
        eprintln!("warning: --value can leak into shell history; prefer the prompt or --stdin");
        return Ok(value.into_bytes());
    }
    if stdin {
        let mut buf = Vec::new();
        io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| Error::Io(e.to_string()))?;
        return Ok(buf);
    }
    eprint!("Value: ");
    io::stderr().flush().ok();
    rpassword::read_password()
        .map(String::into_bytes)
        .map_err(|e| Error::Io(e.to_string()))
}

/// Write a secret value to stdout. Refuses a terminal unless `reveal` is set; appends a newline
/// only for human (terminal) output so piped output stays byte-exact.
fn write_value(value: &[u8], reveal: bool) -> Result<()> {
    let is_tty = io::stdout().is_terminal();
    if is_tty && !reveal {
        return Err(Error::Input(
            "refusing to print a secret to a terminal; use --reveal or pipe the output".to_string(),
        ));
    }
    let mut out = io::stdout().lock();
    out.write_all(value).map_err(|e| Error::Io(e.to_string()))?;
    if is_tty {
        out.write_all(b"\n").ok();
    }
    out.flush().map_err(|e| Error::Io(e.to_string()))
}

/// Render a secret value for the human `env diff --reveal` view. Secrets are arbitrary bytes (see
/// [`write_value`]), so a raw `from_utf8_lossy` would both collapse distinct non-UTF-8 byte
/// sequences to the same replacement glyph and let embedded control/ANSI sequences move the cursor
/// or forge terminal output. Escape a valid-UTF-8 value (control characters become visible escapes,
/// keeping it on one line) and show non-UTF-8 as unambiguous base64.
fn display_secret(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.escape_debug().to_string(),
        Err(_) => format!("base64:{}", STANDARD.encode(bytes)),
    }
}

/// Decrypt all secrets as UTF-8 text pairs (for injection/export). Errors on non-UTF-8 values.
fn text_entries(app: &App, config: &Config) -> Result<Vec<(String, String)>> {
    let mut entries = Vec::new();
    for (name, value) in app.entries(config)? {
        let value = String::from_utf8(value).map_err(|_| {
            Error::Input(format!(
                "secret `{name}` is not valid UTF-8; cannot inject or export it as text"
            ))
        })?;
        entries.push((name, value));
    }
    Ok(entries)
}

/// A secret name is usable as an environment variable only if it's a POSIX identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`). Anything looser (spaces, newlines, shell metacharacters like
/// `;` or `$`) is rejected — otherwise it could change the meaning of `export --format shell`
/// output that a user `eval`s or `source`s.
fn validate_env_key(key: &str) -> Result<()> {
    let mut chars = key.chars();
    let valid = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(Error::Input(format!(
            "secret name {key:?} is not a usable environment variable name \
             (expected [A-Za-z_][A-Za-z0-9_]*)"
        )));
    }
    Ok(())
}

/// Run `args[0]` with the environment's secrets overlaid onto the inherited environment.
fn run_injected(app: &App, config: &Config, args: Vec<String>) -> Result<()> {
    run_with_entries(text_entries(app, config)?, args)
}

/// Run `args[0]` with `entries` overlaid onto the inherited environment (both session and machine
/// mode funnel through here).
fn run_with_entries(entries: Vec<(String, String)>, args: Vec<String>) -> Result<()> {
    let (program, rest) = args.split_first().ok_or_else(|| {
        Error::Input("no command given; usage: sotto run -- <cmd> [args…]".to_string())
    })?;

    let mut command = std::process::Command::new(program);
    command.args(rest);
    for (name, value) in &entries {
        validate_env_key(name)?;
        if std::env::var_os(name).is_some() {
            eprintln!("warning: overriding inherited environment variable {name}");
        }
        command.env(name, value);
    }
    exec_or_replace(command, program)
}

/// On Unix, replace this process with the command so signals and the exit code pass through
/// transparently. Elsewhere, spawn it, wait, and propagate its exit code.
#[cfg(unix)]
fn exec_or_replace(mut command: std::process::Command, program: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // `exec` only returns if launching the program fails.
    Err(Error::Io(format!(
        "failed to run `{program}`: {}",
        command.exec()
    )))
}

#[cfg(not(unix))]
fn exec_or_replace(mut command: std::process::Command, program: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|e| Error::Io(format!("failed to run `{program}`: {e}")))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Render the environment's secrets in `format`, refusing a terminal unless `reveal` is set.
fn export_secrets(app: &App, config: &Config, format: ExportFormat, reveal: bool) -> Result<()> {
    export_with_entries(text_entries(app, config)?, format, reveal)
}

/// Render `entries` in `format` to stdout (both session and machine mode funnel through here).
fn export_with_entries(
    entries: Vec<(String, String)>,
    format: ExportFormat,
    reveal: bool,
) -> Result<()> {
    if io::stdout().is_terminal() && !reveal {
        return Err(Error::Input(
            "refusing to write secrets to a terminal; redirect to a file or use --reveal"
                .to_string(),
        ));
    }
    // Validate names before rendering: an unsafe name (spaces, newlines, shell metacharacters)
    // would otherwise be emitted verbatim and could alter `--format shell` output that's sourced.
    for (name, _) in &entries {
        validate_env_key(name)?;
    }
    let rendered = export::render(format, &entries);
    eprintln!("warning: export writes secrets in plaintext");
    let mut out = io::stdout().lock();
    out.write_all(rendered.as_bytes())
        .map_err(|e| Error::Io(e.to_string()))?;
    out.flush().map_err(|e| Error::Io(e.to_string()))
}

// --- machine (SOTTO_TOKEN) mode ---

/// Resolve the server URL for machine mode: `SOTTO_SERVER` (the CI-friendly path), else the global
/// config written by `sotto login`.
fn machine_server_url() -> Result<String> {
    if let Ok(server) = std::env::var("SOTTO_SERVER") {
        return Ok(server.trim_end_matches('/').to_string());
    }
    let config_path = sotto_cli::paths::config_path()?;
    remote::config::server_url(None, &config_path)
        .map_err(|_| Error::Input("set SOTTO_SERVER to your sync server URL".into()))
}

/// Fetch + decrypt the token's environment in memory, as UTF-8 text pairs.
fn machine_entries(token: &str) -> Result<Vec<(String, String)>> {
    let token = remote::machine::parse_token(token)?;
    let server = machine_server_url()?;
    let mut entries = Vec::new();
    for (name, value) in remote::machine::fetch_entries(&server, &token)? {
        let value = String::from_utf8(value).map_err(|_| {
            Error::Input(format!(
                "secret `{name}` is not valid UTF-8; cannot inject or export it as text"
            ))
        })?;
        entries.push((name, value));
    }
    Ok(entries)
}

/// `sotto run` in machine mode: decrypt via SOTTO_TOKEN and inject.
fn machine_run(token: &str, args: Vec<String>) -> Result<()> {
    run_with_entries(machine_entries(token)?, args)
}

/// `sotto export` in machine mode.
fn machine_export(token: &str, format: ExportFormat, reveal: bool) -> Result<()> {
    export_with_entries(machine_entries(token)?, format, reveal)
}

#[cfg(test)]
mod tests {
    use super::display_secret;

    #[test]
    fn display_secret_keeps_plain_text() {
        assert_eq!(display_secret(b"postgres://prod"), "postgres://prod");
    }

    #[test]
    fn display_secret_escapes_control_and_ansi_sequences() {
        // A newline can't split the diff line, and an ESC can't start a real ANSI sequence.
        assert_eq!(display_secret(b"a\nb"), "a\\nb");
        let rendered = display_secret(b"\x1b[31mred\x1b[0m");
        assert!(
            !rendered.contains('\x1b'),
            "escape byte must not survive: {rendered}"
        );
        assert!(rendered.contains("\\u{1b}"));
    }

    #[test]
    fn display_secret_base64_encodes_non_utf8() {
        // Two distinct non-UTF-8 byte strings must render differently (no lossy collapsing).
        let a = display_secret(&[0xff, 0x00]);
        let b = display_secret(&[0xfe, 0x00]);
        assert!(a.starts_with("base64:"));
        assert_ne!(a, b);
    }
}
