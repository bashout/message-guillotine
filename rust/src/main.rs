//! guillotine — scheduled bulk delete and archive of your own chat messages.
//!
//! Runs as a service (native autostart per OS) driven by a TOML config, with
//! a tray app for status/configuration and one-shot CLI subcommands.
//!
//! WARNING: self-bots violate the platform ToS. Use at your own risk.

mod api;
mod archive_format;
mod archiver;
mod config;
mod deletion;
#[cfg(feature = "gui")]
mod gui;
mod ipc;
mod progress;
mod scheduler;
mod service;
mod viewer;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

use config::Config;

#[derive(Parser)]
#[command(
    name = "guillotine",
    version,
    about = "Scheduled bulk delete and archive of your own chat messages \
             (self-bot — violates the platform ToS, use at your own risk)"
)]
struct Cli {
    /// Config file (default: per-OS config dir/config.toml, or GUILLOTINE_CONFIG)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run or manage the background service
    Service {
        #[command(subcommand)]
        action: ServiceCommands,
    },
    /// Tray icon app: status, run-now, and configuration
    Gui,
    /// Browse archived chats in a local web UI (V2 viewer)
    Viewer {
        /// Archival folder, or a single archive folder (default: the
        /// configured archive dir)
        archives_dir: Option<PathBuf>,
        /// Interface to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind (falls back to an ephemeral port when taken)
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Print the URL instead of opening the browser
        #[arg(long)]
        no_browser: bool,
    },
    /// One-shot scan of a channel: report what a deletion run would delete
    Scan {
        /// Channel id (DM channel or guild channel)
        channel: String,
        /// Only count/delete messages older than this many days
        #[arg(long)]
        retention_days: Option<u32>,
    },
    /// One-shot archive of a chat (appends to an existing archive if present)
    Archive {
        /// Channel id (DM channel or guild channel)
        channel: String,
    },
    /// Run one configured chat's job immediately (serialized with any
    /// scheduled run — only one deletion runs at a time)
    RunNow {
        /// Chat key: dm:<user_id> or guild:<guild_id>/channel:<channel_id>
        chat: String,
    },
    /// Configuration utilities
    Config {
        #[command(subcommand)]
        action: ConfigCommands,
    },
}

#[derive(Subcommand)]
enum ServiceCommands {
    /// Run the daemon in the foreground (the service entries call this)
    Run,
    /// Register native autostart (systemd --user / launchd / registry+SCM)
    Install,
    /// Remove the native autostart registration
    Uninstall,
    /// Show the registered service status
    Status,
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Print the config file path in use
    Path,
    /// Write an example config to the config path (refuses to overwrite)
    Init,
    /// Validate the config and print the resolved chats
    Validate,
}

fn load_config(config_arg: Option<&PathBuf>) -> anyhow::Result<(Config, PathBuf)> {
    let path = config_arg
        .cloned()
        .or_else(|| std::env::var_os(config::ENV_CONFIG).map(PathBuf::from))
        .unwrap_or_else(config::default_config_path);
    let cfg = Config::load(&path)?;
    Ok((cfg, path))
}

/// Last 500 log lines of this process, shown in the gui Logs tab.
pub fn gui_log_buffer() -> &'static std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    static BUF: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<String>>>> =
        std::sync::OnceLock::new();
    BUF.get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("guillotine=info,tower_http=warn,reqwest=warn"));
    // Ring-buffer layer so the gui can show its own output in the Logs tab.
    struct RingBuf;
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RingBuf {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            use tracing::field::Visit;
            struct V(String);
            impl Visit for V {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!(" {}={value:?}", field.name()));
                }
            }
            let mut v = V(String::new());
            event.record(&mut v);
            let meta = event.metadata();
            let line = format!(
                "{} {:<5} {}{}",
                chrono::Local::now().format("%H:%M:%S"),
                meta.level(),
                meta.target(),
                v.0
            );
            let mut buf = gui_log_buffer().lock().unwrap_or_else(|e| e.into_inner());
            buf.push(line);
            if buf.len() > 500 {
                let excess = buf.len() - 500;
                buf.drain(..excess);
            }
        }
    }
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(RingBuf)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Cli {
        config: config_arg,
        command,
    } = Cli::parse();
    // The service path installs its own file-logging subscriber; everything
    // else logs to stderr.
    if !matches!(
        command,
        Commands::Service {
            action: ServiceCommands::Run
        }
    ) {
        init_tracing();
    }

    match command {
        Commands::Config { action } => match action {
            ConfigCommands::Path => {
                let (_, path) = load_config(config_arg.as_ref())
                    .context("config not loadable; path is still: ")?;
                println!("{}", path.display());
            }
            ConfigCommands::Init => {
                let path = config_arg
                    .clone()
                    .or_else(|| std::env::var_os(config::ENV_CONFIG).map(PathBuf::from))
                    .unwrap_or_else(config::default_config_path);
                if path.exists() {
                    anyhow::bail!("{} already exists; refusing to overwrite", path.display());
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, config::EXAMPLE_CONFIG)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                }
                println!("Wrote example config to {}", path.display());
            }
            ConfigCommands::Validate => {
                let (cfg, path) = load_config(config_arg.as_ref())?;
                println!("{} is valid", path.display());
                println!("archive dir: {}", cfg.resolve_archive_dir(None).display());
                println!(
                    "dm policy: {:?} ({} users listed)",
                    cfg.dm.policy,
                    cfg.dm.users.len()
                );
                for (key, chat) in cfg.chats() {
                    println!(
                        "  chat {key}: {} | retain {}d | archive-first {}",
                        chat.schedule, chat.retention_days, chat.archive_before_delete
                    );
                }
            }
        },
        Commands::Service { action } => match action {
            ServiceCommands::Run => service::run(config_arg.as_deref()).await?,
            ServiceCommands::Install => service::install(config_arg.as_deref())?,
            ServiceCommands::Uninstall => service::uninstall(config_arg.as_deref())?,
            ServiceCommands::Status => service::status(config_arg.as_deref())?,
        },
        #[cfg(feature = "gui")]
        Commands::Gui => gui::run(config_arg.as_ref()).await?,
        #[cfg(not(feature = "gui"))]
        Commands::Gui => anyhow::bail!(
            "built without the gui feature; use `guillotine service run` and the CLI subcommands"
        ),
        Commands::Scan {
            channel,
            retention_days,
        } => deletion::scan(config_arg.as_ref(), &channel, retention_days).await?,
        Commands::Archive { channel } => archiver::archive(config_arg.as_ref(), &channel).await?,
        Commands::RunNow { chat } => scheduler::run_now(config_arg.as_ref(), &chat).await?,
        Commands::Viewer {
            archives_dir,
            host,
            port,
            no_browser,
        } => viewer::run(archives_dir, &host, port, no_browser).await?,
    }
    Ok(())
}
