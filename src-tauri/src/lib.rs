use anyhow::{anyhow, Context};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tauri::{Emitter, Manager};
use uuid::Uuid;
use zip::ZipArchive;

const MOJANG_MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

static NICK_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_]{3,16}$").expect("nickname regex"));
static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("RoundStudioLauncher/0.1.0")
        .build()
        .expect("http client")
});
static PROCESS_TABLE: Lazy<Mutex<HashMap<u64, Child>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static HANDLE_SEQ: AtomicU64 = AtomicU64::new(1);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum LoaderType {
    Vanilla,
    Fabric,
    Forge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    id: String,
    name: String,
    minecraft_version: String,
    loader: LoaderType,
    #[serde(default)]
    loader_version: Option<String>,
    nickname: String,
    ram_mb: u32,
    java_path: Option<String>,
    jvm_args: Vec<String>,
    skin_path: Option<String>,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProfileInput {
    name: String,
    minecraft_version: String,
    loader: LoaderType,
    loader_version: Option<String>,
    nickname: String,
    ram_mb: Option<u32>,
    java_path: Option<String>,
    jvm_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfilePatch {
    name: Option<String>,
    minecraft_version: Option<String>,
    loader: Option<LoaderType>,
    loader_version: Option<String>,
    nickname: Option<String>,
    ram_mb: Option<u32>,
    java_path: Option<String>,
    jvm_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileCloneInput {
    source_profile_id: String,
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileSummary {
    id: String,
    name: String,
    minecraft_version: String,
    loader: LoaderType,
    loader_version: Option<String>,
    nickname: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ContentKind {
    Mod,
    Resourcepack,
    Shaderpack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentItem {
    id: String,
    file_name: String,
    kind: ContentKind,
    source: String,
    added_at: String,
}

fn string_or_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        other => Some(other.to_string()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModrinthSearchHit {
    project_id: String,
    #[serde(default, deserialize_with = "string_or_default")]
    slug: String,
    #[serde(default, deserialize_with = "string_or_default")]
    title: String,
    #[serde(default, deserialize_with = "string_or_default")]
    description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModrinthSearchPage {
    hits: Vec<ModrinthSearchHit>,
    total_hits: u32,
    page: u32,
    page_size: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoaderOptions {
    fabric: Vec<String>,
    forge: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JavaResolution {
    required_major: u8,
    resolved_path: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SkinState {
    profile_id: String,
    skin_path: String,
    applied_with_csl: bool,
    warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationIssue {
    severity: String,
    code: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationReport {
    can_launch: bool,
    critical: Vec<ValidationIssue>,
    warnings: Vec<ValidationIssue>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallReport {
    success: bool,
    downloaded_files: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallProgress {
    step: u32,
    total: u32,
    percent: u8,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchHandle {
    id: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftVersion {
    id: String,
    version_type: String,
    release_time: String,
}

fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("{}", d.as_secs()),
        Err(_) => "0".to_string(),
    }
}

fn hide_command_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn app_root() -> PathBuf {
    if let Ok(app_data) = std::env::var("APPDATA") {
        return PathBuf::from(app_data).join("RoundStudioLauncher");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".round-studio-launcher")
}

fn profiles_root() -> PathBuf {
    app_root().join("profiles")
}

fn profile_root(profile_id: &str) -> PathBuf {
    profiles_root().join(profile_id)
}

fn profile_json_path(profile_id: &str) -> PathBuf {
    profile_root(profile_id).join("profile.json")
}

fn manifest_lock_path(profile_id: &str) -> PathBuf {
    profile_root(profile_id).join("manifest-lock.json")
}

fn instance_root(profile_id: &str) -> PathBuf {
    profile_root(profile_id).join("instance")
}

fn ensure_profile_dirs(profile_id: &str) -> anyhow::Result<()> {
    let instance = instance_root(profile_id);
    for dir in [
        instance.clone(),
        instance.join("mods"),
        instance.join("textures"),
        instance.join("shaders"),
        instance.join("resourcepacks"),
        instance.join("shaderpacks"),
        instance.join("versions"),
        instance.join("libraries"),
        instance.join("assets"),
        instance.join("natives"),
        instance.join("skin"),
    ] {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(src_path, dst_path)?;
        }
    }
    Ok(())
}

fn save_json_atomic<T: Serialize + ?Sized>(path: &Path, payload: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(payload)?;
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn load_profile(profile_id: &str) -> anyhow::Result<Profile> {
    let path = profile_json_path(profile_id);
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_profile(profile: &Profile) -> anyhow::Result<()> {
    save_json_atomic(&profile_json_path(&profile.id), profile)
}

fn require_valid_nick(nick: &str) -> anyhow::Result<()> {
    if !NICK_RE.is_match(nick) {
        return Err(anyhow!("Nickname must match [A-Za-z0-9_]{{3,16}}"));
    }
    Ok(())
}

fn offline_uuid(nickname: &str) -> String {
    let id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("OfflinePlayer:{nickname}").as_bytes(),
    );
    id.as_hyphenated().to_string()
}

fn minecraft_java_major(version: &str) -> u8 {
    if let Some((major, minor)) = parse_mc(version) {
        if major > 1 || minor >= 21 {
            return 21;
        }
        if minor >= 18 {
            return 17;
        }
        return 8;
    }
    17
}

fn parse_mc(v: &str) -> Option<(u8, u8)> {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let major = parts[0].parse::<u8>().ok()?;
    let minor = parts[1].parse::<u8>().ok()?;
    Some((major, minor))
}

fn push_java_candidate(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if path.exists() && !candidates.iter().any(|existing| existing == &path) {
        candidates.push(path);
    }
}

fn collect_java_candidates(required_major: u8) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    for exe_name in ["java.exe", "javaw.exe"] {
        push_java_candidate(
            &mut candidates,
            app_root()
                .join("java")
                .join(format!("jdk-{required_major}"))
                .join("bin")
                .join(exe_name),
        );
    }

    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        for exe_name in ["java.exe", "javaw.exe"] {
            push_java_candidate(
                &mut candidates,
                PathBuf::from(&java_home).join("bin").join(exe_name),
            );
        }
    }

    let roots = [
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramW6432").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
    ];
    let vendors = [
        "Java",
        "Eclipse Adoptium",
        "Adoptium",
        "Microsoft",
        "BellSoft",
        "Amazon Corretto",
        "Zulu",
        "Azul",
        "Oracle",
        "GraalVM",
    ];

    for root in roots.into_iter().flatten() {
        for vendor in vendors {
            let vendor_dir = PathBuf::from(&root).join(vendor);
            if !vendor_dir.exists() {
                continue;
            }
            if let Ok(entries) = fs::read_dir(vendor_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    push_java_candidate(&mut candidates, path.join("bin").join("java.exe"));
                    push_java_candidate(&mut candidates, path.join("bin").join("javaw.exe"));
                }
            }
        }
    }

    for cmd in ["java.exe", "javaw.exe"] {
        let mut command = Command::new("where");
        command.arg(cmd);
        hide_command_window(&mut command);
        if let Ok(out) = command.output() {
            if out.status.success() {
                if let Ok(text) = String::from_utf8(out.stdout) {
                    for line in text.lines() {
                        let candidate = PathBuf::from(line.trim());
                        push_java_candidate(&mut candidates, candidate);
                    }
                }
            }
        }
    }

    candidates
}

fn resolve_best_system_java(required_major: u8) -> Option<(String, String)> {
    let mut matches = collect_java_candidates(required_major)
        .into_iter()
        .filter_map(|path| {
            let path_str = path.to_string_lossy().to_string();
            let console_path = to_java_console_path(&path_str);
            let major = detect_java_major(&console_path)?;
            let bits = detect_java_bitness(&console_path).unwrap_or(0);
            Some((path_str, major, bits))
        })
        .filter(|(_, major, _)| *major >= required_major)
        .collect::<Vec<_>>();

    matches.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });

    matches
        .into_iter()
        .next()
        .map(|(path, _, _)| (path, "systemInstalled".to_string()))
}

fn resolve_java_for_profile(profile: &Profile) -> JavaResolution {
    let required_major = required_java_major_from_installed_meta(profile)
        .unwrap_or_else(|| minecraft_java_major(&profile.minecraft_version));
    if let Some(path) = &profile.java_path {
        if Path::new(path).exists() {
            return JavaResolution {
                required_major,
                resolved_path: Some(path.clone()),
                source: Some("profileOverride".to_string()),
            };
        }
    }

    let bundled = app_root()
        .join("java")
        .join(format!("jdk-{required_major}"))
        .join("bin")
        .join("java.exe");
    if bundled.exists() {
        return JavaResolution {
            required_major,
            resolved_path: Some(bundled.to_string_lossy().to_string()),
            source: Some("bundledTemurin".to_string()),
        };
    }

    let bundled = app_root()
        .join("java")
        .join(format!("jdk-{required_major}"))
        .join("bin")
        .join("javaw.exe");
    if bundled.exists() {
        return JavaResolution {
            required_major,
            resolved_path: Some(bundled.to_string_lossy().to_string()),
            source: Some("bundledTemurin".to_string()),
        };
    }

    if let Some((path, source)) = resolve_best_system_java(required_major) {
        return JavaResolution {
            required_major,
            resolved_path: Some(path),
            source: Some(source),
        };
    }

    JavaResolution {
        required_major,
        resolved_path: None,
        source: None,
    }
}

fn to_java_console_path(path: &str) -> String {
    let p = PathBuf::from(path);
    let file_name = p
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if file_name == "javaw.exe" {
        let java = p.with_file_name("java.exe");
        if java.exists() {
            return java.to_string_lossy().to_string();
        }
    }
    path.to_string()
}

fn detect_java_bitness(java_path: &str) -> Option<u32> {
    let mut command = Command::new(java_path);
    command.args(["-XshowSettings:properties", "-version"]);
    hide_command_window(&mut command);
    let out = command.output().ok()?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    for line in text.lines() {
        let normalized = line.trim().to_ascii_lowercase();
        if normalized.contains("sun.arch.data.model") {
            if normalized.contains("64") {
                return Some(64);
            }
            if normalized.contains("32") {
                return Some(32);
            }
        }
    }
    None
}

fn parse_java_major(version_output: &str) -> Option<u8> {
    let token = version_output.lines().find_map(|line| {
        if let Some(start_idx) = line.find("version \"") {
            let rest = &line[start_idx + 9..];
            let end_idx = rest.find('"')?;
            return Some(rest[..end_idx].to_string());
        }
        None
    })?;

    let normalized = token.trim();
    if let Some(rest) = normalized.strip_prefix("1.") {
        return rest
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|part| part.parse::<u8>().ok());
    }

    normalized
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|part| part.parse::<u8>().ok())
}

fn detect_java_major(java_path: &str) -> Option<u8> {
    let mut command = Command::new(java_path);
    command.arg("-version");
    hide_command_window(&mut command);
    let out = command.output().ok()?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    parse_java_major(&text)
}

fn sanitized_jvm_args(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|arg| {
            let a = arg.trim();
            if a.is_empty() {
                return false;
            }
            if !a.starts_with('-') {
                return false;
            }
            if a.starts_with("-Xmx") || a.starts_with("-Xms") {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn effective_ram_mb(requested: u32, java_bits: Option<u32>) -> u32 {
    let base = requested.max(1024);
    match java_bits {
        Some(32) => base.min(1024),
        _ => base.min(8192),
    }
}

fn content_dir(profile_id: &str, kind: &ContentKind) -> PathBuf {
    let root = instance_root(profile_id);
    match kind {
        ContentKind::Mod => root.join("mods"),
        ContentKind::Resourcepack => root.join("textures"),
        ContentKind::Shaderpack => root.join("shaders"),
    }
}

fn compat_content_dir(profile_id: &str, kind: &ContentKind) -> Option<PathBuf> {
    let root = instance_root(profile_id);
    match kind {
        ContentKind::Mod => None,
        ContentKind::Resourcepack => Some(root.join("resourcepacks")),
        ContentKind::Shaderpack => Some(root.join("shaderpacks")),
    }
}

fn load_lock(profile_id: &str) -> anyhow::Result<Vec<ContentItem>> {
    let path = manifest_lock_path(profile_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_lock(profile_id: &str, items: &[ContentItem]) -> anyhow::Result<()> {
    save_json_atomic(&manifest_lock_path(profile_id), items)
}

fn append_lock_item(profile_id: &str, item: &ContentItem) -> anyhow::Result<()> {
    let mut lock = load_lock(profile_id)?;
    lock.push(item.clone());
    save_lock(profile_id, &lock)
}

fn process_table() -> anyhow::Result<MutexGuard<'static, HashMap<u64, Child>>> {
    PROCESS_TABLE
        .lock()
        .map_err(|_| anyhow!("Failed to access process table"))
}

fn spawn_process_watcher(app: tauri::AppHandle, handle_id: u64) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(250));

        let state = {
            let mut table = match process_table() {
                Ok(t) => t,
                Err(err) => {
                    let _ = app.emit(
                        "launch/log",
                        format!("[launcher] process table error: {err}"),
                    );
                    break;
                }
            };

            let mut exit_code: Option<i32> = None;
            if let Some(child) = table.get_mut(&handle_id) {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        exit_code = Some(status.code().unwrap_or(-1));
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = app.emit(
                            "launch/log",
                            format!("[launcher] failed to read process status: {err}"),
                        );
                        exit_code = Some(-1);
                    }
                }
            } else {
                break;
            }

            if let Some(code) = exit_code {
                table.remove(&handle_id);
                Some(code)
            } else {
                None
            }
        };

        if let Some(code) = state {
            let _ = app.emit("launch/state", format!("exited:{handle_id}:{code}"));
            if code != 0 {
                let _ = app.emit(
                    "launch/log",
                    format!("[launcher] process exited with code {code}"),
                );
            }
            break;
        }
    });
}

async fn http_json<T: for<'de> Deserialize<'de>>(url: &str) -> anyhow::Result<T> {
    let resp = HTTP_CLIENT.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()));
    }
    Ok(resp.json::<T>().await?)
}

#[derive(Debug, Deserialize)]
struct MojangManifest {
    versions: Vec<MojangVersionEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct MojangVersionEntry {
    id: String,
    #[serde(rename = "type")]
    version_type: String,
    #[serde(rename = "releaseTime")]
    release_time: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct MojangVersionMeta {
    downloads: MojangDownloads,
    #[serde(rename = "assetIndex")]
    asset_index: Option<MojangAssetIndex>,
}

#[derive(Debug, Deserialize)]
struct ForgeInstallerProfile {
    #[serde(rename = "versionInfo")]
    version_info: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct MojangDownloads {
    client: MojangArtifact,
}

#[derive(Debug, Deserialize)]
struct MojangArtifact {
    url: String,
    sha1: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MojangLibrary {
    name: Option<String>,
    url: Option<String>,
    natives: Option<HashMap<String, String>>,
    rules: Option<Vec<MojangLibraryRule>>,
    downloads: Option<MojangLibraryDownloads>,
}

#[derive(Debug, Deserialize)]
struct MojangLibraryDownloads {
    artifact: Option<MojangLibraryArtifact>,
    classifiers: Option<HashMap<String, MojangLibraryArtifact>>,
}

#[derive(Debug, Deserialize, Clone)]
struct MojangLibraryArtifact {
    path: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct MojangLibraryRule {
    action: String,
    os: Option<MojangRuleOs>,
    features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Deserialize)]
struct MojangRuleOs {
    name: Option<String>,
    arch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MojangAssetIndex {
    id: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct MojangAssetIndexFile {
    objects: HashMap<String, MojangAssetObject>,
}

#[derive(Debug, Deserialize)]
struct MojangAssetObject {
    hash: String,
}

#[derive(Debug, Deserialize)]
struct LaunchVersionMeta {
    id: String,
    #[serde(rename = "mainClass")]
    main_class: String,
    #[serde(rename = "type")]
    version_type: Option<String>,
    #[serde(default)]
    libraries: Vec<MojangLibrary>,
    #[serde(rename = "assetIndex")]
    asset_index: Option<MojangAssetIndex>,
    #[serde(rename = "minecraftArguments")]
    minecraft_arguments: Option<String>,
    arguments: Option<LaunchArguments>,
    #[serde(rename = "javaVersion")]
    java_version: Option<LaunchJavaVersion>,
}

#[derive(Debug, Deserialize)]
struct LaunchJavaVersion {
    #[serde(rename = "majorVersion")]
    major_version: u8,
}

#[derive(Debug, Deserialize)]
struct LaunchArguments {
    #[serde(default)]
    game: Vec<LaunchArgumentItem>,
    #[serde(default)]
    jvm: Vec<LaunchArgumentItem>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LaunchArgumentItem {
    String(String),
    Conditional {
        rules: Option<Vec<MojangLibraryRule>>,
        value: LaunchArgumentValue,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LaunchArgumentValue {
    String(String),
    Array(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct ModrinthVersionFile {
    url: String,
    filename: String,
    #[serde(default)]
    primary: bool,
}

#[derive(Debug, Deserialize)]
struct ModrinthVersion {
    files: Vec<ModrinthVersionFile>,
}

#[tauri::command]
fn profiles_list() -> Result<Vec<ProfileSummary>, String> {
    fs::create_dir_all(profiles_root()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();

    for entry in fs::read_dir(profiles_root()).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.path().is_dir() {
            continue;
        }
        let p = entry.path().join("profile.json");
        if !p.exists() {
            continue;
        }
        let bytes = fs::read(p).map_err(|e| e.to_string())?;
        let profile: Profile = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        out.push(ProfileSummary {
            id: profile.id,
            name: profile.name,
            minecraft_version: profile.minecraft_version,
            loader: profile.loader,
            loader_version: profile.loader_version,
            nickname: profile.nickname,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[tauri::command]
fn profiles_create(input: CreateProfileInput) -> Result<Profile, String> {
    require_valid_nick(&input.nickname).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4().to_string();
    let profile = Profile {
        id: id.clone(),
        name: input.name,
        minecraft_version: input.minecraft_version,
        loader: input.loader,
        loader_version: input.loader_version,
        nickname: input.nickname,
        ram_mb: input.ram_mb.unwrap_or(4096),
        java_path: input.java_path,
        jvm_args: input.jvm_args.unwrap_or_default(),
        skin_path: None,
        created_at: now_rfc3339(),
    };
    ensure_profile_dirs(&id).map_err(|e| e.to_string())?;
    save_profile(&profile).map_err(|e| e.to_string())?;
    save_lock(&id, &[]).map_err(|e| e.to_string())?;
    Ok(profile)
}

#[tauri::command]
fn profiles_get(profile_id: String) -> Result<Profile, String> {
    load_profile(&profile_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn profiles_clone(input: ProfileCloneInput) -> Result<Profile, String> {
    let mut profile = load_profile(&input.source_profile_id).map_err(|e| e.to_string())?;
    let source_root = profile_root(&input.source_profile_id);
    let new_id = Uuid::new_v4().to_string();
    let target_root = profile_root(&new_id);
    copy_dir_recursive(&source_root, &target_root).map_err(|e| e.to_string())?;

    let trimmed_name = input.name.unwrap_or_default().trim().to_string();
    profile.id = new_id.clone();
    profile.name = if trimmed_name.is_empty() {
        format!("{} Copy", profile.name)
    } else {
        trimmed_name
    };
    profile.created_at = now_rfc3339();
    ensure_profile_dirs(&new_id).map_err(|e| e.to_string())?;
    save_profile(&profile).map_err(|e| e.to_string())?;
    if !manifest_lock_path(&new_id).exists() {
        save_lock(&new_id, &[]).map_err(|e| e.to_string())?;
    }
    Ok(profile)
}

#[tauri::command]
fn profiles_update(profile_id: String, patch: ProfilePatch) -> Result<Profile, String> {
    let mut profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    if let Some(name) = patch.name {
        profile.name = name;
    }
    if let Some(v) = patch.minecraft_version {
        profile.minecraft_version = v;
    }
    if let Some(loader) = patch.loader {
        profile.loader = loader;
        if profile.loader == LoaderType::Vanilla {
            profile.loader_version = None;
        }
    }
    if let Some(loader_version) = patch.loader_version {
        profile.loader_version = if loader_version.trim().is_empty() {
            None
        } else {
            Some(loader_version)
        };
    }
    if let Some(nick) = patch.nickname {
        require_valid_nick(&nick).map_err(|e| e.to_string())?;
        profile.nickname = nick;
    }
    if let Some(ram) = patch.ram_mb {
        profile.ram_mb = ram.max(1024);
    }
    if let Some(java_path) = patch.java_path {
        profile.java_path = if java_path.trim().is_empty() {
            None
        } else {
            Some(java_path)
        };
    }
    if let Some(args) = patch.jvm_args {
        profile.jvm_args = args;
    }
    save_profile(&profile).map_err(|e| e.to_string())?;
    Ok(profile)
}

#[tauri::command]
fn profiles_delete(profile_id: String) -> Result<(), String> {
    let root = profile_root(&profile_id);
    if root.exists() {
        fs::remove_dir_all(root).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn versions_list_minecraft() -> Result<Vec<MinecraftVersion>, String> {
    let manifest: MojangManifest = http_json(MOJANG_MANIFEST_URL)
        .await
        .map_err(|e| e.to_string())?;

    let out = manifest
        .versions
        .into_iter()
        .map(|v| MinecraftVersion {
            id: v.id,
            version_type: v.version_type,
            release_time: v.release_time,
        })
        .collect::<Vec<_>>();
    Ok(out)
}

async fn fetch_loader_options(mc_version: &str) -> Result<LoaderOptions, String> {
    let fabric_url = format!(
        "https://meta.fabricmc.net/v2/versions/loader/{}",
        mc_version
    );
    let fabric_raw = HTTP_CLIENT
        .get(fabric_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Vec<Value>>()
        .await
        .map_err(|e| e.to_string())?;

    let fabric = fabric_raw
        .iter()
        .filter_map(|x| x.get("loader")?.get("version")?.as_str())
        .take(20)
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    let forge_json: Value =
        http_json("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json")
            .await
            .map_err(|e| e.to_string())?;

    let mut forge = Vec::new();
    if let Some(obj) = forge_json.get("promos").and_then(|x| x.as_object()) {
        for (key, value) in obj {
            if key.starts_with(mc_version) {
                if let Some(v) = value.as_str() {
                    forge.push(v.to_string());
                }
            }
        }
    }
    forge.sort();
    forge.dedup();

    Ok(LoaderOptions { fabric, forge })
}

#[tauri::command]
async fn loaders_list(profile_id: String) -> Result<LoaderOptions, String> {
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    fetch_loader_options(&profile.minecraft_version).await
}

#[tauri::command]
async fn loaders_for_version(minecraft_version: String) -> Result<LoaderOptions, String> {
    fetch_loader_options(&minecraft_version).await
}

#[tauri::command]
fn java_resolve(profile_id: String) -> Result<JavaResolution, String> {
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    Ok(resolve_java_for_profile(&profile))
}

#[tauri::command]
fn open_versions_folder(profile_id: String) -> Result<String, String> {
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    let versions_path = instance_root(&profile.id).join("versions");
    fs::create_dir_all(&versions_path).map_err(|e| e.to_string())?;
    Command::new("explorer")
        .arg(&versions_path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(versions_path.to_string_lossy().to_string())
}

#[tauri::command]
fn content_import_local(
    profile_id: String,
    kind: ContentKind,
    file_path: String,
) -> Result<ContentItem, String> {
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    ensure_profile_dirs(&profile.id).map_err(|e| e.to_string())?;
    let src = PathBuf::from(file_path);
    if !src.exists() {
        return Err("File not found".to_string());
    }
    let file_name = src
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "Invalid file name".to_string())?
        .to_string();
    let dst = content_dir(&profile.id, &kind).join(&file_name);
    fs::copy(&src, &dst).map_err(|e| e.to_string())?;
    if let Some(compat_dir) = compat_content_dir(&profile.id, &kind) {
        fs::create_dir_all(&compat_dir).map_err(|e| e.to_string())?;
        let compat_dst = compat_dir.join(&file_name);
        fs::copy(&src, &compat_dst).map_err(|e| e.to_string())?;
    }

    let item = ContentItem {
        id: Uuid::new_v4().to_string(),
        file_name,
        kind,
        source: "local".to_string(),
        added_at: now_rfc3339(),
    };
    append_lock_item(&profile.id, &item).map_err(|e| e.to_string())?;
    Ok(item)
}

#[tauri::command]
fn content_list(profile_id: String) -> Result<Vec<ContentItem>, String> {
    let _profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    load_lock(&profile_id).map_err(|e| e.to_string())
}

#[tauri::command]
async fn content_search_modrinth(
    kind: ContentKind,
    query: String,
    version: String,
    loader: String,
    sort_index: Option<String>,
    page: Option<u32>,
) -> Result<ModrinthSearchPage, String> {
    let page_size = 20_u32;
    let page = page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let project_type = match kind {
        ContentKind::Mod => "mod",
        ContentKind::Resourcepack => "resourcepack",
        ContentKind::Shaderpack => "shader",
    };
    let mut facets = vec![
        format!("[\"project_type:{project_type}\"]"),
        format!("[\"versions:{version}\"]"),
    ];
    if matches!(kind, ContentKind::Mod) && !loader.is_empty() && loader != "vanilla" {
        facets.push(format!("[\"categories:{loader}\"]"));
    }
    let facets_str = format!("[{}]", facets.join(","));
    let index = match sort_index.as_deref().map(str::trim) {
        Some("downloads") => "downloads",
        Some("follows") => "follows",
        Some("updated") => "updated",
        Some("newest") => "newest",
        _ => "relevance",
    };
    let url = format!(
        "https://api.modrinth.com/v2/search?query={}&limit={}&offset={}&index={}&facets={}",
        urlencoding::encode(&query),
        page_size,
        offset,
        urlencoding::encode(index),
        urlencoding::encode(&facets_str)
    );

    let payload: Value = http_json(&url).await.map_err(|e| e.to_string())?;
    let hits = payload
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let results = hits
        .into_iter()
        .filter_map(|item| {
            let project_id = item
                .get("project_id")
                .or_else(|| item.get("projectId"))
                .and_then(value_to_string)
                .unwrap_or_default();
            if project_id.is_empty() {
                return None;
            }

            Some(ModrinthSearchHit {
                project_id,
                slug: item
                    .get("slug")
                    .and_then(value_to_string)
                    .unwrap_or_default(),
                title: item
                    .get("title")
                    .and_then(value_to_string)
                    .unwrap_or_else(|| "Untitled".to_string()),
                description: item
                    .get("description")
                    .and_then(value_to_string)
                    .unwrap_or_default(),
            })
        })
        .collect();

    let total_hits = payload
        .get("total_hits")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);

    Ok(ModrinthSearchPage {
        hits: results,
        total_hits,
        page,
        page_size,
    })
}

async fn fetch_modrinth_version(
    project_id: &str,
    version_id: &str,
    game_version: &str,
    kind: &ContentKind,
    loader: &LoaderType,
) -> anyhow::Result<ModrinthVersion> {
    if !version_id.trim().is_empty() {
        let url = format!("https://api.modrinth.com/v2/version/{version_id}");
        return http_json(&url).await;
    }

    let mut url = format!(
        "https://api.modrinth.com/v2/project/{project_id}/version?game_versions=[\"{}\"]",
        game_version
    );
    if matches!(kind, ContentKind::Mod) && *loader != LoaderType::Vanilla {
        let l = match loader {
            LoaderType::Fabric => "fabric",
            LoaderType::Forge => "forge",
            LoaderType::Vanilla => "vanilla",
        };
        url.push_str(&format!("&loaders=[\"{l}\"]"));
    }
    let versions: Vec<ModrinthVersion> = http_json(&url).await?;
    versions
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No compatible file version found"))
}

#[tauri::command]
async fn content_install_modrinth(
    profile_id: String,
    project_id: String,
    version_id: String,
    kind: ContentKind,
) -> Result<ContentItem, String> {
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    ensure_profile_dirs(&profile.id).map_err(|e| e.to_string())?;
    let version = fetch_modrinth_version(
        &project_id,
        &version_id,
        &profile.minecraft_version,
        &kind,
        &profile.loader,
    )
    .await
    .map_err(|e| e.to_string())?;

    let file = version
        .files
        .iter()
        .find(|f| f.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| "Selected Modrinth version has no files".to_string())?;

    let bytes = HTTP_CLIENT
        .get(&file.url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    let dst = content_dir(&profile.id, &kind).join(&file.filename);
    fs::write(&dst, bytes).map_err(|e| e.to_string())?;
    if let Some(compat_dir) = compat_content_dir(&profile.id, &kind) {
        fs::create_dir_all(&compat_dir).map_err(|e| e.to_string())?;
        let compat_dst = compat_dir.join(&file.filename);
        let copied = fs::read(&dst).map_err(|e| e.to_string())?;
        fs::write(compat_dst, copied).map_err(|e| e.to_string())?;
    }

    let item = ContentItem {
        id: Uuid::new_v4().to_string(),
        file_name: file.filename.clone(),
        kind,
        source: "modrinth".to_string(),
        added_at: now_rfc3339(),
    };
    append_lock_item(&profile.id, &item).map_err(|e| e.to_string())?;
    Ok(item)
}

async fn try_install_custom_skin_loader(profile: &Profile) -> anyhow::Result<()> {
    if profile.loader == LoaderType::Vanilla {
        return Ok(());
    }

    let project = "customskinloader";
    let kind = ContentKind::Mod;
    let version =
        fetch_modrinth_version(project, "", &profile.minecraft_version, &kind, &profile.loader)
            .await?;
    let file = version
        .files
        .iter()
        .find(|f| f.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| anyhow!("CustomSkinLoader: empty version payload"))?;
    let bytes = HTTP_CLIENT.get(&file.url).send().await?.bytes().await?;
    let dst = content_dir(&profile.id, &kind).join(&file.filename);
    fs::write(dst, bytes)?;
    Ok(())
}

#[tauri::command]
async fn skin_set(profile_id: String, png_path: String) -> Result<SkinState, String> {
    let mut profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    ensure_profile_dirs(&profile.id).map_err(|e| e.to_string())?;
    let src = PathBuf::from(png_path);
    if !src.exists() {
        return Err("PNG file not found".to_string());
    }

    let dst = instance_root(&profile.id).join("skin").join("current.png");
    fs::copy(src, &dst).map_err(|e| e.to_string())?;
    profile.skin_path = Some(dst.to_string_lossy().to_string());
    save_profile(&profile).map_err(|e| e.to_string())?;

    let mut warning = None;
    let mut applied_with_csl = false;
    if profile.loader == LoaderType::Vanilla {
        warning = Some("Skin application may not work in vanilla.".to_string());
    } else if let Err(err) = try_install_custom_skin_loader(&profile).await {
        warning = Some(format!(
            "Failed to install CustomSkinLoader automatically: {err}"
        ));
    } else {
        applied_with_csl = true;
    }

    Ok(SkinState {
        profile_id,
        skin_path: dst.to_string_lossy().to_string(),
        applied_with_csl,
        warning,
    })
}

#[tauri::command]
fn profile_validate(profile_id: String) -> Result<ValidationReport, String> {
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    let mut critical = Vec::new();
    let mut warnings = Vec::new();

    if profile.loader != LoaderType::Vanilla
        && profile
            .loader_version
            .as_deref()
            .is_none_or(|v| v.trim().is_empty())
    {
        critical.push(ValidationIssue {
            severity: "critical".to_string(),
            code: "MISSING_LOADER_VERSION".to_string(),
            message: "Loader version is not selected.".to_string(),
        });
    }

    if let Err(err) = require_valid_nick(&profile.nickname) {
        critical.push(ValidationIssue {
            severity: "critical".to_string(),
            code: "INVALID_NICK".to_string(),
            message: err.to_string(),
        });
    }

    let java = resolve_java_for_profile(&profile);
    if java.resolved_path.is_none() {
        critical.push(ValidationIssue {
            severity: "critical".to_string(),
            code: "JAVA_NOT_FOUND".to_string(),
            message: format!(
                "Java {} not found (java.exe/javaw.exe)",
                java.required_major
            ),
        });
    } else if let Some(path) = &java.resolved_path {
        let java_path = to_java_console_path(path);
        if let Some(found_major) = detect_java_major(&java_path) {
            if found_major < java.required_major {
                critical.push(ValidationIssue {
                    severity: "critical".to_string(),
                    code: "JAVA_VERSION_MISMATCH".to_string(),
                    message: format!(
                        "Java {} is required for Minecraft {}, but detected Java {} at {}",
                        java.required_major, profile.minecraft_version, found_major, java_path
                    ),
                });
            }
        } else {
            warnings.push(ValidationIssue {
                severity: "warning".to_string(),
                code: "JAVA_VERSION_UNKNOWN".to_string(),
                message: "Unable to detect Java version from selected executable.".to_string(),
            });
        }
    }

    let client_jar = instance_root(&profile.id)
        .join("versions")
        .join(&profile.minecraft_version)
        .join(format!("{}.jar", profile.minecraft_version));
    if !client_jar.exists() {
        critical.push(ValidationIssue {
            severity: "critical".to_string(),
            code: "MISSING_CLIENT_JAR".to_string(),
            message: "Minecraft client is not installed. Run profile install first.".to_string(),
        });
    }
    let mut library_jars = Vec::new();
    if collect_jars_recursive(
        &instance_root(&profile.id).join("libraries"),
        &mut library_jars,
    )
    .is_ok()
        && library_jars.is_empty()
    {
        critical.push(ValidationIssue {
            severity: "critical".to_string(),
            code: "MISSING_LIBRARIES".to_string(),
            message: "Minecraft libraries are missing. Run install before launch.".to_string(),
        });
    }
    let natives_dir = instance_root(&profile.id).join("natives");
    let mut has_natives = has_native_binaries(&natives_dir);
    if !has_natives && restore_natives_from_library_cache(&profile.id).is_ok() {
        has_natives = has_native_binaries(&natives_dir);
    }
    if !has_natives {
        critical.push(ValidationIssue {
            severity: "critical".to_string(),
            code: "MISSING_NATIVES".to_string(),
            message: "Minecraft native libraries are missing. Run install before launch."
                .to_string(),
        });
    }
    if let Ok(meta) = load_launch_meta(&profile) {
        let asset_id = meta
            .asset_index
            .as_ref()
            .map(|x| x.id.clone())
            .unwrap_or_else(|| profile.minecraft_version.clone());
        let index_path = instance_root(&profile.id)
            .join("assets")
            .join("indexes")
            .join(format!("{asset_id}.json"));
        if !index_path.exists() {
            critical.push(ValidationIssue {
                severity: "critical".to_string(),
                code: "MISSING_ASSET_INDEX".to_string(),
                message: "Asset index is missing. Run install before launch.".to_string(),
            });
        } else if let Ok(bytes) = fs::read(&index_path) {
            if let Ok(index) = serde_json::from_slice::<MojangAssetIndexFile>(&bytes) {
                if let Some(any) = index.objects.values().next() {
                    let obj_path = instance_root(&profile.id)
                        .join("assets")
                        .join("objects")
                        .join(&any.hash[..2])
                        .join(&any.hash);
                    if !obj_path.exists() {
                        critical.push(ValidationIssue {
                            severity: "critical".to_string(),
                            code: "MISSING_ASSETS".to_string(),
                            message: "Assets are incomplete. Run install before launch."
                                .to_string(),
                        });
                    }
                }
            }
        }
    }

    if profile.loader != LoaderType::Vanilla {
        warnings.push(ValidationIssue {
            severity: "warning".to_string(),
            code: "LOADER_BETA".to_string(),
            message: "Fabric is fully wired into the launcher. Forge uses a lightweight installer flow and may still fail on some modern packs.".to_string(),
        });
    }

    let shaderpacks = instance_root(&profile.id).join("shaderpacks");
    if shaderpacks.exists() {
        let has_shaderpacks = fs::read_dir(shaderpacks)
            .ok()
            .map(|iter| iter.filter_map(|x| x.ok()).count() > 0)
            .unwrap_or(false);
        if has_shaderpacks {
            let mods_dir = instance_root(&profile.id).join("mods");
            let has_runtime = fs::read_dir(mods_dir)
                .ok()
                .map(|iter| {
                    iter.filter_map(|x| x.ok()).any(|e| {
                        let name = e.file_name().to_string_lossy().to_lowercase();
                        name.contains("iris")
                            || name.contains("oculus")
                            || name.contains("optifine")
                    })
                })
                .unwrap_or(false);
            if !has_runtime {
                warnings.push(ValidationIssue {
                    severity: "warning".to_string(),
                    code: "MISSING_SHADER_RUNTIME".to_string(),
                    message: "Shaders are present, but Iris/Oculus/OptiFine was not found."
                        .to_string(),
                });
            }
        }
    }

    Ok(ValidationReport {
        can_launch: critical.is_empty(),
        critical,
        warnings,
    })
}

async fn download_to_file(url: &str, path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let bytes = HTTP_CLIENT.get(url).send().await?.bytes().await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn emit_install_progress(
    app: &tauri::AppHandle,
    step: u32,
    total: u32,
    message: impl Into<String>,
) -> Result<(), String> {
    let total = total.max(1);
    let step = step.min(total);
    let percent = ((step as f32 / total as f32) * 100.0).round() as u8;
    app.emit(
        "install/progress",
        InstallProgress {
            step,
            total,
            percent,
            message: message.into(),
        },
    )
    .map_err(|e| e.to_string())
}

fn maven_library_path(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let group = parts[0].replace('.', "/");
    let artifact = parts[1];
    let version = parts[2];
    let classifier = if parts.len() >= 4 {
        format!("-{}", parts[3])
    } else {
        String::new()
    };
    Some(format!(
        "{group}/{artifact}/{version}/{artifact}-{version}{classifier}.jar"
    ))
}

fn current_minecraft_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

fn current_minecraft_arch() -> &'static str {
    if cfg!(target_pointer_width = "64") {
        "64"
    } else {
        "32"
    }
}

fn unique_library_key(value: &Value) -> String {
    value
        .get("name")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .get("downloads")
                .and_then(|d| d.get("artifact"))
                .and_then(|a| a.get("path"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| value.to_string())
}

fn merge_version_values(base: Value, overlay: Value) -> anyhow::Result<Value> {
    let mut merged = match base {
        Value::Object(map) => map,
        _ => return Err(anyhow!("base version metadata is not an object")),
    };
    let overlay = match overlay {
        Value::Object(map) => map,
        _ => return Err(anyhow!("loader metadata is not an object")),
    };

    let mut merged_libraries = merged
        .remove("libraries")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let mut seen = merged_libraries
        .iter()
        .map(unique_library_key)
        .collect::<std::collections::HashSet<_>>();

    for (key, value) in overlay {
        if key == "libraries" {
            if let Some(arr) = value.as_array() {
                for item in arr {
                    let item = item.clone();
                    let dedupe_key = unique_library_key(&item);
                    if seen.insert(dedupe_key) {
                        merged_libraries.push(item);
                    }
                }
            }
            continue;
        }
        if key == "arguments" {
            let mut merged_arguments = merged
                .get("arguments")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(overlay_arguments) = value.as_object() {
                for (arg_key, arg_value) in overlay_arguments {
                    if let Some(overlay_array) = arg_value.as_array() {
                        let mut combined = merged_arguments
                            .get(arg_key)
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        combined.extend(overlay_array.iter().cloned());
                        merged_arguments.insert(arg_key.clone(), Value::Array(combined));
                    } else {
                        merged_arguments.insert(arg_key.clone(), arg_value.clone());
                    }
                }
            }
            merged.insert("arguments".to_string(), Value::Object(merged_arguments));
            continue;
        }
        if key == "inheritsFrom" {
            continue;
        }
        merged.insert(key, value);
    }

    merged.insert("libraries".to_string(), Value::Array(merged_libraries));
    Ok(Value::Object(merged))
}

fn write_json_value(path: &Path, value: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

async fn fetch_vanilla_entry_and_meta(
    minecraft_version: &str,
) -> anyhow::Result<(MojangVersionEntry, MojangVersionMeta, Value)> {
    let manifest: MojangManifest = http_json(MOJANG_MANIFEST_URL).await?;
    let entry = manifest
        .versions
        .into_iter()
        .find(|v| v.id == minecraft_version)
        .ok_or_else(|| anyhow!("Minecraft version is missing in official manifest"))?;
    let base_json: Value = http_json(&entry.url).await?;
    let base_meta: MojangVersionMeta = serde_json::from_value(base_json.clone())?;
    Ok((entry, base_meta, base_json))
}

async fn fetch_loader_version_json(profile: &Profile) -> anyhow::Result<Option<Value>> {
    match profile.loader {
        LoaderType::Vanilla => Ok(None),
        LoaderType::Fabric => {
            let loader_version = profile
                .loader_version
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| anyhow!("Fabric loader version is not selected"))?;
            let url = format!(
                "https://meta.fabricmc.net/v2/versions/loader/{}/{}/profile/json",
                profile.minecraft_version, loader_version
            );
            let value: Value = http_json(&url).await?;
            Ok(Some(value))
        }
        LoaderType::Forge => {
            let loader_version = profile
                .loader_version
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| anyhow!("Forge version is not selected"))?;
            let installer_url = format!(
                "https://maven.minecraftforge.net/net/minecraftforge/forge/{0}/forge-{0}-installer.jar",
                loader_version
            );
            let installer_path = app_root()
                .join("cache")
                .join("forge")
                .join(format!("forge-{loader_version}-installer.jar"));
            download_to_file(&installer_url, &installer_path).await?;
            let file = File::open(&installer_path)?;
            let mut zip = ZipArchive::new(file)?;
            if let Ok(mut entry) = zip.by_name("version.json") {
                let mut bytes = Vec::new();
                std::io::copy(&mut entry, &mut bytes)?;
                let value: Value = serde_json::from_slice(&bytes)?;
                return Ok(Some(value));
            }
            if let Ok(mut entry) = zip.by_name("install_profile.json") {
                let mut bytes = Vec::new();
                std::io::copy(&mut entry, &mut bytes)?;
                let profile: ForgeInstallerProfile = serde_json::from_slice(&bytes)?;
                if let Some(version_info) = profile.version_info {
                    return Ok(Some(version_info));
                }
            }
            Err(anyhow!("Forge installer does not contain version metadata"))
        }
    }
}

async fn download_libraries_for_meta(
    profile_id: &str,
    meta: &LaunchVersionMeta,
    app: &tauri::AppHandle,
    step: u32,
    total_steps: u32,
    downloaded_files: &mut Vec<String>,
) -> Result<(), String> {
    let mut tasks = Vec::new();
    let mut natives_to_extract = Vec::new();

    for library in &meta.libraries {
        if !library_allowed(library) {
            continue;
        }
        if let Some((url, rel_path)) = resolve_library_artifact_url_path(library) {
            let dst = instance_root(profile_id).join("libraries").join(rel_path);
            if !dst.exists() {
                let url_clone = url.clone();
                let dst_clone = dst.clone();
                tasks.push(tauri::async_runtime::spawn(async move {
                    download_to_file(&url_clone, &dst_clone)
                        .await
                        .map(|_| dst_clone)
                        .map_err(|e| e.to_string())
                }));
            } else {
                downloaded_files.push(dst.to_string_lossy().to_string());
            }
        }

        if let Some(native_artifact) = resolve_native_artifact(library) {
            let native_jar = instance_root(profile_id)
                .join("libraries")
                .join(&native_artifact.path);
            if !native_jar.exists() {
                let native_url = native_artifact.url.clone();
                let native_dst = native_jar.clone();
                tasks.push(tauri::async_runtime::spawn(async move {
                    download_to_file(&native_url, &native_dst)
                        .await
                        .map(|_| native_dst)
                        .map_err(|e| e.to_string())
                }));
            } else {
                downloaded_files.push(native_jar.to_string_lossy().to_string());
            }
            natives_to_extract.push(native_jar);
        }
    }

    let total_jobs = tasks.len().max(1);
    for (index, task) in tasks.into_iter().enumerate() {
        let path = task.await.map_err(|e| e.to_string())??;
        downloaded_files.push(path.to_string_lossy().to_string());
        let progress = 0.15 + 0.85 * ((index + 1) as f32 / total_jobs as f32);
        emit_install_progress_precise(
            app,
            step,
            total_steps,
            progress,
            format!("Downloading libraries... {}/{}", index + 1, total_jobs),
        )?;
    }

    let natives_dir = instance_root(profile_id).join("natives");
    for native_jar in natives_to_extract {
        if native_jar.exists() {
            extract_native_jar(&native_jar, &natives_dir).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

async fn download_assets_parallel(
    profile_id: &str,
    asset_index: &MojangAssetIndex,
    app: &tauri::AppHandle,
    step: u32,
    total_steps: u32,
    downloaded_files: &mut Vec<String>,
) -> Result<(), String> {
    let index_path = instance_root(profile_id)
        .join("assets")
        .join("indexes")
        .join(format!("{}.json", asset_index.id));
    download_to_file(&asset_index.url, &index_path)
        .await
        .map_err(|e| e.to_string())?;
    downloaded_files.push(index_path.to_string_lossy().to_string());

    let index_bytes = fs::read(&index_path).map_err(|e| e.to_string())?;
    let index: MojangAssetIndexFile =
        serde_json::from_slice(&index_bytes).map_err(|e| e.to_string())?;

    let mut queue = Vec::new();
    for object in index.objects.values() {
        let sub = &object.hash[..2];
        let dst = instance_root(profile_id)
            .join("assets")
            .join("objects")
            .join(sub)
            .join(&object.hash);
        if dst.exists() {
            continue;
        }
        let url = format!(
            "https://resources.download.minecraft.net/{}/{}",
            sub, object.hash
        );
        queue.push((url, dst));
    }

    if queue.is_empty() {
        emit_install_progress_precise(app, step, total_steps, 1.0, "Assets already cached.")?;
        return Ok(());
    }

    const CHUNK: usize = 24;
    let total_jobs = queue.len();
    let mut done = 0usize;

    for batch in queue.chunks(CHUNK) {
        let mut tasks = Vec::new();
        for (url, dst) in batch {
            let url_clone = url.clone();
            let dst_clone = dst.clone();
            tasks.push(tauri::async_runtime::spawn(async move {
                download_to_file(&url_clone, &dst_clone)
                    .await
                    .map(|_| dst_clone)
                    .map_err(|e| e.to_string())
            }));
        }
        for task in tasks {
            let path = task.await.map_err(|e| e.to_string())??;
            downloaded_files.push(path.to_string_lossy().to_string());
            done += 1;
        }
        emit_install_progress_precise(
            app,
            step,
            total_steps,
            done as f32 / total_jobs as f32,
            format!("Downloading assets... {done}/{total_jobs}"),
        )?;
    }

    Ok(())
}

fn emit_install_progress_precise(
    app: &tauri::AppHandle,
    step: u32,
    total: u32,
    phase_fraction: f32,
    message: impl Into<String>,
) -> Result<(), String> {
    let total = total.max(1);
    let step = step.clamp(1, total);
    let base = (step.saturating_sub(1)) as f32 / total as f32;
    let local = phase_fraction.clamp(0.0, 1.0) / total as f32;
    let percent = ((base + local) * 100.0).round() as u8;
    app.emit(
        "install/progress",
        InstallProgress {
            step,
            total,
            percent,
            message: message.into(),
        },
    )
    .map_err(|e| e.to_string())
}

fn library_rule_matches(rule: &MojangLibraryRule) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            if name != current_minecraft_os() {
                return false;
            }
        }
        if let Some(arch) = &os.arch {
            if arch != current_minecraft_arch() {
                return false;
            }
        }
    }
    if let Some(features) = &rule.features {
        for (name, expected) in features {
            let current = match name.as_str() {
                "is_demo_user" => false,
                "has_custom_resolution" => false,
                "has_quick_plays_support" => false,
                "is_quick_play_singleplayer" => false,
                "is_quick_play_multiplayer" => false,
                "is_quick_play_realms" => false,
                _ => false,
            };
            if current != *expected {
                return false;
            }
        }
    }
    true
}

fn library_allowed(lib: &MojangLibrary) -> bool {
    let Some(rules) = &lib.rules else {
        return true;
    };
    let mut allowed = false;
    for rule in rules {
        if library_rule_matches(rule) {
            allowed = rule.action.eq_ignore_ascii_case("allow");
        }
    }
    allowed
}

fn resolve_library_artifact_url_path(lib: &MojangLibrary) -> Option<(String, String)> {
    if let Some(artifact) = lib.downloads.as_ref().and_then(|d| d.artifact.as_ref()) {
        return Some((artifact.url.clone(), artifact.path.clone()));
    }
    if let Some(name) = &lib.name {
        let path = maven_library_path(name)?;
        let base = lib
            .url
            .clone()
            .unwrap_or_else(|| "https://libraries.minecraft.net/".to_string());
        let prefix = if base.ends_with('/') {
            base
        } else {
            format!("{base}/")
        };
        return Some((format!("{prefix}{path}"), path));
    }
    None
}

fn resolve_library_relative_path(lib: &MojangLibrary) -> Option<String> {
    if let Some(artifact) = lib.downloads.as_ref().and_then(|d| d.artifact.as_ref()) {
        return Some(artifact.path.clone());
    }
    lib.name.as_ref().and_then(|name| maven_library_path(name))
}

fn resolve_native_artifact(lib: &MojangLibrary) -> Option<MojangLibraryArtifact> {
    let downloads = lib.downloads.as_ref()?;
    let classifiers = downloads.classifiers.as_ref()?;
    if let Some(natives) = &lib.natives {
        if let Some(template) = natives.get(current_minecraft_os()) {
            let key = template
                .replace("${arch}", current_minecraft_arch())
                .replace("${os}", current_minecraft_os());
            if let Some(found) = classifiers.get(&key) {
                return Some(found.clone());
            }
        }
    }
    if let Some(found) = classifiers.get("natives-windows") {
        return Some(found.clone());
    }
    if let Some(found) = classifiers.get("natives-windows-64") {
        return Some(found.clone());
    }
    classifiers
        .iter()
        .find(|(k, _)| k.starts_with("natives-windows"))
        .map(|(_, v)| v.clone())
}

fn extract_native_jar(jar_path: &Path, natives_dir: &Path) -> anyhow::Result<()> {
    let file = File::open(jar_path)?;
    let mut zip = ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().replace('\\', "/");
        if name.ends_with('/') || name.starts_with("META-INF/") {
            continue;
        }
        let out_name = Path::new(&name)
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| anyhow!("invalid native entry name"))?;
        let out_path = natives_dir.join(out_name);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(out_path)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

fn collect_jars_recursive(root: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jars_recursive(&path, out)?;
        } else if path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn has_native_binaries(dir: &Path) -> bool {
    if !dir.exists() {
        return false;
    }
    let mut files = Vec::new();
    if collect_jars_recursive(dir, &mut files).is_ok() && !files.is_empty() {
        return true;
    }
    fs::read_dir(dir)
        .ok()
        .map(|iter| {
            iter.filter_map(|x| x.ok()).any(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    return has_native_binaries(&path);
                }
                path.extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|ext| {
                        matches!(ext.to_ascii_lowercase().as_str(), "dll" | "so" | "dylib")
                    })
            })
        })
        .unwrap_or(false)
}

fn restore_natives_from_library_cache(profile_id: &str) -> anyhow::Result<()> {
    let libraries_dir = instance_root(profile_id).join("libraries");
    let natives_dir = instance_root(profile_id).join("natives");
    if natives_dir.exists() {
        fs::remove_dir_all(&natives_dir)?;
    }
    fs::create_dir_all(&natives_dir)?;

    let mut jars = Vec::new();
    collect_jars_recursive(&libraries_dir, &mut jars)?;
    for jar in jars {
        let name = jar
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.contains("natives") {
            extract_native_jar(&jar, &natives_dir)?;
        }
    }
    Ok(())
}

fn version_meta_path(profile: &Profile) -> PathBuf {
    instance_root(&profile.id)
        .join("versions")
        .join(&profile.minecraft_version)
        .join(format!("{}.json", profile.minecraft_version))
}

fn load_launch_meta(profile: &Profile) -> anyhow::Result<LaunchVersionMeta> {
    let bytes = fs::read(version_meta_path(profile))?;
    Ok(serde_json::from_slice::<LaunchVersionMeta>(&bytes)?)
}

fn required_java_major_from_installed_meta(profile: &Profile) -> Option<u8> {
    let meta = load_launch_meta(profile).ok()?;
    meta.java_version.map(|j| j.major_version)
}

fn replace_placeholders(input: &str, values: &HashMap<&str, String>) -> String {
    let mut out = input.to_string();
    for (k, v) in values {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

fn expand_launch_argument_items(
    items: &[LaunchArgumentItem],
    values: &HashMap<&str, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        match item {
            LaunchArgumentItem::String(v) => out.push(replace_placeholders(v, values)),
            LaunchArgumentItem::Conditional { rules, value } => {
                let allowed = rules
                    .as_ref()
                    .map(|r| {
                        let mut state = false;
                        for rule in r {
                            if library_rule_matches(rule) {
                                state = rule.action.eq_ignore_ascii_case("allow");
                            }
                        }
                        state
                    })
                    .unwrap_or(true);
                if !allowed {
                    continue;
                }
                match value {
                    LaunchArgumentValue::String(v) => {
                        out.push(replace_placeholders(v, values));
                    }
                    LaunchArgumentValue::Array(arr) => {
                        out.extend(arr.iter().map(|v| replace_placeholders(v, values)));
                    }
                }
            }
        }
    }
    out
}

fn legacy_game_arguments(raw: &str, values: &HashMap<&str, String>) -> Vec<String> {
    raw.split_whitespace()
        .map(|token| replace_placeholders(token, values))
        .collect()
}

fn contains_any_arg(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|arg| names.iter().any(|name| arg == name))
}

fn build_classpath_from_meta(
    profile: &Profile,
    meta: &LaunchVersionMeta,
    client_jar: &Path,
) -> String {
    let libraries_dir = instance_root(&profile.id).join("libraries");
    let mut entries = Vec::new();
    for library in &meta.libraries {
        if !library_allowed(library) {
            continue;
        }
        if let Some(rel) = resolve_library_relative_path(library) {
            let path = libraries_dir.join(rel);
            if path.exists() {
                entries.push(path.to_string_lossy().to_string());
            }
        }
    }
    entries.push(client_jar.to_string_lossy().to_string());
    entries.join(";")
}

#[tauri::command]
async fn profile_install(
    app: tauri::AppHandle,
    profile_id: String,
) -> Result<InstallReport, String> {
    const INSTALL_STEPS: u32 = 8;
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    ensure_profile_dirs(&profile.id).map_err(|e| e.to_string())?;
    let natives_dir = instance_root(&profile.id).join("natives");
    if natives_dir.exists() {
        fs::remove_dir_all(&natives_dir).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&natives_dir).map_err(|e| e.to_string())?;
    emit_install_progress(&app, 1, INSTALL_STEPS, "Loading Minecraft metadata...")?;
    let (version, base_meta, base_json) = fetch_vanilla_entry_and_meta(&profile.minecraft_version)
        .await
        .map_err(|e| e.to_string())?;
    emit_install_progress(
        &app,
        2,
        INSTALL_STEPS,
        format!(
            "Preparing {} {}...",
            match profile.loader {
                LoaderType::Vanilla => "vanilla",
                LoaderType::Fabric => "fabric",
                LoaderType::Forge => "forge",
            },
            version.id
        ),
    )?;
    let merged_json = if let Some(loader_json) = fetch_loader_version_json(&profile)
        .await
        .map_err(|e| e.to_string())?
    {
        merge_version_values(base_json, loader_json).map_err(|e| e.to_string())?
    } else {
        base_json
    };
    let launch_meta: LaunchVersionMeta =
        serde_json::from_value(merged_json.clone()).map_err(|e| e.to_string())?;
    let version_dir = instance_root(&profile.id)
        .join("versions")
        .join(&profile.minecraft_version);
    let client_jar = version_dir.join(format!("{}.jar", profile.minecraft_version));
    let meta_json = version_dir.join(format!("{}.json", profile.minecraft_version));
    emit_install_progress(&app, 3, INSTALL_STEPS, "Downloading client.jar...")?;
    download_to_file(&base_meta.downloads.client.url, &client_jar)
        .await
        .map_err(|e| e.to_string())?;
    emit_install_progress(&app, 4, INSTALL_STEPS, "Saving version metadata...")?;
    write_json_value(&meta_json, &merged_json).map_err(|e| e.to_string())?;
    let mut downloaded_files = vec![
        client_jar.to_string_lossy().to_string(),
        meta_json.to_string_lossy().to_string(),
    ];

    emit_install_progress(
        &app,
        5,
        INSTALL_STEPS,
        "Downloading libraries and natives...",
    )?;
    download_libraries_for_meta(
        &profile.id,
        &launch_meta,
        &app,
        5,
        INSTALL_STEPS,
        &mut downloaded_files,
    )
    .await?;
    restore_natives_from_library_cache(&profile.id).map_err(|e| e.to_string())?;

    if let Some(asset_index) = &base_meta.asset_index {
        emit_install_progress(&app, 6, INSTALL_STEPS, "Downloading assets...")?;
        download_assets_parallel(
            &profile.id,
            asset_index,
            &app,
            6,
            INSTALL_STEPS,
            &mut downloaded_files,
        )
        .await?;
    }

    emit_install_progress(&app, 7, INSTALL_STEPS, "Verifying downloaded files...")?;
    if let Some(expected_sha1) = base_meta.downloads.client.sha1 {
        let bytes = fs::read(&client_jar).map_err(|e| e.to_string())?;
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected_sha1 {
            app.emit(
                "download/error",
                "Client.jar checksum mismatch. File can be corrupted.",
            )
            .map_err(|e| e.to_string())?;
        }
    }
    let mut warnings = Vec::new();
    if profile.loader == LoaderType::Forge {
        warnings.push(
            "Forge metadata is installed through a lightweight flow. If a specific pack fails, send the launch error and I will tighten the installer path."
                .to_string(),
        );
    }
    emit_install_progress(&app, 8, INSTALL_STEPS, "Install completed.")?;
    Ok(InstallReport {
        success: true,
        downloaded_files,
        warnings,
    })
}

#[tauri::command]
fn profile_launch(app: tauri::AppHandle, profile_id: String) -> Result<LaunchHandle, String> {
    let profile = load_profile(&profile_id).map_err(|e| e.to_string())?;
    let validation = profile_validate(profile.id.clone())?;
    if !validation.can_launch {
        return Err("Profile validation failed. Check diagnostics.".to_string());
    }

    let java = resolve_java_for_profile(&profile);
    let raw_java_path = java
        .resolved_path
        .ok_or_else(|| "Java runtime for launch was not found".to_string())?;
    let java_path = to_java_console_path(&raw_java_path);
    if let Some(found_major) = detect_java_major(&java_path) {
        if found_major < java.required_major {
            return Err(format!(
                "Java {} required for Minecraft {}, but found Java {} ({})",
                java.required_major, profile.minecraft_version, found_major, java_path
            ));
        }
    }
    let java_bits = detect_java_bitness(&java_path);
    let ram_mb = effective_ram_mb(profile.ram_mb, java_bits);

    let launch_meta = load_launch_meta(&profile).map_err(|e| e.to_string())?;
    let version_dir = instance_root(&profile.id)
        .join("versions")
        .join(&profile.minecraft_version);
    let client_jar = version_dir.join(format!("{}.jar", profile.minecraft_version));
    let libraries_dir = instance_root(&profile.id).join("libraries");
    let game_dir = instance_root(&profile.id);
    let assets_dir = game_dir.join("assets");
    let natives_dir = game_dir.join("natives");
    let uuid = offline_uuid(&profile.nickname);
    let access_token = "offline-token".to_string();
    let legacy_session = format!("token:{access_token}:{uuid}");
    let classpath = build_classpath_from_meta(&profile, &launch_meta, &client_jar);
    let asset_index_name = launch_meta
        .asset_index
        .as_ref()
        .map(|x| x.id.clone())
        .unwrap_or_else(|| profile.minecraft_version.clone());
    let version_type = launch_meta
        .version_type
        .clone()
        .unwrap_or_else(|| "release".to_string());

    let mut replacements: HashMap<&str, String> = HashMap::new();
    replacements.insert("auth_player_name", profile.nickname.clone());
    replacements.insert("auth_username", profile.nickname.clone());
    replacements.insert("profile_name", profile.nickname.clone());
    replacements.insert("user_name", profile.nickname.clone());
    replacements.insert("username", profile.nickname.clone());
    replacements.insert("version_name", launch_meta.id.clone());
    replacements.insert("game_directory", game_dir.to_string_lossy().to_string());
    replacements.insert("assets_root", assets_dir.to_string_lossy().to_string());
    replacements.insert("assets_index_name", asset_index_name.clone());
    replacements.insert("auth_uuid", uuid.clone());
    replacements.insert("auth_access_token", access_token.clone());
    replacements.insert("user_type", "legacy".to_string());
    replacements.insert("version_type", version_type.clone());
    replacements.insert("auth_session", legacy_session.clone());
    replacements.insert("user_properties", "{}".to_string());
    replacements.insert("game_assets", assets_dir.to_string_lossy().to_string());
    replacements.insert(
        "natives_directory",
        natives_dir.to_string_lossy().to_string(),
    );
    replacements.insert("launcher_name", "RoundStudioLauncher".to_string());
    replacements.insert("launcher_version", "0.1.0".to_string());
    replacements.insert(
        "library_directory",
        libraries_dir.to_string_lossy().to_string(),
    );
    replacements.insert("classpath_separator", ";".to_string());
    replacements.insert("classpath", classpath.clone());
    replacements.insert("clientid", "".to_string());
    replacements.insert("client_id", "".to_string());
    replacements.insert("xuid", "".to_string());
    replacements.insert("auth_xuid", "".to_string());
    replacements.insert("resolution_width", "854".to_string());
    replacements.insert("resolution_height", "480".to_string());

    let mut args = vec![format!("-Xms512M"), format!("-Xmx{}M", ram_mb)];
    if let Some(arguments) = &launch_meta.arguments {
        let mut jvm = expand_launch_argument_items(&arguments.jvm, &replacements);
        if !jvm.iter().any(|v| v == "-cp") {
            jvm.push("-cp".to_string());
            jvm.push(classpath.clone());
        }
        if !jvm.iter().any(|v| v.starts_with("-Djava.library.path=")) {
            jvm.push(format!(
                "-Djava.library.path={}",
                natives_dir.to_string_lossy()
            ));
        }
        args.extend(jvm);
    } else {
        args.push(format!(
            "-Djava.library.path={}",
            natives_dir.to_string_lossy()
        ));
        args.push("-cp".to_string());
        args.push(classpath.clone());
    }
    args.extend(sanitized_jvm_args(&profile.jvm_args));
    args.push(launch_meta.main_class.clone());
    if let Some(arguments) = &launch_meta.arguments {
        let mut game_args = expand_launch_argument_items(&arguments.game, &replacements);
        if !contains_any_arg(&game_args, &["--username", "--user", "--userName"]) {
            game_args.push("--username".to_string());
            game_args.push(profile.nickname.clone());
        }
        if !contains_any_arg(&game_args, &["--version"]) {
            game_args.push("--version".to_string());
            game_args.push(launch_meta.id.clone());
        }
        if !contains_any_arg(&game_args, &["--gameDir"]) {
            game_args.push("--gameDir".to_string());
            game_args.push(game_dir.to_string_lossy().to_string());
        }
        if !contains_any_arg(&game_args, &["--assetsDir"]) {
            game_args.push("--assetsDir".to_string());
            game_args.push(assets_dir.to_string_lossy().to_string());
        }
        if !contains_any_arg(&game_args, &["--assetIndex"]) {
            game_args.push("--assetIndex".to_string());
            game_args.push(asset_index_name.clone());
        }
        if !contains_any_arg(&game_args, &["--uuid"]) {
            game_args.push("--uuid".to_string());
            game_args.push(uuid.clone());
        }
        if !contains_any_arg(&game_args, &["--accessToken"]) {
            game_args.push("--accessToken".to_string());
            game_args.push(access_token.clone());
        }
        if !contains_any_arg(&game_args, &["--userType"]) {
            game_args.push("--userType".to_string());
            game_args.push("legacy".to_string());
        }
        if !contains_any_arg(&game_args, &["--versionType"]) {
            game_args.push("--versionType".to_string());
            game_args.push(version_type.clone());
        }
        args.extend(game_args);
    } else if let Some(raw) = &launch_meta.minecraft_arguments {
        let mut game_args = legacy_game_arguments(raw, &replacements);
        if !contains_any_arg(&game_args, &["--username", "--user", "--userName"]) {
            game_args.push("--username".to_string());
            game_args.push(profile.nickname.clone());
        }
        if !contains_any_arg(&game_args, &["--session"]) {
            game_args.push("--session".to_string());
            game_args.push(legacy_session.clone());
        }
        args.extend(game_args);
    } else {
        args.extend([
            "--username".to_string(),
            profile.nickname.clone(),
            "--version".to_string(),
            launch_meta.id.clone(),
            "--gameDir".to_string(),
            game_dir.to_string_lossy().to_string(),
            "--assetsDir".to_string(),
            assets_dir.to_string_lossy().to_string(),
            "--assetIndex".to_string(),
            asset_index_name,
            "--uuid".to_string(),
            uuid,
            "--accessToken".to_string(),
            access_token,
            "--session".to_string(),
            legacy_session,
            "--userType".to_string(),
            "legacy".to_string(),
            "--versionType".to_string(),
            version_type,
        ]);
    }

    if ram_mb != profile.ram_mb {
        let _ = app.emit(
            "launch/log",
            format!(
                "[launcher] RAM adjusted from {}M to {}M for detected Java {:?}-bit",
                profile.ram_mb, ram_mb, java_bits
            ),
        );
    }

    let mut cmd = Command::new(java_path);
    cmd.args(args);
    cmd.current_dir(game_dir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    hide_command_window(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let id = HANDLE_SEQ.fetch_add(1, Ordering::Relaxed);
    {
        let mut table = process_table().map_err(|e| e.to_string())?;
        table.insert(id, child);
    }

    app.emit("launch/state", format!("started:{id}"))
        .map_err(|e| e.to_string())?;
    spawn_process_watcher(app.clone(), id);

    if let Some(out) = stdout {
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            let reader = BufReader::new(out);
            for line in reader.lines().map_while(Result::ok) {
                let _ = app_clone.emit("launch/log", format!("[stdout] {line}"));
            }
        });
    }
    if let Some(err) = stderr {
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            let reader = BufReader::new(err);
            for line in reader.lines().map_while(Result::ok) {
                let _ = app_clone.emit("launch/log", format!("[stderr] {line}"));
            }
        });
    }

    Ok(LaunchHandle { id })
}

#[tauri::command]
fn launch_stop(app: tauri::AppHandle, handle_id: u64) -> Result<(), String> {
    let mut table = process_table().map_err(|e| e.to_string())?;
    let mut child = table
        .remove(&handle_id)
        .ok_or_else(|| "Launch process not found".to_string())?;
    child.kill().map_err(|e| e.to_string())?;
    app.emit("launch/state", format!("stopped:{handle_id}"))
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            fs::create_dir_all(app_root()).context("create app root")?;
            fs::create_dir_all(profiles_root()).context("create profiles root")?;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(
                    1080.0, 720.0,
                ))));
            }
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            profiles_list,
            profiles_create,
            profiles_get,
            profiles_clone,
            profiles_update,
            profiles_delete,
            versions_list_minecraft,
            loaders_list,
            loaders_for_version,
            java_resolve,
            open_versions_folder,
            content_import_local,
            content_list,
            content_search_modrinth,
            content_install_modrinth,
            skin_set,
            profile_validate,
            profile_install,
            profile_launch,
            launch_stop
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

