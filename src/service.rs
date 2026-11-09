//! The background service: `run` is the daemon; `install`/`uninstall`/
//! `status` manage the per-OS autostart registration (systemd --user on
//! Linux, launchd LaunchAgent on macOS, registry Run key on Windows).
//! Desktop (gui) builds register the default launch — daemon + tray together
//! — on every OS; headless builds register the daemon-only `service run`.
//! On Linux, GUI builds also install a .desktop entry into the user's
//! applications menu (`~/.local/share/applications`), so the tray is
//! launchable from the app grid without a terminal.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Config};
use crate::scheduler;

/// The executable path to write into autostart entries and the .desktop file.
/// Under an AppImage, `current_exe` points into the ephemeral `/tmp/.mount_*`
/// squashfs mount that vanishes when the AppImage exits; the AppImage runtime
/// exports `$APPIMAGE` with the real file path, which is the path that must be
/// registered. Compiled on every OS so the choice is unit-testable everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pick_service_exe(appimage_env: Option<&Path>, current_exe: &Path) -> PathBuf {
    if let Some(p) = appimage_env {
        if p.is_file() {
            return p.to_path_buf();
        }
    }
    current_exe.to_path_buf()
}

pub(crate) fn exe_path() -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let picked = pick_service_exe(
        std::env::var_os("APPIMAGE").as_deref().map(Path::new),
        &std::env::current_exe()?,
    );
    #[cfg(not(target_os = "linux"))]
    let picked = std::env::current_exe()?;
    Ok(picked.canonicalize()?)
}

fn config_path(config_arg: Option<&Path>) -> PathBuf {
    config_arg
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(config::ENV_CONFIG).map(PathBuf::from))
        .unwrap_or_else(config::default_config_path)
}

/// Start the daemon when none is running, so the default launch (no
/// subcommand) and `guillotine gui` bring up the service together with the
/// tray. The child is spawned fully detached — its own process group on
/// unix, `DETACHED_PROCESS` on Windows — so the daemon outlives the tray
/// process, exactly like an autostarted daemon would. Already-running
/// daemons (e.g. started at login) are detected via the IPC hello handshake
/// and left alone, so a second daemon never starts.
#[cfg(feature = "gui")]
pub fn ensure_daemon(config_arg: Option<&Path>) -> anyhow::Result<()> {
    use std::process::Stdio;
    if crate::ipc::daemon_alive() {
        return Ok(());
    }
    let exe =
        std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot resolve exe path: {e}"))?;
    let mut cmd = Command::new(&exe);
    cmd.arg("service")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(config) = config_arg {
        cmd.arg("--config").arg(config);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("cannot start the service daemon: {e}"))?;
    // Bounded wait for the IPC hello handshake, so the tray's first status
    // poll finds a live daemon. A timeout is not fatal: the daemon may still
    // be coming up, and the tray keeps polling IPC either way.
    for _ in 0..50 {
        if crate::ipc::daemon_alive() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

/// Resolves when the process should shut down: SIGINT (Ctrl-C) on every
/// platform, plus SIGTERM on unix so `systemctl stop` flushes cleanly.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!("could not register SIGTERM handler ({e}); SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Wake the daemon for a graceful shutdown (IPC `shutdown` command — the
/// tray's "Stop service" item and Quit). Same cleanup path as SIGINT/
/// SIGTERM: flush logs, remove the IPC port file, exit. The notify future
/// is registered when `run`'s select starts — before the IPC listener can
/// accept a connection — so a shutdown request is never missed.
static SHUTDOWN_NOTIFY: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

/// Request a clean daemon shutdown (IPC `shutdown`, tray Stop service/Quit).
pub fn request_shutdown() {
    SHUTDOWN_NOTIFY.notify_waiters();
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
    // Graceful shutdown: flush logs and exit cleanly on SIGINT (Ctrl-C) and,
    // on unix, SIGTERM (e.g. `systemctl stop`).
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received; flushing logs");
        let _ = shutdown_tx.send(());
    });
    let result = tokio::select! {
        r = scheduler::run_loop(&cfg, &path) => r,
        _ = shutdown_rx => Ok(()),
        _ = SHUTDOWN_NOTIFY.notified() => {
            tracing::info!("shutdown requested over ipc");
            Ok(())
        }
    };
    // The IPC port file must not outlive the daemon: a stale port could be
    // reused by an unrelated local process (clients verify the peer with the
    // hello handshake, but the file should not advertise a dead listener).
    let _ = std::fs::remove_file(crate::ipc::port_file());
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

/// systemd --user unit (Linux). GUI builds register the default launch (no
/// subcommand) so login starts the daemon and the tray together, exactly like
/// the Windows/macOS entries; headless builds keep the daemon-only
/// `service run` for servers. A clean tray Quit exits 0, and
/// Restart=on-failure leaves it stopped, matching launchd's SuccessfulExit=false.
#[cfg(target_os = "linux")]
fn systemd_unit(exe: &Path, config: &Path) -> String {
    #[cfg(feature = "gui")]
    let exec = format!("{} --config {}", systemd_quote(exe), systemd_quote(config));
    #[cfg(not(feature = "gui"))]
    let exec = format!(
        "{} --config {} service run",
        systemd_quote(exe),
        systemd_quote(config)
    );
    format!(
        "[Unit]\nDescription=guillotine — scheduled message deletion/archival\n\n\
         [Service]\nType=simple\nExecStart={exec}\nRestart=on-failure\nRestartSec=30\n\n\
         [Install]\nWantedBy=default.target\n"
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
    // GUI builds also add the app to the application menu, so the tray is
    // launchable without a terminal. Headless builds skip this: there is no
    // tray to launch.
    #[cfg(feature = "gui")]
    {
        crate::gui::install_desktop_entry();
        if let Some(home) = dirs::home_dir() {
            println!(
                "Installed desktop entry: {}",
                home.join(".local/share/applications/guillotine.desktop")
                    .display()
            );
            let _ = Command::new("update-desktop-database")
                .arg(home.join(".local/share/applications"))
                .status();
        }
    }
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
    // Remove the application-menu entry and hicolor icon too (written by GUI
    // builds on install/first launch); harmless no-op when absent.
    if let Some(home) = dirs::home_dir() {
        let _ = std::fs::remove_file(home.join(".local/share/applications/guillotine.desktop"));
        let _ = std::fs::remove_file(
            home.join(".local/share/icons/hicolor/256x256/apps/guillotine.png"),
        );
    }
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

/// Quoting for a .desktop Exec argument: wrap in double quotes and escape the
/// spec's reserved characters (\\, ", `, $). Compiled on every OS so the
/// quoting is unit-testable everywhere.
#[cfg_attr(not(all(target_os = "linux", feature = "gui")), allow(dead_code))]
fn desktop_quote(path: &Path) -> String {
    let raw = path.display().to_string();
    assert!(!raw.contains('\n'), "path contains a newline");
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

/// The freedesktop .desktop entry (Linux GUI builds): launches the tray from
/// the application menu, with the branding icon installed into hicolor.
/// Compiled on every OS so the content is unit-testable everywhere.
#[cfg_attr(not(all(target_os = "linux", feature = "gui")), allow(dead_code))]
pub(crate) fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Message Guillotine\n\
         Comment=Scheduled bulk delete and archive of your own chat messages\n\
         Exec={} gui\nIcon=guillotine\nTerminal=false\nCategories=Utility;\n",
        desktop_quote(exe)
    )
}

/// The registry Run key command line: the default launch (no subcommand), so
/// login starts the service daemon and the tray together. Compiled on every
/// OS so the quoting is unit-testable everywhere.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn run_key_value(exe: &Path, config: &Path) -> String {
    format!("\"{}\" --config \"{}\"", exe.display(), config.display())
}

/// launchd LaunchAgent (macOS). Compiled on every OS so the escaping rules
/// are unit-testable everywhere. Runs the default launch (no subcommand) so
/// login starts the service daemon and the tray together; KeepAlive uses
/// SuccessfulExit=false so a clean tray Quit stays quit while a crash is
/// restarted.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchd_plist(exe: &Path, config: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let config = xml_escape(&config.display().to_string());
    let out = xml_escape(
        &config::data_dir()
            .join("logs/stdout.log")
            .display()
            .to_string(),
    );
    let err = xml_escape(
        &config::data_dir()
            .join("logs/stderr.log")
            .display()
            .to_string(),
    );
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>com.guillotine.service</string>\n\
         <key>ProgramArguments</key><array><string>{exe}</string><arg>--config</arg><string>{config}</string></array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
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
pub fn uninstall(_config_arg: Option<&Path>) -> anyhow::Result<()> {
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
    let value = run_key_value(&exe, &config);
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
pub fn uninstall(_config_arg: Option<&Path>) -> anyhow::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_quote_escapes_specials_and_wraps_in_quotes() {
        // plain path: just quoted
        assert_eq!(
            systemd_quote(Path::new("/usr/bin/guillotine")),
            "\"/usr/bin/guillotine\""
        );
        // backslash, quote, dollar, backtick, percent are escaped
        let tricky = Path::new("/tmp/a\\b\"c$d`e%f");
        assert_eq!(systemd_quote(tricky), "\"/tmp/a\\\\b\\\"c\\$d\\`e%%f\"");
        // spaces need no escaping inside the quotes
        assert_eq!(
            systemd_quote(Path::new("/tmp/my dir/x")),
            "\"/tmp/my dir/x\""
        );
    }

    #[cfg(all(target_os = "linux", feature = "gui"))]
    #[test]
    fn systemd_unit_gui_build_registers_the_default_launch() {
        // daemon + tray together: no subcommand on the ExecStart line
        let unit = systemd_unit(
            Path::new("/usr/bin/guillotine"),
            Path::new("/home/me/.config/guillotine/config.toml"),
        );
        assert!(unit.contains(
            "ExecStart=\"/usr/bin/guillotine\" --config \"/home/me/.config/guillotine/config.toml\"\n"
        ));
        assert!(!unit.contains("service run"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[cfg(all(target_os = "linux", not(feature = "gui")))]
    #[test]
    fn systemd_unit_headless_build_runs_the_daemon_only() {
        let unit = systemd_unit(
            Path::new("/usr/bin/guillotine"),
            Path::new("/home/me/.config/guillotine/config.toml"),
        );
        assert!(unit.contains(
            "ExecStart=\"/usr/bin/guillotine\" --config \"/home/me/.config/guillotine/config.toml\" service run\n"
        ));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn pick_service_exe_prefers_a_real_appimage_path() {
        // A real file stands in for the AppImage: it must win over current_exe.
        let real = std::env::current_exe().unwrap();
        assert_eq!(
            pick_service_exe(Some(&real), Path::new("/nonexistent/mount/guillotine")),
            real
        );
        // A stale/missing $APPIMAGE (e.g. moved after install) falls back.
        assert_eq!(
            pick_service_exe(Some(Path::new("/nonexistent/guillotine.AppImage")), &real),
            real
        );
        assert_eq!(
            pick_service_exe(None, Path::new("/usr/bin/guillotine")),
            PathBuf::from("/usr/bin/guillotine")
        );
    }

    #[test]
    fn xml_escape_escapes_markup_chars() {
        assert_eq!(xml_escape("plain"), "plain");
        assert_eq!(xml_escape("a<b>&\"c\""), "a&lt;b&gt;&amp;\"c\"");
        assert_eq!(
            xml_escape("/tmp/<weird> & \"path\""),
            "/tmp/&lt;weird&gt; &amp; \"path\""
        );
        assert_eq!(xml_escape(""), "");
    }

    #[test]
    fn desktop_entry_launches_the_tray_with_icon_and_quoted_exec() {
        let entry = desktop_entry(Path::new("/home/me/Apps/guillotine AppImage"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Type=Application"));
        assert!(entry.contains("Name=Message Guillotine"));
        // Exec launches the tray; the path is quoted so spaces cannot split it
        assert!(entry.contains("Exec=\"/home/me/Apps/guillotine AppImage\" gui\n"));
        assert!(entry.contains("Icon=guillotine"));
        assert!(entry.contains("Terminal=false"));
        // no shell-ish reserved characters survive unescaped in the path
        let entry = desktop_entry(Path::new("/tmp/a\\b\"c$d`e"));
        assert!(entry.contains("Exec=\"/tmp/a\\\\b\\\"c\\$d\\`e\" gui\n"));
    }

    #[test]
    fn run_key_value_is_the_default_launch_with_quoted_paths() {
        // no subcommand: the default launch (service + tray) with both
        // paths quoted so spaces in the exe/config path cannot split args
        let value = run_key_value(
            Path::new("C:\\Program Files\\guillotine.exe"),
            Path::new("C:\\Users\\me\\AppData\\Roaming\\guillotine\\config.toml"),
        );
        assert_eq!(
            value,
            "\"C:\\Program Files\\guillotine.exe\" --config \"C:\\Users\\me\\AppData\\Roaming\\guillotine\\config.toml\""
        );
        assert!(!value.contains("service"), "must be the default launch");
    }

    #[test]
    fn launchd_plist_uses_default_launch_and_restarts_only_on_crash() {
        let plist = launchd_plist(
            Path::new("/Applications/Guillotine.app/Contents/MacOS/guillotine"),
            Path::new("/Users/me/Library/Application Support/guillotine/config.toml"),
        );
        // default launch: exe + --config config, no "service run" arguments
        assert!(plist
            .contains("<string>/Applications/Guillotine.app/Contents/MacOS/guillotine</string>"));
        assert!(plist.contains(
            "<string>/Users/me/Library/Application Support/guillotine/config.toml</string>"
        ));
        assert!(!plist.contains("<arg>service</arg>"));
        assert!(!plist.contains("<arg>run</arg>"));
        // a & in the config path must be escaped, never injected raw
        assert!(plist.contains("&amp;") || !plist.contains('&'));
        // run at login, restart only after a non-clean exit (crash), so a
        // tray Quit is respected
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(
            plist.contains("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>")
        );
    }
}
