//! Load/save of `~/.config/teamagent.json` — atomic writes, 0600 perms, and
//! read-merge-write so server and CLI can edit concurrently.

pub mod migrate;
pub mod schema;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub use schema::{
    AccountConfig, AccountCredential, CodexConfig, Config, ProxyConfig, RoutingConfig,
    SchedulerConfig, Upsert,
};

/// Environment variable overriding the config file location.
pub const CONFIG_ENV: &str = "TEAMAGENT_CONFIG";

/// Prefix of auto-generated proxy api keys.
const API_KEY_PREFIX: &str = "ta-";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported config version {0}")]
    UnsupportedVersion(u32),
    #[error("could not determine config directory")]
    NoConfigDir,
    #[error("invalid import data: {0}")]
    Invalid(String),
}

fn io_err(path: &Path, source: std::io::Error) -> ConfigError {
    ConfigError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Resolve the config path: `$TEAMAGENT_CONFIG` if set, else
/// `$XDG_CONFIG_HOME/teamagent.json`, else `~/.config/teamagent.json`.
///
/// Deliberately NOT `dirs::config_dir()`: on macOS that is
/// `~/Library/Application Support`, but the contract (FR2, teamclaude
/// compatibility) is `~/.config` on every Unix platform.
pub fn config_path() -> Result<PathBuf, ConfigError> {
    if let Some(path) = std::env::var_os(CONFIG_ENV) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    xdg_config_dir()
        .map(|dir| dir.join("teamagent.json"))
        .ok_or(ConfigError::NoConfigDir)
}

/// `$XDG_CONFIG_HOME` when set and non-empty, else `~/.config`.
pub(crate) fn xdg_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    dirs::home_dir().map(|home| home.join(".config"))
}

/// Load the config from [`config_path`]. A missing file yields
/// `Config::default()` (first run); nothing is written — use
/// [`load_or_init`] to also create the file with a fresh api key.
pub fn load() -> Result<Config, ConfigError> {
    load_path(&config_path()?)
}

/// [`load`] against an explicit path.
pub fn load_path(path: &Path) -> Result<Config, ConfigError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(err) => return Err(io_err(path, err)),
    };
    let config: Config = serde_json::from_str(&raw)?;
    if config.version != 1 {
        return Err(ConfigError::UnsupportedVersion(config.version));
    }
    Ok(config)
}

/// Load the config, creating it on first run: when the file does not exist,
/// a default config with a freshly generated proxy api key is written
/// (mode 0600) and returned.
pub fn load_or_init() -> Result<Config, ConfigError> {
    load_or_init_path(&config_path()?)
}

/// [`load_or_init`] against an explicit path.
pub fn load_or_init_path(path: &Path) -> Result<Config, ConfigError> {
    if path.exists() {
        return load_path(path);
    }
    let mut config = Config::default();
    config.proxy.api_key = Some(generate_api_key());
    save_path(path, &config)?;
    tracing::info!(path = %path.display(), "created config");
    Ok(config)
}

/// Atomically persist `config` (write temp file mode 0600 in the same
/// directory, fsync, then rename over the target).
pub fn save(config: &Config) -> Result<(), ConfigError> {
    save_path(&config_path()?, config)
}

/// [`save`] against an explicit path.
pub fn save_path(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
            dir.to_path_buf()
        }
        _ => PathBuf::from("."),
    };

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("teamagent.json");
    let tmp = dir.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        ulid::Ulid::new()
    ));

    let mut data = serde_json::to_vec_pretty(config)?;
    data.push(b'\n');

    let result = write_tmp_and_rename(&tmp, path, &data);
    if result.is_err() {
        // Best-effort cleanup of the orphaned temp file.
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_tmp_and_rename(tmp: &Path, path: &Path, data: &[u8]) -> Result<(), ConfigError> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp).map_err(|e| io_err(tmp, e))?;
    file.write_all(data).map_err(|e| io_err(tmp, e))?;
    file.sync_all().map_err(|e| io_err(tmp, e))?;
    drop(file);

    fs::rename(tmp, path).map_err(|e| io_err(path, e))?;

    // Best-effort directory fsync so the rename itself is durable.
    if let Some(dir) = path.parent() {
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Read-merge-write update: re-reads the file, applies `mutate` to the
/// freshest on-disk state, and saves atomically.
/// This is the ONLY safe way to write when the server may also be writing
/// (e.g. persisting refreshed tokens) — never `save(load()?)` around edits.
///
/// Concurrency contract: callers express intent as *merges* on the fresh
/// state — `Config::upsert_account` (keyed by `account_uuid`/`name`) and
/// `Config::update_oauth_tokens` (keyed by account identity) — never as a
/// blind overwrite of a stale in-memory snapshot. Two writers each doing
/// `update(|c| c.upsert_account(...))` therefore both land: each rewrite
/// starts from the other's persisted accounts.
pub fn update<F>(mutate: F) -> Result<Config, ConfigError>
where
    F: FnOnce(&mut Config),
{
    update_path(&config_path()?, mutate)
}

/// [`update`] against an explicit path.
pub fn update_path<F>(path: &Path, mutate: F) -> Result<Config, ConfigError>
where
    F: FnOnce(&mut Config),
{
    let mut config = load_path(path)?;
    mutate(&mut config);
    save_path(path, &config)?;
    Ok(config)
}

/// Generate a proxy api key: `ta-` + 32 random bytes, base64url (no pad).
pub fn generate_api_key() -> String {
    use base64::Engine as _;
    format!(
        "{API_KEY_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes_32())
    )
}

/// 32 bytes of entropy. `/dev/urandom` on Unix (the only tier-1 targets are
/// macOS/Linux); falls back to hashing rand-backed ULIDs + time + pid, which
/// still carries ≥256 bits of entropy through SHA-256.
fn random_bytes_32() -> [u8; 32] {
    #[cfg(unix)]
    {
        use std::io::Read as _;
        if let Ok(mut f) = fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 32];
            if f.read_exact(&mut buf).is_ok() {
                return buf;
            }
        }
    }
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for _ in 0..4 {
        hasher.update(ulid::Ulid::new().to_bytes());
    }
    if let Ok(elapsed) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hasher.update(elapsed.as_nanos().to_le_bytes());
    }
    hasher.update(std::process::id().to_le_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-cleaning unique temp dir (no tempfile dev-dependency).
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "teamagent-test-{}-{}",
                std::process::id(),
                ulid::Ulid::new()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn oauth_account(name: &str, uuid: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Oauth {
                account_uuid: uuid.to_string(),
                access_token: format!("at-{name}"),
                refresh_token: format!("rt-{name}"),
                expires_at_ms: 1_750_000_000_000,
                tier: None,
                last_refresh_ms: None,
            },
        }
    }

    fn apikey_account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Apikey {
                api_key: format!("sk-ant-api03-{name}"),
            },
        }
    }

    fn codex_account(name: &str, account_id: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Codex {
                account_id: account_id.to_string(),
                access_token: format!("at-{name}"),
                refresh_token: format!("rt-{name}"),
                expires_at_ms: 1_750_000_000_000,
                last_refresh_ms: None,
            },
        }
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");
        let config = load_path(&path).expect("load");
        assert_eq!(config, Config::default());
        assert!(!path.exists(), "plain load must not create the file");
    }

    #[test]
    fn load_or_init_creates_file_with_api_key_and_0600() {
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");
        let config = load_or_init_path(&path).expect("init");

        let key = config.proxy.api_key.as_deref().expect("api key generated");
        assert!(key.starts_with("ta-"), "prefix: {key}");
        // 32 bytes -> 43 base64url chars, no padding.
        assert_eq!(key.len(), 3 + 43, "key length: {key}");
        assert!(path.exists());

        // Second init must NOT regenerate the key.
        let again = load_or_init_path(&path).expect("reload");
        assert_eq!(again.proxy.api_key, config.proxy.api_key);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        }
    }

    #[test]
    fn partial_file_fills_defaults() {
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");
        fs::write(&path, r#"{ "proxy": { "port": 9999 } }"#).expect("write");

        let config = load_path(&path).expect("load");
        assert_eq!(config.version, 1);
        assert_eq!(config.proxy.port, 9999);
        assert_eq!(config.proxy.api_key, None);
        assert_eq!(config.upstream, schema::DEFAULT_UPSTREAM);
        assert!((config.scheduler.five_hour_max - 0.90).abs() < f64::EPSILON);
        assert!((config.scheduler.seven_day_max - 0.99).abs() < f64::EPSILON);
        assert_eq!(config.scheduler.usage_poll_secs, 300);
        assert_eq!(config.scheduler.usage_max_age_secs, 600);
        assert_eq!(config.scheduler.refresh_ahead_secs, 7 * 3600);
        assert!(config.accounts.is_empty());
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");

        let mut config = Config::default();
        config.proxy.port = 4000;
        config.proxy.api_key = Some("ta-test".into());
        config.upstream = "https://example.test".into();
        config.scheduler.five_hour_max = 0.5;
        config.accounts.push(oauth_account("a@x.com", "uuid-a"));
        config.accounts.push(apikey_account("api-1"));
        config.accounts.push(codex_account("cx@x.com", "acct-cx"));
        config.codex.upstream = "https://codex.test/backend".into();
        config.codex.token_url = "https://codex.test/oauth/token".into();

        save_path(&path, &config).expect("save");
        let loaded = load_path(&path).expect("load");
        assert_eq!(loaded, config);
    }

    #[test]
    fn codex_credential_serializes_with_type_codex_and_defaults_apply() {
        let json = serde_json::to_value(codex_account("cx", "acct-1")).expect("json");
        assert_eq!(json["type"], "codex");
        assert_eq!(json["account_id"], "acct-1");

        // A config without a codex section gets the production defaults.
        let config: Config = serde_json::from_str(r#"{ "version": 1 }"#).expect("parse");
        assert_eq!(config.codex.upstream, schema::DEFAULT_CODEX_UPSTREAM);
        assert_eq!(config.codex.token_url, schema::DEFAULT_CODEX_TOKEN_URL);
    }

    #[test]
    fn routing_config_is_additive_and_defaults_to_disabled() {
        // A config written before routing existed (no `routing` key) loads
        // with routing OFF — the backward-compat guarantee. enabled=false ⇒
        // exactly today's behavior.
        let config: Config =
            serde_json::from_str(r#"{ "version": 1 }"#).expect("old config parses");
        assert!(!config.routing.enabled, "routing defaults to disabled");
        assert_eq!(config.routing.default_group, "claude");
        assert_eq!(config.routing.on_empty_group, "error");
        assert!(config.routing.claude_models.is_empty());
        assert!(config.routing.codex_models.is_empty());

        // An explicit routing block round-trips through save→load.
        let raw = r#"{
            "version": 1,
            "routing": {
                "enabled": true,
                "codex_models": ["gpt-", "~codex"],
                "default_group": "codex",
                "on_empty_group": "fallback"
            }
        }"#;
        let config: Config = serde_json::from_str(raw).expect("routing config parses");
        assert!(config.routing.enabled);
        assert_eq!(config.routing.codex_models, vec!["gpt-", "~codex"]);
        assert_eq!(config.routing.default_group, "codex");
        assert_eq!(config.routing.on_empty_group, "fallback");
        let reparsed: Config =
            serde_json::from_str(&serde_json::to_string(&config).expect("serialize"))
                .expect("re-parse");
        assert_eq!(reparsed.routing, config.routing);
    }

    #[test]
    fn codex_accounts_dedup_by_account_id_and_update_tokens() {
        let mut config = Config::default();
        config.accounts.push(codex_account("codex-old", "acct-1"));

        // Re-import with the same account_id replaces, never duplicates.
        let outcome = config.upsert_account(codex_account("cx@x.com", "acct-1"));
        assert_eq!(outcome, Upsert::Updated);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "cx@x.com");

        // Refreshed codex tokens persist through the shared updater.
        assert!(config.update_oauth_tokens("acct-1", "at-new", Some("rt-new"), 99, 77));
        match &config.accounts[0].credential {
            AccountCredential::Codex {
                access_token,
                refresh_token,
                expires_at_ms,
                last_refresh_ms,
                ..
            } => {
                assert_eq!(access_token, "at-new");
                assert_eq!(refresh_token, "rt-new");
                assert_eq!(*expires_at_ms, 99);
                assert_eq!(*last_refresh_ms, Some(77), "refresh stamps the timestamp");
            }
            other => panic!("unexpected credential {other:?}"),
        }
    }

    #[test]
    fn last_refresh_ms_is_additive_and_round_trips() {
        // Pre-upgrade config (no last_refresh_ms anywhere) loads unchanged.
        let raw = r#"{
            "version": 1,
            "accounts": [
                { "name": "a@x.com", "type": "oauth", "account_uuid": "uuid-a",
                  "access_token": "at", "refresh_token": "rt", "expires_at_ms": 42 },
                { "name": "cx", "type": "codex", "account_id": "acct-1",
                  "access_token": "at", "refresh_token": "rt", "expires_at_ms": 42 }
            ]
        }"#;
        let config: Config = serde_json::from_str(raw).expect("old config parses");
        assert_eq!(config.accounts[0].credential.last_refresh_ms(), None);
        assert_eq!(config.accounts[1].credential.last_refresh_ms(), None);

        // None is omitted on write (the file stays byte-compatible until
        // the first refresh actually happens).
        let json = serde_json::to_value(&config.accounts[0]).expect("json");
        assert!(
            json.get("last_refresh_ms").is_none(),
            "None omitted: {json}"
        );

        // A stamped refresh round-trips through save/load.
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");
        let mut config = config;
        assert!(config.update_oauth_tokens("uuid-a", "at-new", None, 99, 88));
        save_path(&path, &config).expect("save");
        let loaded = load_path(&path).expect("load");
        assert_eq!(loaded.accounts[0].credential.last_refresh_ms(), Some(88));
        assert_eq!(loaded, config);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");
        fs::write(&path, r#"{ "version": 2 }"#).expect("write");
        match load_path(&path) {
            Err(ConfigError::UnsupportedVersion(2)) => {}
            other => panic!("expected UnsupportedVersion(2), got {other:?}"),
        }
    }

    #[test]
    fn update_two_writers_preserve_each_others_accounts() {
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");
        save_path(&path, &Config::default()).expect("seed");

        // Both "processes" hold the same stale snapshot, then write
        // through update(): each upsert is re-applied to fresh disk state,
        // so neither write clobbers the other.
        let _stale_a = load_path(&path).expect("stale a");
        let _stale_b = load_path(&path).expect("stale b");

        update_path(&path, |c| {
            c.upsert_account(oauth_account("a@x.com", "uuid-a"));
        })
        .expect("writer a");
        update_path(&path, |c| {
            c.upsert_account(oauth_account("b@x.com", "uuid-b"));
        })
        .expect("writer b");

        let merged = load_path(&path).expect("load");
        let names: Vec<_> = merged.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a@x.com", "b@x.com"]);
    }

    #[test]
    fn upsert_matches_uuid_over_name() {
        let mut config = Config::default();
        config.accounts.push(oauth_account("old-name", "uuid-a"));
        config.accounts.push(apikey_account("api-1"));

        // Same uuid, new name -> replaces in place (re-login rename).
        let outcome = config.upsert_account(oauth_account("new@x.com", "uuid-a"));
        assert_eq!(outcome, Upsert::Updated);
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.accounts[0].name, "new@x.com");

        // Unknown uuid, unknown name -> appended.
        let outcome = config.upsert_account(oauth_account("c@x.com", "uuid-c"));
        assert_eq!(outcome, Upsert::Added);
        assert_eq!(config.accounts.len(), 3);

        // No uuid -> falls back to name match.
        let outcome = config.upsert_account(apikey_account("api-1"));
        assert_eq!(outcome, Upsert::Updated);
        assert_eq!(config.accounts.len(), 3);
    }

    #[test]
    fn update_oauth_tokens_preserves_refresh_on_none() {
        let mut config = Config::default();
        config.accounts.push(oauth_account("a@x.com", "uuid-a"));

        assert!(config.update_oauth_tokens("uuid-a", "at-new", None, 42, 41));
        match &config.accounts[0].credential {
            AccountCredential::Oauth {
                access_token,
                refresh_token,
                expires_at_ms,
                last_refresh_ms,
                ..
            } => {
                assert_eq!(access_token, "at-new");
                assert_eq!(refresh_token, "rt-a@x.com", "refresh preserved");
                assert_eq!(*expires_at_ms, 42);
                assert_eq!(*last_refresh_ms, Some(41), "refresh stamps the timestamp");
            }
            other => panic!("unexpected credential {other:?}"),
        }

        // Match by name too; unknown identity is reported.
        assert!(config.update_oauth_tokens("a@x.com", "at-2", Some("rt-2"), 43, 42));
        assert!(!config.update_oauth_tokens("nobody", "at", None, 0, 0));
    }

    #[test]
    fn remove_account_by_name() {
        let mut config = Config::default();
        config.accounts.push(oauth_account("a@x.com", "uuid-a"));
        assert!(config.remove_account("a@x.com"));
        assert!(!config.remove_account("a@x.com"));
        assert!(config.accounts.is_empty());
    }

    #[test]
    fn atomic_save_leaves_no_temp_files() {
        let dir = TempDir::new();
        let path = dir.path().join("teamagent.json");
        save_path(&path, &Config::default()).expect("save");
        save_path(&path, &Config::default()).expect("overwrite");

        let entries: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("teamagent.json")]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        }
    }

    #[test]
    fn config_path_env_override() {
        // Only this test touches TEAMAGENT_CONFIG; every other test uses
        // the *_path variants, so no env race across the parallel runner.
        std::env::set_var(CONFIG_ENV, "/tmp/teamagent-override.json");
        let path = config_path().expect("path");
        std::env::remove_var(CONFIG_ENV);
        assert_eq!(path, PathBuf::from("/tmp/teamagent-override.json"));
    }
}
