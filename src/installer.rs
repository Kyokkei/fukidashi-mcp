//! Cross-client installation and rollback for the Fukidashi MCP server.
//!
//! The installer owns one native executable and writes only the Fukidashi MCP
//! entry in each client's user configuration.  Project-local configuration is
//! deliberately never discovered or modified.

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::error::{FukidashiError, Result};

const PRODUCT: &str = "Fukidashi";
const MANIFEST_VERSION: u32 = 1;
const MANIFEST_NAME: &str = "install-manifest.json";
const DATA_DIR_ENV: &str = "FUKIDASHI_DATA_DIR";
const MANAGED_START: &str = "<!-- fukidashi:begin -->";
const MANAGED_END: &str = "<!-- fukidashi:end -->";

/// Supported user-level MCP clients.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize, ValueEnum,
)]
#[value(rename_all = "kebab-case")]
pub enum Client {
    Codex,
    Claude,
    Antigravity,
    Gemini,
    Cursor,
    VsCode,
}

impl Client {
    pub fn all() -> &'static [Client] {
        &[
            Client::Codex,
            Client::Claude,
            Client::Antigravity,
            Client::Gemini,
            Client::Cursor,
            Client::VsCode,
        ]
    }

    fn display_name(self) -> &'static str {
        match self {
            Client::Codex => "Codex CLI/Desktop/IDE",
            Client::Claude => "Claude Code",
            Client::Antigravity => "Antigravity",
            Client::Gemini => "Gemini CLI",
            Client::Cursor => "Cursor",
            Client::VsCode => "VS Code/Copilot",
        }
    }

    fn config_kind(self) -> ConfigKind {
        match self {
            Client::Codex => ConfigKind::Toml,
            _ => ConfigKind::Json,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ConfigKind {
    Json,
    Toml,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStatus {
    pub client: Client,
    pub name: String,
    pub detected: bool,
    pub config_path: PathBuf,
    pub configured: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub data_root: PathBuf,
    pub executable: PathBuf,
    pub clients: Vec<ClientStatus>,
    pub context_path: PathBuf,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct MutationReport {
    pub data_root: PathBuf,
    pub changed: Vec<PathBuf>,
    pub skipped: Vec<String>,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallationStatus {
    pub data_root: PathBuf,
    pub executable: PathBuf,
    pub installed: bool,
    pub manifest_path: PathBuf,
    pub clients: Vec<ClientStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    executable: PathBuf,
    executable_sha256: String,
    #[serde(default)]
    executable_backup_path: Option<PathBuf>,
    context_path: PathBuf,
    clients: Vec<ManifestClient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestClient {
    client: Client,
    config_path: PathBuf,
    backup_path: Option<PathBuf>,
    after_sha256: String,
    skill_path: Option<PathBuf>,
    /// Whether Fukidashi inserted the MCP entry. A preexisting identical
    /// entry is observed but remains user-owned.
    #[serde(default = "default_owned")]
    owned: bool,
}

fn default_owned() -> bool {
    true
}

#[derive(Debug, Clone)]
struct InstallPaths {
    data_root: PathBuf,
    executable: PathBuf,
    context: PathBuf,
    manifest: PathBuf,
    backup_dir: PathBuf,
}

/// Install the native executable and configure selected clients.
///
/// With an empty client list, only clients that appear installed are selected.
/// `all` configures every supported client, creating stable user config paths
/// where necessary.
pub fn install(requested: &[Client], all: bool) -> Result<InstallReport> {
    let paths = install_paths()?;
    fs::create_dir_all(paths.executable.parent().expect("executable has parent"))?;
    fs::create_dir_all(&paths.backup_dir)?;
    let previous_executable = fs::read(&paths.executable).ok();
    let previous_context = fs::read(&paths.context).ok();
    let executable_backup_path = if let Some(bytes) = previous_executable.as_ref() {
        let path = paths.backup_dir.join("previous-fukidashi-mcp.bin");
        if !path.is_file() {
            write_atomic(&path, bytes)?;
        }
        Some(path)
    } else {
        None
    };
    install_executable(&paths.executable)?;
    write_atomic(&paths.context, workflow_context().as_bytes())?;

    let clients = select_clients(requested, all);
    let previous_manifest = load_manifest(&paths.manifest)?;
    let mut installed = Vec::new();
    let mut changed_paths = Vec::new();
    let mut skill_paths = Vec::new();
    let mut skill_journal: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut journal: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();

    let result = (|| {
        for client in clients {
            let config_path = config_path(client)?;
            let before = fs::read(&config_path).ok();
            let previous_entry = previous_manifest.as_ref().and_then(|manifest| {
                manifest
                    .clients
                    .iter()
                    .find(|entry| entry.client == client && entry.config_path == config_path)
            });
            let before_bytes = before.as_deref().unwrap_or(b"");
            let previously_owned = previous_entry.is_some()
                && previous_manifest.as_ref().is_some_and(|manifest| {
                    config_contains(client, before_bytes, &manifest.executable)
                });
            if entry_present(client, before_bytes)
                && !config_contains(client, before_bytes, &paths.executable)
                && !previously_owned
            {
                return Err(FukidashiError::InvalidInput(format!(
                    "{} already contains a different fukidashi entry; remove it or run rollback before reinstalling",
                    config_path.display()
                )));
            }
            let patched = patch_config(client, before_bytes, &paths.executable)?;
            if before.as_deref() != Some(patched.as_slice()) {
                if let Some(bytes) = before.as_ref() {
                    let preserve_backup = previous_entry
                        .filter(|entry| digest(bytes) == entry.after_sha256)
                        .and_then(|entry| entry.backup_path.clone())
                        .filter(|path| path.is_file());
                    let backup = preserve_backup
                        .unwrap_or_else(|| paths.backup_dir.join(backup_name(&config_path)));
                    if !backup.is_file() {
                        write_atomic(&backup, bytes)?;
                    }
                    journal.push((config_path.clone(), Some(bytes.clone())));
                    write_atomic(&config_path, &patched)?;
                    changed_paths.push(config_path.clone());
                    let owned = previous_entry
                        .map(|entry| entry.owned)
                        .unwrap_or(!config_contains(client, before_bytes, &paths.executable));
                    installed.push(ManifestClient {
                        client,
                        config_path: config_path.clone(),
                        backup_path: Some(backup),
                        after_sha256: digest(&patched),
                        skill_path: previous_entry.and_then(|entry| entry.skill_path.clone()),
                        owned,
                    });
                } else {
                    journal.push((config_path.clone(), None));
                    write_atomic(&config_path, &patched)?;
                    changed_paths.push(config_path.clone());
                    installed.push(ManifestClient {
                        client,
                        config_path: config_path.clone(),
                        backup_path: None,
                        after_sha256: digest(&patched),
                        skill_path: previous_entry.and_then(|entry| entry.skill_path.clone()),
                        owned: true,
                    });
                }
            } else {
                // An already-managed config still belongs in the manifest so
                // uninstall can remove the entry after a repeated install.
                let (backup_path, owned) = unchanged_manifest_state(
                    before.as_deref(),
                    previous_entry,
                    entry_present(client, before_bytes),
                    config_contains(client, before_bytes, &paths.executable),
                    &paths.backup_dir,
                    &config_path,
                );
                if let (Some(backup), Some(bytes)) = (backup_path.as_ref(), before.as_ref())
                    && !backup.is_file()
                {
                    write_atomic(backup, bytes)?;
                }
                installed.push(ManifestClient {
                    client,
                    config_path: config_path.clone(),
                    backup_path,
                    after_sha256: digest(&patched),
                    skill_path: previous_entry.and_then(|entry| entry.skill_path.clone()),
                    owned,
                });
            }

            if let Some(skill) = supported_skill_path(client)? {
                if !should_write_skill(fs::read_to_string(&skill).ok().as_deref()) {
                    // A user-owned skill with the same name is left intact;
                    // the MCP entry remains sufficient for operation.
                    continue;
                }
                skill_journal.push((skill.clone(), fs::read(&skill).ok()));
                write_atomic(&skill, skill_contents(client).as_bytes())?;
                skill_paths.push((client, skill));
            }
        }
        for entry in &mut installed {
            entry.skill_path = skill_paths
                .iter()
                .find(|(client, _)| *client == entry.client)
                .map(|(_, path)| path.clone());
        }
        // A targeted reinstall must retain ownership records for adapters
        // installed by an earlier `--all`/auto-detect run.
        if let Some(previous) = previous_manifest.as_ref() {
            for entry in &previous.clients {
                if !installed
                    .iter()
                    .any(|current| current.client == entry.client)
                {
                    installed.push(entry.clone());
                }
            }
        }
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            executable: paths.executable.clone(),
            executable_sha256: digest_file(&paths.executable)?,
            executable_backup_path,
            context_path: paths.context.clone(),
            clients: installed.clone(),
        };
        write_atomic(
            &paths.manifest,
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        Ok::<(), FukidashiError>(())
    })();

    if let Err(error) = result {
        for (path, previous) in skill_journal.into_iter().rev() {
            match previous {
                Some(bytes) => {
                    let _ = write_atomic(&path, &bytes);
                }
                None => {
                    let _ = fs::remove_file(path);
                }
            }
        }
        for (path, previous) in journal.into_iter().rev() {
            match previous {
                Some(bytes) => {
                    let _ = write_atomic(&path, &bytes);
                }
                None => {
                    let _ = fs::remove_file(path);
                }
            }
        }
        match previous_context {
            Some(bytes) => {
                let _ = write_atomic(&paths.context, &bytes);
            }
            None => {
                let _ = fs::remove_file(&paths.context);
            }
        }
        match previous_executable {
            Some(bytes) => {
                let _ = write_atomic(&paths.executable, &bytes);
            }
            None => {
                let _ = fs::remove_file(&paths.executable);
            }
        }
        return Err(error);
    }

    let statuses = Client::all()
        .iter()
        .copied()
        .map(|client| client_status(client, &paths.executable))
        .collect::<Result<Vec<_>>>()?;
    Ok(InstallReport {
        data_root: paths.data_root,
        executable: paths.executable,
        clients: statuses,
        context_path: paths.context,
        manifest_path: paths.manifest,
    })
}

/// Remove Fukidashi entries and owned files while preserving unrelated client data.
pub fn uninstall() -> Result<MutationReport> {
    let paths = install_paths()?;
    let manifest = load_manifest(&paths.manifest)?;
    let Some(manifest) = manifest else {
        return Ok(MutationReport {
            data_root: paths.data_root,
            changed: Vec::new(),
            skipped: vec!["no Fukidashi install manifest found".to_owned()],
            manifest_path: paths.manifest,
        });
    };
    let mut changed = Vec::new();
    let mut skipped = Vec::new();
    for entry in &manifest.clients {
        if let Ok(current) = fs::read(&entry.config_path) {
            if !entry.owned {
                skipped.push(format!(
                    "{}: preexisting fukidashi entry is user-owned; left in place",
                    entry.config_path.display()
                ));
            } else if !config_contains(entry.client, &current, &manifest.executable) {
                skipped.push(format!(
                    "{}: fukidashi entry no longer points to installed executable; left in place",
                    entry.config_path.display()
                ));
            } else {
                match remove_entry(entry.client, &current) {
                    Ok(updated) if updated != current => {
                        write_atomic(&entry.config_path, &updated)?;
                        changed.push(entry.config_path.clone());
                    }
                    Ok(_) => {}
                    Err(error) => skipped.push(format!("{}: {error}", entry.config_path.display())),
                }
            }
        }
        if let Some(skill) = entry.skill_path.as_deref() {
            remove_owned_skill(skill, &mut changed, &mut skipped);
        }
    }
    remove_owned_file(&manifest.context_path, &mut changed, &mut skipped);
    if digest_file(&manifest.executable).ok().as_deref()
        == Some(manifest.executable_sha256.as_str())
    {
        remove_owned_file(&manifest.executable, &mut changed, &mut skipped);
    } else {
        skipped.push(format!(
            "{}: executable changed after installation; left in place",
            manifest.executable.display()
        ));
    }
    // Keep backups for explicit rollback and recovery; only the active
    // manifest is removed after a successful uninstall.
    let _ = fs::remove_file(&paths.manifest);
    Ok(MutationReport {
        data_root: paths.data_root,
        changed,
        skipped,
        manifest_path: paths.manifest,
    })
}

/// Restore configurations from the last install, but refuse to overwrite a
/// file that changed since installation. This makes rollback safe after a
/// user edits a client configuration.
pub fn rollback() -> Result<MutationReport> {
    let paths = install_paths()?;
    let manifest = load_manifest(&paths.manifest)?;
    let Some(manifest) = manifest else {
        return Ok(MutationReport {
            data_root: paths.data_root,
            changed: Vec::new(),
            skipped: vec!["no Fukidashi install manifest found".to_owned()],
            manifest_path: paths.manifest,
        });
    };
    let mut changed = Vec::new();
    let mut skipped = Vec::new();
    for entry in &manifest.clients {
        if !entry.owned && entry.backup_path.is_none() {
            skipped.push(format!(
                "{}: preexisting fukidashi entry is user-owned; left in place",
                entry.config_path.display()
            ));
            continue;
        }
        let current_hash = digest_file(&entry.config_path).ok();
        if current_hash.as_deref() != Some(entry.after_sha256.as_str()) {
            skipped.push(format!(
                "{}: changed since installation; backup not restored",
                entry.config_path.display()
            ));
            continue;
        }
        if let Some(backup) = entry.backup_path.as_deref() {
            let bytes = fs::read(backup)?;
            write_atomic(&entry.config_path, &bytes)?;
            changed.push(entry.config_path.clone());
        } else {
            fs::remove_file(&entry.config_path)?;
            changed.push(entry.config_path.clone());
            skipped.push(format!(
                "{}: no pre-install file existed; removed the managed file",
                entry.config_path.display()
            ));
        }
    }
    if let Some(backup) = manifest.executable_backup_path.as_deref() {
        if digest_file(&manifest.executable).ok().as_deref()
            == Some(manifest.executable_sha256.as_str())
        {
            let bytes = fs::read(backup)?;
            write_atomic(&manifest.executable, &bytes)?;
            changed.push(manifest.executable.clone());
        } else {
            skipped.push(format!(
                "{}: executable changed since installation; backup not restored",
                manifest.executable.display()
            ));
        }
    }
    Ok(MutationReport {
        data_root: paths.data_root,
        changed,
        skipped,
        manifest_path: paths.manifest,
    })
}

/// Return read-only status for all known clients.
pub fn status() -> Result<Vec<ClientStatus>> {
    let paths = install_paths()?;
    Client::all()
        .iter()
        .copied()
        .map(|client| client_status(client, &paths.executable))
        .collect()
}

pub fn installation_status() -> Result<InstallationStatus> {
    let paths = install_paths()?;
    Ok(InstallationStatus {
        data_root: paths.data_root,
        executable: paths.executable.clone(),
        installed: paths.executable.is_file() && paths.manifest.is_file(),
        manifest_path: paths.manifest,
        clients: status()?,
    })
}

pub fn doctor_lines() -> Result<Vec<String>> {
    let installation = installation_status()?;
    let mut lines = vec![format!(
        "Fukidashi engine: installed={}, executable={}, data_root={}",
        installation.installed,
        installation.executable.display(),
        installation.data_root.display()
    )];
    lines.extend(
        installation
            .clients
            .into_iter()
            .map(|status| {
                format!(
                    "{}: detected={}, configured={}, config={}",
                    status.name,
                    status.detected,
                    status.configured,
                    status.config_path.display()
                )
            })
            .collect::<Vec<_>>(),
    );
    Ok(lines)
}

fn install_paths() -> Result<InstallPaths> {
    let data_root = env::var_os(DATA_DIR_ENV)
        .map(PathBuf::from)
        .or_else(|| {
            if cfg!(windows) {
                env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join(PRODUCT))
            } else {
                env::var_os("XDG_DATA_HOME")
                    .map(|path| PathBuf::from(path).join(PRODUCT))
                    .or_else(|| dirs::data_dir().map(|path| path.join(PRODUCT)))
            }
        })
        .or_else(|| dirs::data_local_dir().map(|path| path.join(PRODUCT)))
        .ok_or_else(|| {
            FukidashiError::InvalidInput("cannot determine Fukidashi data directory".into())
        })?;
    let data_root = validate_data_root(data_root)?;
    let executable = data_root.join("bin").join(if cfg!(windows) {
        "fukidashi-mcp.exe"
    } else {
        "fukidashi-mcp"
    });
    Ok(InstallPaths {
        context: data_root.join("context").join("fukidashi-workflow.md"),
        manifest: data_root.join(MANIFEST_NAME),
        backup_dir: data_root.join("backups"),
        data_root,
        executable,
    })
}

fn validate_data_root(path: PathBuf) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(FukidashiError::InvalidInput(format!(
            "{DATA_DIR_ENV} must be an absolute path: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn install_executable(destination: &Path) -> Result<()> {
    let source = env::current_exe()?;
    if source.canonicalize().ok() == destination.canonicalize().ok() {
        return Ok(());
    }
    let parent = destination.parent().expect("destination has parent");
    let mut temp = NamedTempFile::new_in(parent)?;
    let mut input = fs::File::open(&source)?;
    std::io::copy(&mut input, &mut temp)?;
    temp.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let source_mode = fs::metadata(&source)?.permissions().mode();
        let mut permissions = temp.as_file().metadata()?.permissions();
        permissions.set_mode(source_mode | 0o111);
        temp.as_file().set_permissions(permissions)?;
    }
    temp.as_file().sync_all()?;
    replace_temp(temp, destination)
}

fn config_path(client: Client) -> Result<PathBuf> {
    config_path_for(client, &home_dir()?, &dirs::config_dir())
}

fn config_path_for(client: Client, home: &Path, config_dir: &Option<PathBuf>) -> Result<PathBuf> {
    Ok(match client {
        Client::Codex => home.join(".codex/config.toml"),
        Client::Claude => home.join(".claude.json"),
        Client::Antigravity => home.join(".gemini/config/mcp_config.json"),
        Client::Gemini => home.join(".gemini/settings.json"),
        Client::Cursor => home.join(".cursor/mcp.json"),
        Client::VsCode => config_dir
            .clone()
            .unwrap_or_else(|| home.join(".config"))
            .join("Code/User/mcp.json"),
    })
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .ok_or_else(|| FukidashiError::InvalidInput("cannot determine home directory".into()))
}

fn select_clients(requested: &[Client], all: bool) -> Vec<Client> {
    if all {
        return Client::all().to_vec();
    }
    if !requested.is_empty() {
        return requested.to_vec();
    }
    Client::all()
        .iter()
        .copied()
        .filter(|client| detected(*client))
        .collect()
}

fn detected(client: Client) -> bool {
    let Ok(home) = home_dir() else { return false };
    let config_dir = dirs::config_dir();
    let Ok(path) = config_path_for(client, &home, &config_dir) else {
        return false;
    };
    if path.exists()
        || match client {
            Client::Codex => home.join(".codex").exists() || command_exists("codex"),
            Client::Claude => home.join(".claude").exists() || command_exists("claude"),
            Client::Antigravity => {
                home.join(".gemini/antigravity").exists()
                    || command_exists("agy")
                    || command_exists("antigravity")
            }
            Client::Gemini => home.join(".gemini").exists() || command_exists("gemini"),
            Client::Cursor => home.join(".cursor").exists() || command_exists("cursor"),
            Client::VsCode => command_exists("code") || path.parent().is_some_and(Path::exists),
        }
    {
        return true;
    }
    false
}

fn command_exists(command: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let direct = dir.join(command);
        if direct.is_file() {
            return true;
        }
        if cfg!(windows) {
            [".exe", ".cmd", ".bat"]
                .iter()
                .any(|suffix| dir.join(format!("{command}{suffix}")).is_file())
        } else {
            false
        }
    })
}

fn client_status(client: Client, executable: &Path) -> Result<ClientStatus> {
    let home = home_dir()?;
    let config_dir = dirs::config_dir();
    let config_path = config_path_for(client, &home, &config_dir)?;
    let configured = fs::read(&config_path)
        .ok()
        .is_some_and(|bytes| config_contains(client, &bytes, executable));
    Ok(ClientStatus {
        client,
        name: client.display_name().to_owned(),
        detected: detected(client),
        config_path,
        configured,
    })
}

fn patch_config(client: Client, input: &[u8], executable: &Path) -> Result<Vec<u8>> {
    if matches!(client.config_kind(), ConfigKind::Toml) {
        patch_toml(input, executable)
    } else {
        patch_json(client, input, executable)
    }
}

fn patch_json(client: Client, input: &[u8], executable: &Path) -> Result<Vec<u8>> {
    let mut root: serde_json::Value = if input.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(input).map_err(|error| {
            FukidashiError::InvalidInput(format!("invalid client JSON: {error}"))
        })?
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| FukidashiError::InvalidInput("client JSON root must be an object".into()))?;
    let servers_key = json_servers_key(client);
    let servers = object
        .entry(servers_key)
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers.as_object_mut().ok_or_else(|| {
        FukidashiError::InvalidInput(format!("client {servers_key} must be an object"))
    })?;
    servers.insert("fukidashi".into(), server_json(client, executable));
    Ok(serde_json::to_vec_pretty(&root)?)
}

fn patch_toml(input: &[u8], executable: &Path) -> Result<Vec<u8>> {
    // Keep this small parser dependency-free. It replaces only a managed
    // `mcp_servers.fukidashi` table and preserves all other TOML text.
    let source = String::from_utf8_lossy(input);
    let mut lines = source.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut start = None;
    let mut end = lines.len();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "[mcp_servers.fukidashi]" || trimmed.starts_with("[mcp_servers.fukidashi.") {
            start.get_or_insert(index);
        } else if start.is_some() && trimmed.starts_with('[') {
            end = index;
            break;
        }
    }
    if let Some(start) = start {
        lines.drain(start..end);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    let command = toml_string(&executable.to_string_lossy());
    lines.extend([
        "[mcp_servers.fukidashi]".to_owned(),
        format!("command = {command}"),
        "args = []".to_owned(),
    ]);
    lines.push(String::new());
    Ok(lines.join("\n").into_bytes())
}

fn json_servers_key(client: Client) -> &'static str {
    if matches!(client, Client::VsCode) {
        "servers"
    } else {
        "mcpServers"
    }
}

fn server_json(client: Client, executable: &Path) -> serde_json::Value {
    let mut server = serde_json::json!({
        "command": executable,
        "args": []
    });
    if matches!(client, Client::VsCode) {
        server["type"] = serde_json::json!("stdio");
    }
    server
}

fn config_contains(client: Client, bytes: &[u8], executable: &Path) -> bool {
    if matches!(client.config_kind(), ConfigKind::Toml) {
        toml_entry_command(bytes).is_some_and(|command| command == executable.to_string_lossy())
    } else {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|value| {
                value
                    .get(json_servers_key(client))?
                    .get("fukidashi")?
                    .get("command")?
                    .as_str()
                    .map(str::to_owned)
            })
            .is_some_and(|command| command == executable.to_string_lossy())
    }
}

fn entry_present(client: Client, bytes: &[u8]) -> bool {
    if matches!(client.config_kind(), ConfigKind::Toml) {
        toml_entry_command(bytes).is_some()
            || String::from_utf8_lossy(bytes).lines().any(|line| {
                let section = line.trim();
                section == "[mcp_servers.fukidashi]"
                    || section.starts_with("[mcp_servers.fukidashi.")
            })
    } else {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .map(|value| {
                value
                    .get(json_servers_key(client))
                    .and_then(|servers| servers.get("fukidashi"))
                    .is_some()
            })
            .unwrap_or(false)
    }
}

fn toml_entry_command(bytes: &[u8]) -> Option<String> {
    let mut in_entry = false;
    for line in String::from_utf8_lossy(bytes).lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_entry = trimmed == "[mcp_servers.fukidashi]"
                || trimmed.starts_with("[mcp_servers.fukidashi.");
            continue;
        }
        if in_entry
            && let Some((key, value)) = trimmed.split_once('=')
            && key.trim() == "command"
        {
            let value = value.trim();
            return value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .map(|value| value.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
    }
    None
}

fn remove_entry(client: Client, bytes: &[u8]) -> Result<Vec<u8>> {
    if matches!(client.config_kind(), ConfigKind::Toml) {
        let source = String::from_utf8_lossy(bytes);
        let mut lines = source.lines().map(str::to_owned).collect::<Vec<_>>();
        let mut start = None;
        let mut end = lines.len();
        for (index, line) in lines.iter().enumerate() {
            if line.trim() == "[mcp_servers.fukidashi]"
                || line.trim().starts_with("[mcp_servers.fukidashi.")
            {
                start.get_or_insert(index);
            } else if start.is_some() && line.trim().starts_with('[') {
                end = index;
                break;
            }
        }
        if let Some(start) = start {
            lines.drain(start..end);
        }
        Ok(lines.join("\n").into_bytes())
    } else {
        let mut root: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
            FukidashiError::InvalidInput(format!("invalid client JSON: {error}"))
        })?;
        if let Some(servers) = root
            .get_mut(json_servers_key(client))
            .and_then(serde_json::Value::as_object_mut)
        {
            servers.remove("fukidashi");
            if servers.is_empty() {
                root.as_object_mut()
                    .expect("JSON root object")
                    .remove(json_servers_key(client));
            }
        }
        Ok(serde_json::to_vec_pretty(&root)?)
    }
}

fn supported_skill_path(client: Client) -> Result<Option<PathBuf>> {
    let home = home_dir()?;
    Ok(match client {
        Client::Codex => Some(home.join(".codex/skills/fukidashi-comic-translation/SKILL.md")),
        Client::Claude => Some(home.join(".claude/skills/fukidashi-comic-translation/SKILL.md")),
        _ => None,
    })
}

fn skill_contents(_client: Client) -> String {
    include_str!("../assets/fukidashi-comic-translation/SKILL.md").to_owned()
}

fn workflow_context() -> &'static str {
    "# Fukidashi workflow\n\nFukidashi is a local MCP server for comic processing. Prefer the strict-v1 two-call loop: fukidashi_translation_start -> fukidashi_translation_submit. The server owns page selection, analysis, clean, typeset, stage reuse, model release, and advancement. sfx_mode=preserve is the default: structurally unmatched text-* items are preserved and excluded from required translation, cleaning, and typesetting; preserve-mode covers and SFX-only pages receive an explicit pass-through clean/render stage; use sfx_mode=replace only explicitly. Never shell-read managed state, import/search for the Fukidashi package, invent artifact paths, or pass artifact paths to strict submit. For acquisition, use fukidashi_search_manga for native MangaDex titles, then pass its exact manga_id to fukidashi_pull_chapter with latest=true for a vague latest request. Latest selects the highest chapter value including external releases and never silently downgrades to an older hosted chapter; source language is metadata that Fukidashi auto-translates. An external or unavailable result reports that exact release with imported=false. Direct mode is first-class: pass one explicit http(s) URL and optional bounded job_name to fukidashi_pull_chapter; it requires an already provisioned gallery-dl helper, uses bounded transactional staging, and never guesses a URL. Imported jobs return the exact strict translation start step. After review-ready, call fukidashi_review_and_export with the exact returned job_id; it opens the editor and keeps one MCP call pending until review, returning fixes or exporting zip after approval. fukidashi_serve_editor and fukidashi_wait_for_review remain compatibility tools.\n"
}

fn load_manifest(path: &Path) -> Result<Option<Manifest>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|error| {
            FukidashiError::InvalidInput(format!("invalid install manifest: {error}"))
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_owned_file(path: &Path, changed: &mut Vec<PathBuf>, skipped: &mut Vec<String>) {
    match fs::remove_file(path) {
        Ok(()) => changed.push(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => skipped.push(format!("{}: {error}", path.display())),
    }
}

fn remove_owned_skill(path: &Path, changed: &mut Vec<PathBuf>, skipped: &mut Vec<String>) {
    match fs::read_to_string(path) {
        Ok(content) if content.contains(MANAGED_START) && content.contains(MANAGED_END) => {
            remove_owned_file(path, changed, skipped)
        }
        Ok(_) => skipped.push(format!(
            "{}: skill was edited or replaced; left in place",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => skipped.push(format!("{}: {error}", path.display())),
    }
}

fn should_write_skill(existing: Option<&str>) -> bool {
    existing.is_none_or(|content| content.contains(MANAGED_START))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = path.parent().ok_or_else(|| {
        FukidashiError::InvalidInput(format!("path has no parent: {}", path.display()))
    })?;
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    replace_temp(temp, path)
}

fn replace_temp(temp: NamedTempFile, destination: &Path) -> Result<()> {
    match temp.persist(destination) {
        Ok(_) => Ok(()),
        Err(error) if cfg!(windows) && error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Windows lacks POSIX rename-overwrite semantics. The destination
            // is always backed up before config writes; executable replacement
            // is best effort and remains recoverable through the manifest.
            fs::remove_file(destination)?;
            error
                .file
                .persist(destination)
                .map(|_| ())
                .map_err(|e| e.error.into())
        }
        Err(error) => Err(error.error.into()),
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_file(path: &Path) -> Result<String> {
    Ok(digest(&fs::read(path)?))
}

fn backup_name(path: &Path) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{stamp}-{}.bak",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config")
    )
}

/// Decide ownership and backup retention when patching produces byte-identical
/// output. In particular, an idempotent reinstall retains a prior `None`
/// backup, which means rollback knows the file was created by the first run.
fn unchanged_manifest_state(
    before: Option<&[u8]>,
    previous: Option<&ManifestClient>,
    entry_present: bool,
    points_to_current: bool,
    backup_dir: &Path,
    config_path: &Path,
) -> (Option<PathBuf>, bool) {
    if let (Some(bytes), Some(entry)) = (before, previous)
        && digest(bytes) == entry.after_sha256
    {
        return (entry.backup_path.clone(), entry.owned);
    }
    if entry_present && points_to_current {
        // The user had already configured this exact command. Observe it but
        // do not claim ownership, so uninstall leaves it intact.
        return (None, false);
    }
    (
        before.map(|_| backup_dir.join(backup_name(config_path))),
        true,
    )
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_patch_preserves_unrelated_servers() {
        let input = br#"{"mcpServers":{"other":{"command":"other"}},"theme":"dark"}"#;
        let patched = patch_json(Client::Cursor, input, Path::new("/opt/fukidashi-mcp")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mcpServers"]["other"]["command"], "other");
        assert_eq!(
            value["mcpServers"]["fukidashi"]["args"],
            serde_json::json!([])
        );
    }

    #[test]
    fn vscode_patch_uses_servers_schema_and_preserves_unrelated_servers() {
        let input = br#"{"servers":{"other":{"type":"stdio","command":"other"}},"inputs":[]}"#;
        let patched = patch_json(Client::VsCode, input, Path::new("/opt/fukidashi-mcp")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(value["inputs"], serde_json::json!([]));
        assert_eq!(value["servers"]["other"]["command"], "other");
        assert_eq!(value["servers"]["fukidashi"]["type"], "stdio");
        assert_eq!(value["servers"]["fukidashi"]["args"], serde_json::json!([]));
        assert!(value.get("mcpServers").is_none());

        let removed: serde_json::Value =
            serde_json::from_slice(&remove_entry(Client::VsCode, &patched).unwrap()).unwrap();
        assert!(removed["servers"]["fukidashi"].is_null());
        assert_eq!(removed["servers"]["other"]["command"], "other");
    }

    #[test]
    fn toml_patch_replaces_only_fukidashi_table() {
        let input = b"[other]\nvalue = 1\n\n[mcp_servers.fukidashi]\ncommand = \"old\"\n\n[mcp_servers.keep]\ncommand = \"keep\"\n";
        let patched = patch_toml(input, Path::new("/opt/fukidashi-mcp")).unwrap();
        let output = String::from_utf8(patched).unwrap();
        assert!(output.contains("value = 1"));
        assert!(output.contains("[mcp_servers.keep]"));
        assert!(output.contains("command = \"/opt/fukidashi-mcp\""));
        assert_eq!(output.matches("[mcp_servers.fukidashi]").count(), 1);
    }

    #[test]
    fn json_remove_keeps_other_servers() {
        let input = serde_json::to_vec(&serde_json::json!({"mcpServers":{"fukidashi":{"command":"x"},"other":{"command":"y"}}})).unwrap();
        let output: serde_json::Value =
            serde_json::from_slice(&remove_entry(Client::Cursor, &input).unwrap()).unwrap();
        assert!(output["mcpServers"]["fukidashi"].is_null());
        assert_eq!(output["mcpServers"]["other"]["command"], "y");
    }

    #[test]
    fn existing_different_json_entry_is_detectable() {
        let input = serde_json::to_vec(&serde_json::json!({
            "mcpServers": { "fukidashi": { "command": "user-owned" } }
        }))
        .unwrap();
        assert!(entry_present(Client::Cursor, &input));
        assert!(!config_contains(
            Client::Cursor,
            &input,
            Path::new("/opt/fukidashi-mcp")
        ));
    }

    #[test]
    fn toml_remove_deletes_nested_owned_tables_as_one_block() {
        let input = b"[mcp_servers.fukidashi]\ncommand = \"/opt/fukidashi-mcp\"\n[mcp_servers.fukidashi.env]\nFOO = \"bar\"\n[mcp_servers.keep]\ncommand = \"keep\"\n";
        let output = String::from_utf8(remove_entry(Client::Codex, input).unwrap()).unwrap();
        assert!(!output.contains("mcp_servers.fukidashi"));
        assert!(output.contains("[mcp_servers.keep]"));
    }

    #[test]
    fn json_server_does_not_override_provider() {
        let value = server_json(Client::Cursor, Path::new("/opt/fukidashi-mcp"));
        assert!(value.get("env").is_none());
    }

    #[test]
    fn idempotent_reinstall_preserves_missing_prior_backup() {
        let before = br#"{"mcpServers":{"fukidashi":{"command":"/opt/fukidashi-mcp"}}}"#;
        let previous = ManifestClient {
            client: Client::Cursor,
            config_path: PathBuf::from("/tmp/mcp.json"),
            backup_path: None,
            after_sha256: digest(before),
            skill_path: None,
            owned: true,
        };
        let (backup, owned) = unchanged_manifest_state(
            Some(before),
            Some(&previous),
            true,
            true,
            Path::new("/tmp/backups"),
            Path::new("/tmp/mcp.json"),
        );
        assert!(backup.is_none());
        assert!(owned);
    }

    #[test]
    fn preexisting_identical_entry_stays_user_owned() {
        let before = br#"{"mcpServers":{"fukidashi":{"command":"/opt/fukidashi-mcp"}}}"#;
        let (backup, owned) = unchanged_manifest_state(
            Some(before),
            None,
            true,
            true,
            Path::new("/tmp/backups"),
            Path::new("/tmp/mcp.json"),
        );
        assert!(backup.is_none());
        assert!(!owned);
    }

    #[test]
    fn installer_data_root_must_be_absolute() {
        assert!(validate_data_root(PathBuf::from("relative/fukidashi")).is_err());
        assert!(
            validate_data_root(if cfg!(windows) {
                PathBuf::from("C:\\Fukidashi")
            } else {
                PathBuf::from("/tmp/Fukidashi")
            })
            .is_ok()
        );
    }

    #[test]
    fn generated_skills_use_established_name_and_frontmatter() {
        let content = skill_contents(Client::Claude);
        assert!(content.starts_with("---\nname: fukidashi-comic-translation\n"));
        assert!(content.contains("description: "));
        assert!(content.contains(MANAGED_START));
        assert!(content.contains(MANAGED_END));
        let path = supported_skill_path(Client::Claude).unwrap().unwrap();
        assert!(path.ends_with("fukidashi-comic-translation/SKILL.md"));
        assert!(should_write_skill(None));
        assert!(should_write_skill(Some(&content)));
        assert!(!should_write_skill(Some("# user-owned skill\n")));
    }

    #[test]
    fn edited_skill_is_not_removed_on_uninstall() {
        let temp = tempfile::tempdir().unwrap();
        let skill = temp.path().join("SKILL.md");
        std::fs::write(&skill, "# user replacement\n").unwrap();
        let mut changed = Vec::new();
        let mut skipped = Vec::new();
        remove_owned_skill(&skill, &mut changed, &mut skipped);
        assert!(skill.is_file());
        assert!(changed.is_empty());
        assert_eq!(skipped.len(), 1);
    }
}
