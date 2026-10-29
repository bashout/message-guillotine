//! The background service: `run` is the daemon the native autostart entries
//! launch; `install`/`uninstall`/`status` manage the per-OS registration
//! (systemd --user on Linux, launchd LaunchAgent on macOS, registry Run key
//! on Windows).

use std::path::PathBuf;
use std::process::Command;

use crate::config::{self, Config};
use crate::scheduler;

fn exe_path() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

fn config_path(config_arg: Option<&PathBuf>) -> PathBuf {
    config_arg
        .cloned()
        .or_else(|| std::env::var_os(config::ENV_CONFIG).map(PathBuf::from))
        .unwrap_or_else(config::default_config_path)
}

/// Run the daemon in the foreground, logging to the per-OS data dir.
pub async fn run(config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
    let path = config_path(config_arg);
    let cfg = Config::load(&path)?;

    // Layer a rolling file appender under the stderr one so a service has
    // persistent logs without changing CLI behavior.
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
    // Keep the guard alive for the lifetime of the process.
    std::mem::forget(_guard);

    tracing::info!(config = %path.display(), "guillotine service starting");
    scheduler::run_loop(&cfg).await
}

/// systemd --user unit (Linux).
#[cfg(target_os = "linux")]
fn systemd_unit(exe: &PathBuf, config: &PathBuf) -> String {
    format!(
        "[Unit]\nDescription=guillotine — scheduled message deletion/archival\n\n\
         [Service]\nType=simple\nExecStart={} --config {} service run\nRestart=on-failure\nRestartSec=30\n\n\
         [Install]\nWantedBy=default.target\n",
        exe.display(),
        config.display()
    )
}

#[cfg(target_os = "linux")]
pub fn install(config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
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
pub fn uninstall(config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
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
pub fn status(_config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
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
fn launchd_plist(exe: &PathBuf, config: &PathBuf) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>com.guillotine.service</string>\n\
         <key>ProgramArguments</key><array><string>{}</string><arg>--config</arg><string>{}</string><arg>service</arg><arg>run</arg></array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>KeepAlive</key><true/>\n\
         <key>StandardOutPath</key><string>{}</string>\n\
         <key>StandardErrorPath</key><string>{}</string>\n\
         </dict></plist>\n",
        exe.display(),
        config.display(),
        config::data_dir().join("logs/stdout.log").display(),
        config::data_dir().join("logs/stderr.log").display()
    )
}

#[cfg(target_os = "macos")]
pub fn install(config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
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
pub fn status(_config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
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
pub fn install(config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
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
pub fn status(_config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
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
