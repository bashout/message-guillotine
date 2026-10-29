//! The background service: `run` is the daemon the native autostart entries
//! launch; `install`/`uninstall`/`status` manage the per-OS registration
//! (systemd --user on Linux, launchd LaunchAgent on macOS, registry Run key
//! on Windows).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Config};
use crate::scheduler;

fn exe_path() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

fn config_path(config_arg: Option<&Path>) -> PathBuf {
    config_arg
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(config::ENV_CONFIG).map(PathBuf::from))
        .unwrap_or_else(config::default_config_path)
}

/// Run the daemon in the foreground, logging to the per-OS data dir.
pub async fn run(config_arg: Option<&Path>) -> anyhow::Result<()> {
    let path = config_path(config_arg);
    let cfg = Config::load(&path)?;

    // Layer a rolling file appender under the stderr one so a service has
    // persistent logs without changing CLI behavior.
    config::ensure_data_dir()?; // 0700 before anything is written into it
    let log_dir = config::data_dir().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "guillotine.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        use tracing_subscriber::EnvFilter;
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("guillotine=info"));
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false);
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .try_init();
    }
    // The guard stays alive for the whole run; on exit it flushes the logs.
    let _guard = _guard;

    // Age out old log files so months of uptime don't grow unbounded.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
            if let Ok(rd) = std::fs::read_dir(&log_dir) {
                for entry in rd.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        if meta.modified().is_ok_and(|m| m < cutoff) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
    });

    tracing::info!(config = %path.display(), "guillotine service starting");
    // Graceful shutdown: flush logs and exit cleanly on SIGINT/SIGTERM.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received; flushing logs");
        let _ = shutdown_tx.send(());
    });
    let result = tokio::select! {
        r = scheduler::run_loop(&cfg, &path) => r,
        _ = shutdown_rx => Ok(()),
    };
    drop(_guard); // flush the non-blocking log writer
    result
}

/// systemd --user unit (Linux).
#[cfg(target_os = "linux")]
fn systemd_quote(path: &Path) -> String {
    // systemd quoting: wrap in double quotes, escape \, ", $, `, and %
    // (specifier expansion), and reject newlines outright.
    let raw = path.display().to_string();
    assert!(!raw.contains('\n'), "path contains a newline");
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(target_os = "linux")]
fn systemd_unit(exe: &Path, config: &Path) -> String {
    format!(
        "[Unit]\nDescription=guillotine — scheduled message deletion/archival\n\n\
         [Service]\nType=simple\nExecStart={} --config {} service run\nRestart=on-failure\nRestartSec=30\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quote(exe),
        systemd_quote(config)
    )
}

#[cfg(target_os = "linux")]
pub fn install(config_arg: Option<&Path>) -> anyhow::Result<()> {
    let exe = exe_path()?;
    let config = config_path(config_arg);
    let unit_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit = unit_dir.join("guillotine.service");
    std::fs::write(&unit, systemd_unit(&exe, &config))?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "enable", "--now", "guillotine.service"])
        .status();
    println!("Installed systemd user unit: {}", unit.display());
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall(config_arg: Option<&Path>) -> anyhow::Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "guillotine.service"])
        .status();
    let unit = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("systemd/user/guillotine.service");
    let _ = std::fs::remove_file(&unit);
    let _ = config_arg;
    println!("Removed systemd user unit: {}", unit.display());
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn status(_config_arg: Option<&Path>) -> anyhow::Result<()> {
    let s = Command::new("systemctl")
        .args(["--user", "is-active", "guillotine.service"])
        .status()?;
    println!(
        "guillotine.service: {}",
        if s.success() { "active" } else { "inactive" }
    );
    Ok(())
}

/// launchd LaunchAgent (macOS).
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn launchd_plist(exe: &Path, config: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let config = xml_escape(&config.display().to_string());
    let out = xml_escape(&config::data_dir().join("logs/stdout.log").display().to_string());
    let err = xml_escape(&config::data_dir().join("logs/stderr.log").display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>com.guillotine.service</string>\n\
         <key>ProgramArguments</key><array><string>{exe}</string><arg>--config</arg><string>{config}</string><arg>service</arg><arg>run</arg></array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>KeepAlive</key><true/>\n\
         <key>StandardOutPath</key><string>{out}</string>\n\
         <key>StandardErrorPath</key><string>{err}</string>\n\
         </dict></plist>\n"
    )
}

#[cfg(target_os = "macos")]
pub fn install(config_arg: Option<&Path>) -> anyhow::Result<()> {
    let exe = exe_path()?;
    let config = config_path(config_arg);
    std::fs::create_dir_all(config::data_dir().join("logs"))?;
    let agents = PathBuf::from(std::env::var("HOME")?).join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents)?;
    let plist = agents.join("com.guillotine.service.plist");
    std::fs::write(&plist, launchd_plist(&exe, &config))?;
    let _ = Command::new("launchctl")
        .args(["load", plist.to_str().unwrap()])
        .status();
    println!("Installed launchd agent: {}", plist.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall(_config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
    let plist = PathBuf::from(std::env::var("HOME")?)
        .join("Library/LaunchAgents/com.guillotine.service.plist");
    let _ = Command::new("launchctl")
        .args(["unload", plist.to_str().unwrap_or("")])
        .status();
    let _ = std::fs::remove_file(&plist);
    println!("Removed launchd agent: {}", plist.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn status(_config_arg: Option<&Path>) -> anyhow::Result<()> {
    let s = Command::new("launchctl")
        .args(["list", "com.guillotine.service"])
        .status()?;
    println!(
        "com.guillotine.service: {}",
        if s.success() { "loaded" } else { "not loaded" }
    );
    Ok(())
}

/// Registry Run key (Windows): the native per-user autostart.
#[cfg(target_os = "windows")]
pub fn install(config_arg: Option<&Path>) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let exe = exe_path()?;
    let config = config_path(config_arg);
    let value = format!(
        "\"{}\" --config \"{}\" service run",
        exe.display(),
        config.display()
    );
    let status = Command::new("reg")
        .args([
            "add",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            "/v",
            "guillotine",
            "/t",
            "REG_SZ",
            "/d",
            &value,
            "/f",
        ])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .status()?;
    anyhow::ensure!(status.success(), "reg add failed");
    println!("Installed registry Run key for guillotine");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn uninstall(_config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let status = Command::new("reg")
        .args([
            "delete",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            "/v",
            "guillotine",
            "/f",
        ])
        .creation_flags(0x08000000)
        .status()?;
    let _ = status;
    println!("Removed registry Run key for guillotine");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn status(_config_arg: Option<&Path>) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let output = Command::new("reg")
        .args([
            "query",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            "/v",
            "guillotine",
        ])
        .creation_flags(0x08000000)
        .output()?;
    println!(
        "guillotine Run key: {}",
        if output.status.success() {
            "installed"
        } else {
            "not installed"
        }
    );
    Ok(())
}
