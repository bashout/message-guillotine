//! Configuration: TOML file at the per-OS config path, with environment
//! overrides. Holds the token, the archive folder, the DM scope policy
//! (nominated users or an exclusion list — never an unconditional "all"),
//! and one entry per chat with its own schedule, retention and
//! archive-before-delete flag.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context};
use chrono::NaiveTime;
use serde::{Deserialize, Deserializer};

/// Environment variable that overrides the token stored in the config file.
pub const ENV_TOKEN: &str = "GUILLOTINE_TOKEN";
/// Environment variable pointing at a non-default config file.
pub const ENV_CONFIG: &str = "GUILLOTINE_CONFIG";
/// V1-compatible environment variable overriding the archive folder.
pub const ENV_ARCHIVE_DIR: &str = "DM_ARCHIVE_DIR";

/// Per-OS configuration directory (holds config.toml).
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("guillotine")
}

/// Per-OS data directory (holds archives and deletion sessions by default).
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("guillotine")
}

pub fn default_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn default_archive_dir() -> PathBuf {
    data_dir().join("archives")
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("deletion_sessions")
}

/// Write a file with owner-only permissions (0600 on unix), creating it if
/// needed. Used for session files, last_runs, and other private state.
pub fn write_file_private(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
            .write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// Create the per-OS data dir with owner-only permissions (0700) and return
/// it. Safe to call repeatedly.
pub fn ensure_data_dir() -> std::io::Result<PathBuf> {
    let dir = data_dir();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir)?;
    }
    // An existing dir is never re-chmodded by DirBuilder; enforce 0700.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&dir) {
            if meta.permissions().mode() & 0o077 != 0 {
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    Ok(dir)
}

/// How DM chats are brought into scope. There is deliberately no "all" mode:
/// either individual users are nominated, or specific users are excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DmScope {
    Nominate,
    Exclude,
}

/// Schedule for one chat: how often a deletion run fires, and at what local
/// time of day (default 04:00). `monthly` fires on the 1st of the month,
/// `weekly` on the weekday the config was created (or a named weekday).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub kind: ScheduleKind,
    pub at: NaiveTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleKind {
    Daily,
    Weekly,
    Monthly,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            kind: ScheduleKind::Daily,
            at: NaiveTime::from_hms_opt(4, 0, 0).unwrap(),
        }
    }
}

impl FromStr for Schedule {
    type Err = anyhow::Error;

    /// Accepted: "daily", "weekly", "monthly", each optionally followed by
    /// "at HH:MM" (24h local time).
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (head, at) = match s.split_once(" at ") {
            Some((h, t)) => (h.trim(), Some(t.trim())),
            None => (s.trim(), None),
        };
        let kind = match head.to_ascii_lowercase().as_str() {
            "daily" => ScheduleKind::Daily,
            "weekly" => ScheduleKind::Weekly,
            "monthly" => ScheduleKind::Monthly,
            other => bail!("unknown schedule {other:?}: expected daily, weekly or monthly"),
        };
        let at = match at {
            Some(t) => NaiveTime::parse_from_str(t, "%H:%M")
                .with_context(|| format!("invalid time {t:?}: expected HH:MM"))?,
            None => NaiveTime::from_hms_opt(4, 0, 0).unwrap(),
        };
        Ok(Schedule { kind, at })
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ScheduleKind::Daily => "daily",
            ScheduleKind::Weekly => "weekly",
            ScheduleKind::Monthly => "monthly",
        };
        write!(f, "{kind} at {}", self.at.format("%H:%M"))
    }
}

impl<'de> Deserialize<'de> for Schedule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Schedule::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

/// A chat target: a DM with a user, or a channel inside a guild.
/// Serialized in config as "dm:<user_id>" or "guild:<guild_id>/channel:<channel_id>".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChatKey {
    Dm(String),
    GuildChannel { guild: String, channel: String },
}

/// Snowflake ids are all digits; anything else is rejected so chat keys can
/// never traverse paths in session/archive file names or API URLs.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

impl ChatKey {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        if let Some(user) = raw.strip_prefix("dm:") {
            if !valid_id(user) {
                bail!("invalid chat key {raw:?}: user id must be numeric");
            }
            return Ok(ChatKey::Dm(user.to_string()));
        }
        if let Some(rest) = raw.strip_prefix("guild:") {
            if let Some((guild, channel)) = rest.split_once("/channel:") {
                if !valid_id(guild) || !valid_id(channel) {
                    bail!("invalid chat key {raw:?}: guild/channel ids must be numeric");
                }
                return Ok(ChatKey::GuildChannel {
                    guild: guild.to_string(),
                    channel: channel.to_string(),
                });
            }
            bail!("invalid chat key {raw:?}: expected guild:<id>/channel:<id>");
        }
        bail!("invalid chat key {raw:?}: expected dm:<user_id> or guild:<id>/channel:<id>")
    }
}

impl fmt::Display for ChatKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatKey::Dm(user) => write!(f, "dm:{user}"),
            ChatKey::GuildChannel { guild, channel } => {
                write!(f, "guild:{guild}/channel:{channel}")
            }
        }
    }
}

/// One chat's settings: when to run, what to keep, whether to archive first.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatConfig {
    pub schedule: Schedule,
    pub retention_days: u32,
    #[serde(default)]
    pub archive_before_delete: bool,
}

/// The [dm] section: scope policy plus defaults for DM chats that have no
/// own [chat] entry (used under the "exclude" policy).
#[derive(Debug, Clone, Deserialize)]
pub struct DmPolicy {
    pub policy: DmScope,
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub default_schedule: Schedule,
    #[serde(default = "default_retention")]
    pub default_retention_days: u32,
    #[serde(default)]
    pub default_archive_before_delete: bool,
}

fn default_retention() -> u32 {
    30
}

impl Default for DmPolicy {
    fn default() -> Self {
        DmPolicy {
            policy: DmScope::Exclude,
            users: Vec::new(),
            default_schedule: Schedule::default(),
            default_retention_days: default_retention(),
            default_archive_before_delete: false,
        }
    }
}

/// Root configuration file.
#[derive(Clone, Deserialize)]
pub struct Config {
    /// Token may be omitted here when GUILLOTINE_TOKEN is set.
    pub token: Option<String>,
    pub archive_dir: Option<PathBuf>,
    /// Optional path to the V1 Python archive viewer (archive_viewer.py);
    /// when unset, archive folders open in the OS file manager instead.
    pub archive_viewer: Option<PathBuf>,
    #[serde(default)]
    pub dm: DmPolicy,
    /// Raw chat table keyed by strings like "dm:123"; parsed into ChatKey by
    /// `chats()` so a bad key is a load error, not a silent skip.
    #[serde(default)]
    pub chat: BTreeMap<String, ChatConfig>,
}

impl Config {
    /// Load and validate the config from `path`, applying environment
    /// overrides for the token and archive dir.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("invalid config file {}", path.display()))?;
        if let Some(token) = std::env::var_os(ENV_TOKEN) {
            cfg.token = Some(token.to_string_lossy().into_owned());
        }
        if let Some(dir) = std::env::var_os(ENV_ARCHIVE_DIR) {
            cfg.archive_dir = Some(PathBuf::from(dir));
        }
        cfg.validate(path)?;
        Ok(cfg)
    }

    fn validate(&self, path: &Path) -> anyhow::Result<()> {
        if self.token.as_deref().is_none_or(|t| t.trim().is_empty()) {
            bail!(
                "no token: set `token` in {} or the {ENV_TOKEN} environment variable",
                path.display()
            );
        }
        for (key, chat) in &self.chat {
            if chat.retention_days > 36_500 {
                bail!("retention_days for chat {key} is {}; max 36500 (100 years)", chat.retention_days);
            }
        }
        if self.dm.default_retention_days > 36_500 {
            bail!("dm.default_retention_days is {}; max 36500", self.dm.default_retention_days);
        }
        for key in self.chat.keys() {
            ChatKey::parse(key)
                .with_context(|| format!("invalid [chat] key {key:?} in {}", path.display()))?;
        }
        if self.dm.users.is_empty() {
            tracing::warn!(
                "dm.users is empty: under the {:?} policy no DM chats will be in scope",
                self.dm.policy
            );
        }
        Ok(())
    }

    /// The configured chats as parsed keys. DM chats listed here are
    /// intersected with the dm policy when resolving the run scope.
    pub fn chats(&self) -> impl Iterator<Item = (ChatKey, &ChatConfig)> {
        self.chat
            .iter()
            .map(|(k, v)| (ChatKey::parse(k).unwrap(), v))
    }

    /// The token, from the environment override or the file.
    pub fn token(&self) -> anyhow::Result<&str> {
        self.token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no token configured (set `token` or {ENV_TOKEN})"))
    }

    /// Archive folder resolution, same priority as V1: explicit argument,
    /// then DM_ARCHIVE_DIR, then the config file, then the per-OS default.
    pub fn resolve_archive_dir(&self, explicit: Option<&Path>) -> PathBuf {
        if let Some(p) = explicit {
            return p.to_path_buf();
        }
        if let Some(p) = std::env::var_os(ENV_ARCHIVE_DIR) {
            return PathBuf::from(p);
        }
        if let Some(p) = &self.archive_dir {
            return p.clone();
        }
        default_archive_dir()
    }

    /// Effective settings for a DM chat: its own [chat] entry when present,
    /// otherwise the dm defaults for any in-scope user (nominated or not
    /// excluded). None when the user is out of scope.
    pub fn dm_chat_settings(&self, user_id: &str) -> Option<ChatConfig> {
        if let Some(own) = self.chat.get(&format!("dm:{user_id}")) {
            return Some(own.clone());
        }
        if self.dm_user_in_scope(user_id) {
            return Some(ChatConfig {
                schedule: self.dm.default_schedule,
                retention_days: self.dm.default_retention_days,
                archive_before_delete: self.dm.default_archive_before_delete,
            });
        }
        None
    }

    /// Whether a DM user is in scope at all under the dm policy.
    pub fn dm_user_in_scope(&self, user_id: &str) -> bool {
        match self.dm.policy {
            DmScope::Nominate => self.dm.users.iter().any(|u| u == user_id),
            DmScope::Exclude => !self.dm.users.iter().any(|u| u == user_id),
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never leak the token into logs or error chains.
        f.debug_struct("Config")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("archive_dir", &self.archive_dir)
            .field("archive_viewer", &self.archive_viewer)
            .field("dm", &self.dm)
            .field("chat", &self.chat)
            .finish()
    }
}

/// The example config written by `guillotine config init`.
pub const EXAMPLE_CONFIG: &str = r##"# guillotine configuration
# Token may also come from the GUILLOTINE_TOKEN environment variable.
token = "YOUR_USER_TOKEN"
# archive_dir = "/path/to/archives"   # default: per-OS data dir /guillotine/archives

# How DM chats are scoped. There is no "all" mode:
#   policy = "nominate" -> only the users listed below are in scope
#   policy = "exclude"  -> every DM except the users listed below is in scope
# DMs without their own [chat] entry use the defaults below (exclude policy).
[dm]
policy = "exclude"
users = ["USER_ID_TO_EXCLUDE"]
default_schedule = "weekly"
default_retention_days = 30
default_archive_before_delete = true

# Each chat has its own schedule, retention and archive flag.
# schedule: daily | weekly | monthly, optional "at HH:MM" (default 04:00).
# retention_days: the most recent N days of your messages are kept;
# on each run everything older is deleted.
# archive_before_delete: archive the chat first, appending to the existing
# archive for this chat when one is present.

# [chat."dm:USER_ID"]
# schedule = "monthly"
# retention_days = 7
# archive_before_delete = true

# [chat."guild:GUILD_ID/channel:CHANNEL_ID"]
# schedule = "daily"
# retention_days = 30
# archive_before_delete = false
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_parsing() {
        let s = Schedule::from_str("monthly").unwrap();
        assert_eq!(s.kind, ScheduleKind::Monthly);
        assert_eq!(s.at, NaiveTime::from_hms_opt(4, 0, 0).unwrap());
        let s = Schedule::from_str("daily at 23:30").unwrap();
        assert_eq!(s.kind, ScheduleKind::Daily);
        assert_eq!(s.at, NaiveTime::from_hms_opt(23, 30, 0).unwrap());
        assert!(Schedule::from_str("hourly").is_err());
        assert!(Schedule::from_str("daily at 25:00").is_err());
    }

    #[test]
    fn chat_key_roundtrip() {
        assert_eq!(ChatKey::parse("dm:123").unwrap(), ChatKey::Dm("123".into()));
        assert_eq!(
            ChatKey::parse("guild:1/channel:2").unwrap(),
            ChatKey::GuildChannel {
                guild: "1".into(),
                channel: "2".into()
            }
        );
        assert_eq!(ChatKey::Dm("123".into()).to_string(), "dm:123");
        assert!(ChatKey::parse("channel:2").is_err());
        assert!(ChatKey::parse("dm:").is_err());
    }

    #[test]
    fn dm_scope_never_all() {
        let toml = r#"
token = "t"
[dm]
policy = "exclude"
users = ["9"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.dm_user_in_scope("1"));
        assert!(!cfg.dm_user_in_scope("9"));
        // Exclude policy: user 1 has no own entry, gets the defaults.
        assert!(cfg.dm_chat_settings("1").is_some());
        assert!(cfg.dm_chat_settings("9").is_none());

        let toml = r#"
token = "t"
[dm]
policy = "nominate"
users = ["1"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.dm_user_in_scope("1"));
        assert!(!cfg.dm_user_in_scope("2"));
        // Nominate policy without an entry: not in scope at all.
        assert!(cfg.dm_chat_settings("2").is_none());
    }

    #[test]
    fn per_chat_entry_overrides_dm_defaults() {
        let toml = r#"
token = "t"
[dm]
policy = "exclude"
users = []
default_retention_days = 30
[chat."dm:5"]
schedule = "monthly"
retention_days = 7
archive_before_delete = true
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let own = cfg.dm_chat_settings("5").unwrap();
        assert_eq!(own.retention_days, 7);
        assert!(own.archive_before_delete);
        assert_eq!(own.schedule.kind, ScheduleKind::Monthly);
        let other = cfg.dm_chat_settings("6").unwrap();
        assert_eq!(other.retention_days, 30);
        assert!(!other.archive_before_delete);
    }

    #[test]
    fn token_env_override() {
        let toml = r#"archive_dir = "/tmp/x""#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.token.is_none());
    }

    #[test]
    fn schedule_parsing_at_times_and_invalid() {
        let s = Schedule::from_str("weekly at 09:15").unwrap();
        assert_eq!(s.kind, ScheduleKind::Weekly);
        assert_eq!(s.at, NaiveTime::from_hms_opt(9, 15, 0).unwrap());
        // default 04:00 when no "at" clause
        assert_eq!(Schedule::from_str("weekly").unwrap().at, NaiveTime::from_hms_opt(4, 0, 0).unwrap());
        // case-insensitive kind
        assert_eq!(Schedule::from_str("MONTHLY").unwrap().kind, ScheduleKind::Monthly);
        // invalid forms
        assert!(Schedule::from_str("daily at 25:00").is_err());
        assert!(Schedule::from_str("daily at 12:60").is_err());
        assert!(Schedule::from_str("daily at noon").is_err());
        assert!(Schedule::from_str("daily at").is_err());
        assert!(Schedule::from_str("").is_err());
        assert!(Schedule::from_str("every day").is_err());
        // Display round-trips through FromStr
        let s = Schedule::from_str("monthly at 23:45").unwrap();
        assert_eq!(Schedule::from_str(&s.to_string()).unwrap(), s);
    }

    #[test]
    fn chat_key_rejects_non_numeric_ids() {
        // traversal attempts must never reach the filesystem
        assert!(ChatKey::parse("dm:../x").is_err());
        assert!(ChatKey::parse("dm:..").is_err());
        assert!(ChatKey::parse("guild:1/channel:2/3").is_err());
        assert!(ChatKey::parse("guild:../channel:2").is_err());
        assert!(ChatKey::parse("guild:1/channel:2x").is_err());
        assert!(ChatKey::parse("dm:12 34").is_err());
        assert!(ChatKey::parse("dm:").is_err());
        assert!(ChatKey::parse("guild:1/channel:").is_err());
        assert!(ChatKey::parse("guild:1").is_err());
        assert!(ChatKey::parse("").is_err());
        // valid keys round-trip
        for raw in ["dm:123", "guild:1/channel:2"] {
            assert_eq!(ChatKey::parse(raw).unwrap().to_string(), raw);
        }
    }

    #[test]
    fn dm_scope_matrix() {
        // nominate: only listed users; own entry wins over defaults
        let cfg: Config = toml::from_str(
            r#"token = "t"
[dm]
policy = "nominate"
users = ["1"]
default_retention_days = 30
[chat."dm:1"]
schedule = "daily"
retention_days = 7
"#,
        )
        .unwrap();
        assert!(cfg.dm_user_in_scope("1"));
        assert!(!cfg.dm_user_in_scope("2"));
        assert_eq!(cfg.dm_chat_settings("1").unwrap().retention_days, 7);
        assert!(cfg.dm_chat_settings("2").is_none(), "nominate: unlisted user not in scope at all");

        // exclude: everyone except listed; unlisted users get dm defaults
        let cfg: Config = toml::from_str(
            r#"token = "t"
[dm]
policy = "exclude"
users = ["9"]
default_retention_days = 30
default_schedule = "weekly"
"#,
        )
        .unwrap();
        assert!(cfg.dm_user_in_scope("2"));
        assert!(!cfg.dm_user_in_scope("9"));
        let s = cfg.dm_chat_settings("2").unwrap();
        assert_eq!(s.retention_days, 30);
        assert_eq!(s.schedule.kind, ScheduleKind::Weekly);
        assert!(cfg.dm_chat_settings("9").is_none());
    }

    fn temp_config(tag: &str, body: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "guillotine-config-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn retention_clamp_validation() {
        // over the clamp: rejected for a chat entry
        let path = temp_config("clamp1", "token = \"t\"\n[chat.\"dm:1\"]\nschedule = \"daily\"\nretention_days = 36501\n");
        assert!(Config::load(&path).is_err());
        // over the clamp: rejected for the dm default
        let path = temp_config("clamp2", "token = \"t\"\n[dm]\npolicy = \"exclude\"\ndefault_retention_days = 40000\n");
        assert!(Config::load(&path).is_err());
        // exactly at the clamp: accepted
        let path = temp_config("clamp3", "token = \"t\"\n[dm]\npolicy = \"exclude\"\ndefault_retention_days = 36500\n[chat.\"dm:1\"]\nschedule = \"daily\"\nretention_days = 36500\n");
        assert!(Config::load(&path).is_ok());
        // invalid chat key in [chat] table is a load error
        let path = temp_config("clamp4", "token = \"t\"\n[chat.\"dm:../evil\"]\nschedule = \"daily\"\nretention_days = 1\n");
        assert!(Config::load(&path).is_err());
        // missing token entirely is a load error
        let path = temp_config("clamp5", "[dm]\npolicy = \"exclude\"\n");
        assert!(Config::load(&path).is_err());
        for tag in ["clamp1", "clamp2", "clamp3", "clamp4", "clamp5"] {
            let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("guillotine-config-test-{}-{tag}", std::process::id())));
        }
    }

    #[test]
    fn archive_dir_resolution_priority_and_token_env() {
        // serialize env mutation within this test binary
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let path = temp_config("prio", "token = \"file-token\"\narchive_dir = \"/config/dir\"\n");
        let cfg = Config::load(&path).unwrap();
        // config file value
        std::env::remove_var(ENV_ARCHIVE_DIR);
        assert_eq!(cfg.resolve_archive_dir(None), PathBuf::from("/config/dir"));
        // env beats config
        std::env::set_var(ENV_ARCHIVE_DIR, "/env/dir");
        assert_eq!(cfg.resolve_archive_dir(None), PathBuf::from("/env/dir"));
        // explicit beats env
        assert_eq!(
            cfg.resolve_archive_dir(Some(Path::new("/explicit/dir"))),
            PathBuf::from("/explicit/dir")
        );
        std::env::remove_var(ENV_ARCHIVE_DIR);
        // neither env nor config -> per-OS default
        let path2 = temp_config("prio2", "token = \"t\"\n");
        let cfg2 = Config::load(&path2).unwrap();
        assert_eq!(cfg2.resolve_archive_dir(None), default_archive_dir());

        // token env override wins over the file token
        std::env::set_var(ENV_TOKEN, "env-token");
        let cfg3 = Config::load(&path2).unwrap();
        assert_eq!(cfg3.token().unwrap(), "env-token");
        std::env::remove_var(ENV_TOKEN);
        let cfg4 = Config::load(&path2).unwrap();
        assert_eq!(cfg4.token().unwrap(), "t");

        // Debug redacts the token
        let dbg = format!("{:?}", cfg4);
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("file-token"));

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("guillotine-config-test-{}-prio", std::process::id())));
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("guillotine-config-test-{}-prio2", std::process::id())));
    }
}
