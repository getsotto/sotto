//! The `sotto` CLI — local, end-to-end-encrypted secret management.
//!
//! This binary is the IO layer: it parses arguments, resolves paths and config, prompts for the
//! master password (hidden, or `SOTTO_PASSWORD`), enforces TTY-safe output, and renders results.
//! All logic lives in the `sotto_cli` library.

use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::time::Duration;

use clap::{Parser, Subcommand};
use zeroize::Zeroize;

use sotto_cli::commands::App;
use sotto_cli::config::{self, Config};
use sotto_cli::error::{Error, Result};
use sotto_cli::keychain::{Keychain, OsKeychain};
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
    },
    /// Unlock the store for this session.
    Unlock,
    /// Lock the store (clear the cached session).
    Lock,
    /// Show identity, session, and project status.
    Status,
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
    Ls,
    /// Remove a secret.
    Rm { name: String },
    /// Manage environments.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
}

#[derive(Subcommand)]
enum EnvCommand {
    /// List the project's environments (the active one is marked).
    Ls,
    /// Set the active environment for this project.
    Use { name: String },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(e.exit_code());
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

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
        Command::Init { name } => init(&store, &keychain, &cwd, name),
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
        Command::Status => status(&app, &cwd),
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
        Command::Ls => {
            let config = effective_config(&cwd, cli.env.as_deref())?;
            ensure_unlocked(&store, &keychain)?;
            for name in app.list(&config)? {
                println!("{name}");
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
        },
    }
}

fn init(store: &Store, keychain: &dyn Keychain, cwd: &Path, name: Option<String>) -> Result<()> {
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
        eprintln!("  Save your Secret Key — it cannot be recovered:");
        eprintln!("    {}", kit.secret_key);
        eprintln!();
    } else {
        ensure_unlocked(store, keychain)?;
    }

    let master = session::current_master_key(keychain)?.ok_or(Error::Locked)?;
    let project_name = name.unwrap_or_else(|| {
        cwd.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string())
    });
    let project = Vault::create_project(store, master.as_bytes(), &project_name)?;
    let config = Config {
        project_id: project.id,
        project: project_name,
        environment: "dev".to_string(),
    };
    config.save_to(cwd)?;
    eprintln!("initialized `{}` ({})", config.project, config.environment);
    Ok(())
}

fn status(app: &App, cwd: &Path) -> Result<()> {
    // Only an actually-absent config is "no project"; a present-but-invalid or unreadable config
    // is a real error and must not be reported as "none".
    let config = match Config::discover(cwd) {
        Ok((c, _dir)) => Some(c),
        Err(Error::NoConfig(_)) => None,
        Err(e) => return Err(e),
    };
    let status = app.status(config.as_ref())?;
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
    let first = rpassword::read_password().map_err(|e| Error::Io(e.to_string()))?;
    eprint!("Confirm master password: ");
    io::stderr().flush().ok();
    let second = rpassword::read_password().map_err(|e| Error::Io(e.to_string()))?;
    if first != second {
        return Err(Error::Input("passwords do not match".to_string()));
    }
    Ok(first.into_bytes())
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
