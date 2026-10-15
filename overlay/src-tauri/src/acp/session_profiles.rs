//! Persistent Codeg-owned native profiles. These are data directories, not a
//! sandbox: agent tools still have their normal project permissions.
use super::error::AcpError;
use crate::models::agent::AgentType;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

static PROFILES: OnceLock<BTreeMap<String, String>> = OnceLock::new();

pub(crate) fn codex_home() -> PathBuf {
    PROFILES
        .get()
        .and_then(|p| p.get("CODEX_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::codeg_home_dir().join("agent-profiles/codex"))
}

// Ambient credentials belong to the launching user's other applications.
// Explicit Codeg settings may supply keys after these are removed at spawn.
pub(crate) const AMBIENT_CREDENTIAL_KEYS: &[&str] = &[
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CHATGPT_ACCESS_TOKEN",
    "DEFAULT_AUTH_REQUEST",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "DEVIN_API_KEY",
    "DEVIN_TOKEN",
];

fn is_alias(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

#[cfg(windows)]
fn reject_windows_hard_link(path: &Path) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let file = std::fs::File::open(path)?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // The file owns a live handle, and the API initializes info on success.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { info.assume_init() }.nNumberOfLinks != 1 {
        return Err(io::Error::other("Credential/config file must not be hard linked"));
    }
    Ok(())
}

fn private_file(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        if std::fs::symlink_metadata(ancestor).is_ok_and(|m| is_alias(&m)) {
            return Err(io::Error::other(
                "Credential/config file must be a regular private file without symlink ancestors",
            ));
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_file() || is_alias(&meta) {
                return Err(io::Error::other(
                    "Credential/config file must be a regular private file",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(io::Error::other(
                        "Credential/config file must not be hard linked",
                    ));
                }
            }
            #[cfg(windows)]
            reject_windows_hard_link(path)?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn reject_shared_google_refresh(owned: &Path, personal: &Path) -> io::Result<()> {
    private_file(owned)?;
    let read = |p: &Path| -> io::Result<Option<serde_json::Value>> {
        match std::fs::read(p) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map(Some)
                .map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    };
    if let (Some(a), Some(b)) = (read(owned)?, read(personal)?) {
        if a.get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .is_some_and(|token| b.get("refresh_token").and_then(|v| v.as_str()) == Some(token))
        {
            return Err(io::Error::other("Google credentials share the personal refresh token; sign in independently in Codeg"));
        }
    }
    Ok(())
}

fn jwt_session(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("session_id")
        .or_else(|| value.get("sid"))?
        .as_str()
        .map(str::to_owned)
}

/// Local comparison only: never refresh, revoke, import or log personal secrets.
pub(crate) fn reject_shared_codex_auth(raw: &str, personal_path: &Path) -> io::Result<()> {
    let candidate: serde_json::Value = serde_json::from_str(raw).map_err(io::Error::other)?;
    let personal = match std::fs::read_to_string(personal_path) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for key in ["access_token", "refresh_token", "id_token"] {
        let a = candidate
            .get("tokens")
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str());
        let b = personal
            .get("tokens")
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str());
        if let (Some(a), Some(b)) = (a, b) {
            if !a.is_empty()
                && (a == b || jwt_session(a).is_some_and(|sid| Some(sid) == jwt_session(b)))
            {
                return Err(io::Error::other("Codex credentials share the personal login session; sign in independently in Codeg"));
            }
        }
    }
    if candidate
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .is_some_and(|key| personal.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(key))
    {
        return Err(io::Error::other(
            "Codex API key is shared with the personal profile",
        ));
    }
    Ok(())
}

pub(crate) fn validate_codex_files(home: &Path) -> io::Result<()> {
    private_file(&home.join("config.toml"))?;
    private_file(&home.join("auth.json"))
}

pub(crate) fn enforce_codex_auth_storage(home: &Path) -> io::Result<()> {
    let config = home.join("config.toml");
    let auth = home.join("auth.json");
    validate_codex_files(home)?;
    match std::fs::read_to_string(&auth) {
        Ok(raw) => {
            let personal = dirs::home_dir()
                .ok_or_else(|| io::Error::other("No user home"))?
                .join(".codex/auth.json");
            reject_shared_codex_auth(&raw, &personal)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let raw = match std::fs::read_to_string(&config) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc = raw
        .parse::<toml_edit::Document>()
        .map_err(io::Error::other)?;
    if doc
        .get("cli_auth_credentials_store")
        .and_then(|v| v.as_str())
        != Some("file")
    {
        doc["cli_auth_credentials_store"] = toml_edit::value("file");
        std::fs::write(&config, doc.to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [&config, &auth] {
            if file.exists() {
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    Ok(())
}

fn prepare_dir(path: &Path, personal: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::other("Agent profile must be an absolute path"));
    }
    // Reject aliases before chmod or any credential/config writes.
    let mut ancestor = Some(path);
    while let Some(item) = ancestor {
        if let Ok(meta) = std::fs::symlink_metadata(item) {
            if is_alias(&meta) {
                return Err(io::Error::other("Agent profile must not contain symlinks"));
            }
        }
        ancestor = item.parent();
    }
    if path.starts_with(personal) || personal.starts_with(path) {
        return Err(io::Error::other(
            "Agent profile overlaps the personal profile",
        ));
    }
    std::fs::create_dir_all(path)?;
    let resolved = path.canonicalize()?;
    let personal = personal
        .canonicalize()
        .unwrap_or_else(|_| personal.to_path_buf());
    if resolved.starts_with(&personal) || personal.starts_with(&resolved) {
        return Err(io::Error::other(
            "Agent profile overlaps the personal profile",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&resolved, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(resolved)
}

/// Must run in the single-threaded entry point, before logging/runtime threads.
/// Ambient vendor HOME variables are deliberately not trusted. Only Codeg's
/// explicit migration override may reuse an existing isolated Codex profile.
pub fn initialize() -> io::Result<()> {
    // LaunchServices replaces TMPDIR on macOS even when LSEnvironment sets
    // it. An explicit application override is applied before worker threads.
    if let Some(value) = std::env::var_os("CODEG_TMP_DIR").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() || !path.is_dir() {
            return Err(io::Error::other(
                "CODEG_TMP_DIR must be an existing absolute directory",
            ));
        }
        let path = path.canonicalize()?;
        for key in ["TMPDIR", "TMP", "TEMP"] {
            std::env::set_var(key, &path);
        }
    }
    let home = dirs::home_dir().ok_or_else(|| io::Error::other("No user home"))?;
    let root = std::env::var_os("CODEG_DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(crate::paths::codeg_home_dir)
        .join("agent-profiles");
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    let mut profiles = BTreeMap::new();
    for (key, leaf, personal) in [
        ("CODEX_HOME", "codex", ".codex"),
        ("CLAUDE_CONFIG_DIR", "claude", ".claude"),
        ("GEMINI_CLI_HOME", "gemini", ".gemini"),
        ("GEMINI_HOME", "antigravity", ".gemini"),
        ("DSH_HOME", "deepseek", ".dsh"),
    ] {
        let path = if key == "CODEX_HOME" {
            std::env::var_os("CODEG_CODEX_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(leaf))
        } else {
            root.join(leaf)
        };
        let resolved = prepare_dir(&path, &home.join(personal))?;
        profiles.insert(key.to_string(), resolved.to_string_lossy().into_owned());
    }
    let devin = prepare_dir(&root.join("devin-data"), &home.join(".local/share/devin"))?;
    profiles.insert(
        "CODEG_DEVIN_DATA_HOME".into(),
        devin.to_string_lossy().into_owned(),
    );
    // Publish only after every directory passed validation. No personal
    // session/config database is imported, copied, or linked.
    for (key, value) in &profiles {
        std::env::set_var(key, value);
    }
    PROFILES
        .set(profiles)
        .map_err(|_| io::Error::other("Profiles already initialized"))?;
    Ok(())
}

/// Applied after settings/provider overlays, and again at the actual spawn
/// boundary, so web, desktop, handoffs and internal launches share the rule.
pub(crate) fn apply(agent: AgentType, env: &mut BTreeMap<String, String>) -> Result<(), AcpError> {
    let profiles = PROFILES
        .get()
        .ok_or_else(|| AcpError::protocol("Agent profiles were not initialized"))?;
    apply_with_profiles(agent, env, profiles)
}

pub(crate) fn ensure_supported(agent: AgentType) -> Result<(), AcpError> {
    profile_key(agent).map(|_| ())
}

fn profile_key(agent: AgentType) -> Result<&'static str, AcpError> {
    Ok(match agent {
        AgentType::Codex => "CODEX_HOME",
        AgentType::ClaudeCode => "CLAUDE_CONFIG_DIR",
        AgentType::Gemini => "GEMINI_CLI_HOME",
        AgentType::Antigravity => "GEMINI_HOME",
        AgentType::DeepSeek => "DSH_HOME",
        a if a.custom_id() == Some("devin") => "CODEG_DEVIN_DATA_HOME",
        _ => return Err(AcpError::protocol(format!(
            "Session isolation is not yet verified for {agent}; launch refused to protect personal agent history"
        ))),
    })
}

/// deepseek-acp picks its route only from `DEEPSEEK_ACP_PROVIDER` /
/// `DEEPSEEK_ACP_MODEL` and otherwise falls back to `deepseek-official`,
/// ignoring the profile's `agent-default-model`. Bridge that setting so a
/// route declared in `settings.yaml` is the one a new session starts on.
/// Explicit env values win; an unreadable or partial setting changes nothing.
fn apply_dsh_default_model(home: &Path, env: &mut BTreeMap<String, String>) {
    let set = |k: &str| env.get(k).is_some_and(|v| !v.trim().is_empty());
    if set("DEEPSEEK_ACP_PROVIDER") || set("DEEPSEEK_ACP_MODEL") {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(home.join("settings.yaml")) else {
        return;
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&raw) else {
        return;
    };
    let pick = |k: &str| {
        doc.get("agent-default-model")?
            .get(k)?
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    if let (Some(provider), Some(model)) = (pick("provider"), pick("model")) {
        env.insert("DEEPSEEK_ACP_PROVIDER".into(), provider);
        env.insert("DEEPSEEK_ACP_MODEL".into(), model);
    }
}

fn apply_with_profiles(
    agent: AgentType,
    env: &mut BTreeMap<String, String>,
    profiles: &BTreeMap<String, String>,
) -> Result<(), AcpError> {
    let key = profile_key(agent)?;
    let value = profiles.get(key).expect("initialized profile key");
    // Refuse a replaced/missing profile instead of silently falling back.
    let path = Path::new(value);
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| AcpError::protocol(format!("Agent profile unavailable: {e}")))?;
    if !meta.is_dir()
        || is_alias(&meta)
        || path.canonicalize().ok().as_deref() != Some(path)
    {
        return Err(AcpError::protocol(
            "Agent profile changed; restart Codeg after repairing its directory",
        ));
    }
    let child_key = if key == "CODEG_DEVIN_DATA_HOME" {
        "XDG_DATA_HOME"
    } else {
        key
    };
    env.insert(child_key.into(), value.clone());
    if agent == AgentType::Codex {
        enforce_codex_auth_storage(path).map_err(|e| AcpError::protocol(e.to_string()))?;
        let mut config = env
            .get("CODEX_CONFIG")
            .filter(|v| !v.is_empty())
            .map(|raw| serde_json::from_str::<serde_json::Value>(raw))
            .transpose()
            .map_err(|_| AcpError::protocol("Invalid CODEX_CONFIG"))?
            .unwrap_or_else(|| serde_json::json!({}));
        let object = config
            .as_object_mut()
            .ok_or_else(|| AcpError::protocol("CODEX_CONFIG must be an object"))?;
        object.insert(
            "cli_auth_credentials_store".into(),
            serde_json::json!("file"),
        );
        env.insert("CODEX_CONFIG".into(), config.to_string());
    }
    if agent == AgentType::DeepSeek {
        apply_dsh_default_model(path, env);
    }
    if agent == AgentType::Antigravity {
        // The vendor defaults to the global gemini/antigravity-acp Keychain
        // entry even with GEMINI_HOME set. Disable both reads and logout writes.
        env.insert("AGY_ACP_FORCE_FILE_STORAGE".into(), "1".into());
        let personal = dirs::home_dir()
            .ok_or_else(|| AcpError::protocol("No user home"))?
            .join(".gemini");
        for file in [
            "antigravity-acp/acp_token.json",
            "antigravity-acp/acp_business_token.json",
        ] {
            reject_shared_google_refresh(&path.join(file), &personal.join("oauth_creds.json"))
                .map_err(|e| AcpError::protocol(e.to_string()))?;
        }
    }
    if agent == AgentType::Gemini {
        env.insert("GEMINI_FORCE_FILE_STORAGE".into(), "true".into());
        let personal = dirs::home_dir()
            .ok_or_else(|| AcpError::protocol("No user home"))?
            .join(".gemini/oauth_creds.json");
        reject_shared_google_refresh(&path.join("oauth_creds.json"), &personal)
            .map_err(|e| AcpError::protocol(e.to_string()))?;
    }
    if agent == AgentType::ClaudeCode
        && ![
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ]
        .iter()
        .any(|key| env.get(*key).is_some_and(|v| !v.trim().is_empty()))
    {
        return Err(AcpError::protocol(
            "Claude system Keychain isolation is not verified; configure a dedicated Codeg credential before connecting",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_overrides_personal_settings_and_fails_if_profile_disappears() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().canonicalize().unwrap().join("codex");
        std::fs::create_dir(&path).unwrap();
        let profiles = BTreeMap::from([("CODEX_HOME".into(), path.to_string_lossy().into_owned())]);
        let mut env = BTreeMap::from([
            ("CODEX_HOME".into(), "/personal/.codex".into()),
            ("MODEL".into(), "kept".into()),
        ]);
        apply_with_profiles(AgentType::Codex, &mut env, &profiles).unwrap();
        assert_eq!(env["CODEX_HOME"], path.to_string_lossy());
        assert_eq!(env["MODEL"], "kept");
        std::fs::remove_dir_all(path).unwrap();
        assert!(apply_with_profiles(AgentType::Codex, &mut env, &profiles).is_err());
    }
    #[test]
    fn supported_agents_pin_profiles_and_auth_storage() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().canonicalize().unwrap();
        let devin: AgentType = serde_json::from_str("\"custom:devin\"").unwrap();
        for (agent, key, child_key) in [
            (AgentType::Codex, "CODEX_HOME", "CODEX_HOME"),
            (
                AgentType::ClaudeCode,
                "CLAUDE_CONFIG_DIR",
                "CLAUDE_CONFIG_DIR",
            ),
            (AgentType::Gemini, "GEMINI_CLI_HOME", "GEMINI_CLI_HOME"),
            (AgentType::Antigravity, "GEMINI_HOME", "GEMINI_HOME"),
            (devin, "CODEG_DEVIN_DATA_HOME", "XDG_DATA_HOME"),
        ] {
            let profiles = BTreeMap::from([(key.into(), path.to_string_lossy().into_owned())]);
            let mut env = BTreeMap::from([(child_key.into(), "personal".into())]);
            if agent == AgentType::ClaudeCode {
                env.insert("ANTHROPIC_API_KEY".into(), "dedicated-test-key".into());
            }
            apply_with_profiles(agent, &mut env, &profiles).unwrap();
            assert_eq!(env[child_key], path.to_string_lossy());
            if agent == AgentType::Antigravity {
                assert_eq!(env["AGY_ACP_FORCE_FILE_STORAGE"], "1");
            } else if agent == AgentType::Gemini {
                assert_eq!(env["GEMINI_FORCE_FILE_STORAGE"], "true");
            }
        }
    }
    #[test]
    fn dsh_starts_on_the_profile_default_route() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().canonicalize().unwrap();
        let profiles = BTreeMap::from([("DSH_HOME".into(), path.to_string_lossy().into_owned())]);
        let launch = |env: &mut BTreeMap<String, String>| {
            apply_with_profiles(AgentType::DeepSeek, env, &profiles).unwrap();
        };
        // No settings: the adapter keeps its own default.
        let mut env = BTreeMap::new();
        launch(&mut env);
        assert!(!env.contains_key("DEEPSEEK_ACP_PROVIDER"));

        std::fs::write(
            path.join("settings.yaml"),
            "agent-default-model:\n  provider: my-route\n  model: flash-auto\n",
        )
        .unwrap();
        let mut env = BTreeMap::new();
        launch(&mut env);
        assert_eq!(env["DSH_HOME"], path.to_string_lossy());
        assert_eq!(env["DEEPSEEK_ACP_PROVIDER"], "my-route");
        assert_eq!(env["DEEPSEEK_ACP_MODEL"], "flash-auto");

        // An explicit choice in the agent's env wins over the profile.
        let mut env = BTreeMap::from([("DEEPSEEK_ACP_MODEL".into(), "chosen".into())]);
        launch(&mut env);
        assert_eq!(env["DEEPSEEK_ACP_MODEL"], "chosen");
        assert!(!env.contains_key("DEEPSEEK_ACP_PROVIDER"));

        // A half-written setting is ignored rather than guessed at.
        std::fs::write(path.join("settings.yaml"), "agent-default-model:\n  provider: x\n").unwrap();
        let mut env = BTreeMap::new();
        launch(&mut env);
        assert!(!env.contains_key("DEEPSEEK_ACP_PROVIDER"));
    }
    #[test]
    fn unverified_agents_fail_closed() {
        assert!(
            apply_with_profiles(AgentType::OpenClaw, &mut BTreeMap::new(), &BTreeMap::new())
                .is_err()
        );
    }
    #[test]
    fn persistent_private_directory_preserves_sessions() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let p = base.join("service/codex");
        let personal = base.join("personal/.codex");
        let first = prepare_dir(&p, &personal).unwrap();
        std::fs::write(first.join("session-marker"), "keep").unwrap();
        assert_eq!(first, prepare_dir(&p, &personal).unwrap());
        assert_eq!(
            std::fs::read_to_string(first.join("session-marker")).unwrap(),
            "keep"
        );
        assert!(!personal.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
    #[test]
    fn rejects_personal_and_relative_paths() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let personal = base.join(".codex");
        for p in [
            personal.clone(),
            personal.join("child"),
            base.clone(),
            PathBuf::from("relative"),
        ] {
            assert!(prepare_dir(&p, &personal).is_err());
        }
    }
    #[test]
    fn cannot_fall_back_when_path_is_a_file() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let p = base.join("file");
        std::fs::write(&p, "untouched").unwrap();
        assert!(prepare_dir(&p, &base.join("personal")).is_err());
        assert_eq!(std::fs::read_to_string(p).unwrap(), "untouched");
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_to_personal_before_writing() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let personal = base.join("personal");
        std::fs::create_dir(&personal).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&personal, &alias).unwrap();
        assert!(prepare_dir(&alias.join("new"), &personal).is_err());
        assert!(!personal.join("new").exists());
    }

    #[test]
    fn rejects_rotated_tokens_from_same_session_and_accepts_independent_login() {
        use base64::Engine;
        let t = tempfile::tempdir().unwrap();
        let personal = t.path().join("auth.json");
        let token = |sid: &str, generation: u8| {
            format!(
                "e30.{}.sig{generation}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::json!({"session_id":sid}).to_string())
            )
        };
        let auth = |sid: &str, generation: u8| {
            serde_json::json!({"tokens":{
            "access_token":token(sid, generation), "refresh_token":format!("refresh-{sid}-{generation}")
        }}).to_string()
        };
        std::fs::write(&personal, auth("personal", 2)).unwrap();
        let before = std::fs::read(&personal).unwrap();
        assert!(reject_shared_codex_auth(&auth("personal", 1), &personal).is_err());
        assert!(reject_shared_codex_auth(&auth("personal", 2), &personal).is_err());
        assert!(reject_shared_codex_auth(&auth("independent", 1), &personal).is_ok());
        assert_eq!(std::fs::read(&personal).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_linked_auth_and_config_without_touching_target() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("personal");
        std::fs::write(&target, "unchanged").unwrap();
        let home = t.path().join("owned");
        std::fs::create_dir(&home).unwrap();
        std::os::unix::fs::symlink(&target, home.join("config.toml")).unwrap();
        assert!(enforce_codex_auth_storage(&home).is_err());
        std::fs::remove_file(home.join("config.toml")).unwrap();
        std::fs::hard_link(&target, home.join("auth.json")).unwrap();
        assert!(enforce_codex_auth_storage(&home).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
    }

    #[test]
    fn forces_file_storage_preserving_other_config_and_never_imports_auth() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join("config.toml"),
            "# keep comment\nmodel = 'test'\ncli_auth_credentials_store = 'keyring'\n",
        )
        .unwrap();
        enforce_codex_auth_storage(t.path()).unwrap();
        let raw = std::fs::read_to_string(t.path().join("config.toml")).unwrap();
        assert!(raw.contains("# keep comment"));
        let doc = raw.parse::<toml::Table>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("test"));
        assert_eq!(doc["cli_auth_credentials_store"].as_str(), Some("file"));
        assert!(!t.path().join("auth.json").exists());
    }

    #[test]
    fn rejects_hard_linked_credentials_before_writing() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("personal.json");
        std::fs::write(&target, "unchanged").unwrap();
        let owned = t.path().join("owned");
        std::fs::create_dir(&owned).unwrap();
        std::fs::hard_link(&target, owned.join("auth.json")).unwrap();
        assert!(validate_codex_files(&owned).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "unchanged");
        assert!(!owned.join("config.toml").exists());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_junction_ancestors_before_creating_profile() {
        let t = tempfile::tempdir().unwrap();
        let personal = t.path().join("personal");
        std::fs::create_dir(&personal).unwrap();
        let alias = t.path().join("alias");
        junction::create(&personal, &alias).unwrap();
        assert!(prepare_dir(&alias.join("new"), &personal).is_err());
        assert!(private_file(&alias.join("auth.json")).is_err());
        assert!(!personal.join("new").exists());
    }

    #[test]
    fn google_copied_refresh_is_refused_and_fresh_login_is_allowed() {
        let t = tempfile::tempdir().unwrap();
        let personal = t.path().join("personal.json");
        let owned = t.path().join("owned.json");
        std::fs::write(&personal, r#"{"refresh_token":"personal"}"#).unwrap();
        std::fs::write(&owned, r#"{"refresh_token":"personal"}"#).unwrap();
        assert!(reject_shared_google_refresh(&owned, &personal).is_err());
        std::fs::write(&owned, r#"{"refresh_token":"independent"}"#).unwrap();
        assert!(reject_shared_google_refresh(&owned, &personal).is_ok());
        assert_eq!(
            std::fs::read_to_string(personal).unwrap(),
            r#"{"refresh_token":"personal"}"#
        );
    }

    #[test]
    fn claude_without_dedicated_credentials_cannot_reuse_system_login() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().canonicalize().unwrap();
        let profiles = BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            path.to_string_lossy().into_owned(),
        )]);
        let mut env = BTreeMap::new();
        assert!(apply_with_profiles(AgentType::ClaudeCode, &mut env, &profiles).is_err());
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), "dedicated-token".into());
        assert!(apply_with_profiles(AgentType::ClaudeCode, &mut env, &profiles).is_ok());
    }
}
