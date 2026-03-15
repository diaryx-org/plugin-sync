//! Extism guest plugin wrapping diaryx_sync for on-demand CRDT sync.
//!
//! This crate compiles to a `.wasm` module loaded by the Extism host runtime
//! (wasmtime on native, @extism/extism JS SDK on web). It owns all CRDT state
//! (WorkspaceCrdt, BodyDocManager) in its own WASM sandbox and exposes both
//! JSON-based and binary-native exports.
//!
//! ## JSON exports (standard Extism protocol)
//!
//! - `manifest()` — plugin metadata + UI contributions
//! - `init()` — initialize with workspace config
//! - `shutdown()` — persist state and clean up
//! - `handle_command()` — structured commands (sync state, CRDT ops, etc.)
//! - `on_event()` — filesystem events from the host
//! - `get_config()` / `set_config()` — plugin configuration
//!
//! ## Binary exports (hot path)
//!
//! - `handle_binary_message()` — framed v2 sync message in
//! - `handle_text_message()` — control/handshake messages
//! - `on_connected()` — connection established
//! - `on_disconnected()` — connection lost
//! - `queue_local_update()` — local CRDT change

pub mod binary_protocol;
pub mod host_fs;
#[cfg(not(target_arch = "wasm32"))]
mod native_extism_stubs;
pub mod state;

use diaryx_plugin_sdk::prelude::*;
diaryx_plugin_sdk::register_getrandom_v02!();

use diaryx_core::frontmatter;
use diaryx_core::types::FileMetadata;
use extism_pdk::*;
use serde_json::Value as JsonValue;
use std::collections::VecDeque;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

use diaryx_core::plugin::{
    ComponentRef, SettingsField, SidebarSide, StatusBarPosition, UiContribution,
};
use diaryx_sync::{IncomingEvent, SessionAction};

// ============================================================================
// HTTP compat helpers (adapt SDK's typed HttpResponse to old JsonValue API)
// ============================================================================

fn http_request_compat(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body_json: Option<JsonValue>,
) -> Result<JsonValue, String> {
    let header_map: std::collections::HashMap<String, String> =
        headers.iter().cloned().collect();
    let body_str = body_json.map(|b| b.to_string());
    let resp = host::http::request(method, url, &header_map, body_str.as_deref())?;
    let mut result = serde_json::json!({
        "status": resp.status,
        "body": resp.body,
    });
    if let Some(b64) = &resp.body_base64 {
        result["body_base64"] = JsonValue::String(b64.clone());
    }
    Ok(result)
}

fn http_request_binary_compat(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<JsonValue, String> {
    let header_map: std::collections::HashMap<String, String> =
        headers.iter().cloned().collect();
    let resp = host::http::request_binary(method, url, &header_map, body)?;
    let mut result = serde_json::json!({
        "status": resp.status,
        "body": resp.body,
    });
    if let Some(b64) = &resp.body_base64 {
        result["body_base64"] = JsonValue::String(b64.clone());
    }
    Ok(result)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InitParams {
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    write_to_disk: Option<bool>,
    #[serde(default)]
    server_url: Option<String>,
    #[serde(default)]
    auth_token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct SyncExtismConfig {
    #[serde(default)]
    server_url: Option<String>,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
}

fn normalize_server_base(server_url: &str) -> String {
    let mut base = server_url.trim().trim_end_matches('/').to_string();
    loop {
        if let Some(stripped) = base.strip_suffix("/sync2") {
            base = stripped.trim_end_matches('/').to_string();
            continue;
        }
        if let Some(stripped) = base.strip_suffix("/sync") {
            base = stripped.trim_end_matches('/').to_string();
            continue;
        }
        break;
    }
    base
}

fn load_extism_config() -> SyncExtismConfig {
    match host::storage::get("sync.extism.config") {
        Ok(Some(bytes)) => serde_json::from_slice::<SyncExtismConfig>(&bytes).unwrap_or_default(),
        _ => SyncExtismConfig::default(),
    }
}

fn save_extism_config(config: &SyncExtismConfig) {
    if let Ok(bytes) = serde_json::to_vec(config) {
        let _ = host::storage::set("sync.extism.config", &bytes);
    }
}

fn command_param_str(params: &JsonValue, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn command_param_bool(params: &JsonValue, key: &str) -> Option<bool> {
    params.get(key).and_then(|v| v.as_bool())
}

fn apply_config_patch(config: &mut SyncExtismConfig, incoming: &JsonValue) {
    apply_config_string(config, incoming, "server_url", |cfg, value| {
        cfg.server_url = value
    });
    apply_config_string(config, incoming, "auth_token", |cfg, value| {
        cfg.auth_token = value
    });
    apply_config_string(config, incoming, "workspace_id", |cfg, value| {
        cfg.workspace_id = value
    });
}

fn apply_config_string<F>(config: &mut SyncExtismConfig, incoming: &JsonValue, key: &str, set: F)
where
    F: FnOnce(&mut SyncExtismConfig, Option<String>),
{
    if let Some(raw) = incoming.get(key) {
        if raw.is_null() {
            set(config, None);
        } else if let Some(value) = raw.as_str() {
            let normalized = value.trim();
            if normalized.is_empty() {
                set(config, None);
            } else {
                set(config, Some(normalized.to_string()));
            }
        }
    }
}

fn resolve_server_url(params: &JsonValue, config: &SyncExtismConfig) -> Option<String> {
    command_param_str(params, "server_url")
        .or_else(|| config.server_url.clone())
        .or_else(|| runtime_context_string("server_url"))
        .map(|s| normalize_server_base(&s))
}

fn resolve_auth_token(params: &JsonValue, config: &SyncExtismConfig) -> Option<String> {
    command_param_str(params, "auth_token")
        .or_else(|| config.auth_token.clone())
        .or_else(|| runtime_context_string("auth_token"))
}

fn runtime_context_string(key: &str) -> Option<String> {
    host::context::get()
        .ok()
        .and_then(|runtime| {
            runtime
                .get(key)
                .and_then(|value| value.as_str())
                .map(str::trim)
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty())
}

fn sync_transport_connect_if_ready(
    config: &SyncExtismConfig,
    write_to_disk: Option<bool>,
) -> Result<(), String> {
    let Some(server_url) = resolve_server_url(&JsonValue::Null, config) else {
        host::ws::disconnect()?;
        return Ok(());
    };
    let Some(workspace_id) = config.workspace_id.as_deref() else {
        host::ws::disconnect()?;
        return Ok(());
    };
    if workspace_id.trim().is_empty() {
        host::ws::disconnect()?;
        return Ok(());
    }

    let auth_token =
        resolve_auth_token(&JsonValue::Null, config).filter(|token| !token.trim().is_empty());
    if auth_token.is_none() {
        host::ws::disconnect()?;
        return Ok(());
    }

    host::ws::connect(
        &server_url,
        workspace_id,
        auth_token.as_deref(),
        None,
        write_to_disk,
    )
}

fn reconcile_sync_transport(write_to_disk: Option<bool>) {
    let config = load_extism_config();
    let resolved_write_to_disk = write_to_disk.or_else(|| state::get_write_to_disk().ok());
    if let Err(e) = sync_transport_connect_if_ready(&config, resolved_write_to_disk) {
        host::log::log("warn", &format!("[sync_transport] {e}"));
    }
}

fn with_session_actions<F>(label: &str, f: F) -> Vec<SessionAction>
where
    F: FnOnce(&mut diaryx_sync::SyncSession<crate::host_fs::HostFs>) -> Vec<SessionAction>,
{
    state::with_session_mut(f)
        .map(|actions| actions.unwrap_or_default())
        .unwrap_or_else(|e| {
            host::log::log("warn", &format!("[{label}] {e}"));
            vec![]
        })
}

fn execute_session_actions(actions: Vec<SessionAction>) {
    let mut queue: VecDeque<SessionAction> = actions.into();

    loop {
        while let Some(action) = queue.pop_front() {
            match action {
                SessionAction::SendBinary(data) => {
                    if let Err(e) = host::ws::send_binary(&data) {
                        host::log::log(
                            "warn",
                            &format!("[sync_transport:send_binary] {e}"),
                        );
                    }
                }
                SessionAction::SendText(text) => {
                    if let Err(e) = host::ws::send_text(&text) {
                        host::log::log(
                            "warn",
                            &format!("[sync_transport:send_text] {e}"),
                        );
                    }
                }
                SessionAction::Emit(event) => state::emit_sync_event(&event),
                SessionAction::DownloadSnapshot { workspace_id } => {
                    let follow_up = match handle_download_workspace(&serde_json::json!({
                        "remote_id": workspace_id,
                        "include_attachments": true,
                        "link": false,
                    })) {
                        Ok(_) => with_session_actions("snapshot_imported", |session| {
                            poll_future(session.process(IncomingEvent::SnapshotImported))
                        }),
                        Err(e) => {
                            host::log::log("warn", &format!("[snapshot_download] {e}"));
                            vec![]
                        }
                    };
                    queue.extend(follow_up);
                }
            }
        }

        let local_updates = state::drain_local_updates();
        if local_updates.is_empty() {
            break;
        }

        let follow_up = with_session_actions("local_update", |session| {
            let mut actions = Vec::new();
            for (doc_id, data) in local_updates {
                actions.extend(poll_future(
                    session.process(IncomingEvent::LocalUpdate { doc_id, data }),
                ));
            }
            actions
        });
        queue.extend(follow_up);
    }
}

fn http_error(status: u64, body: &str) -> String {
    if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    }
}

fn sync_status_from_state() -> JsonValue {
    let config = load_extism_config();
    let has_linked_workspace = config
        .workspace_id
        .as_deref()
        .map(|workspace_id| !workspace_id.trim().is_empty())
        .unwrap_or(false);
    let (state, label) = match (has_linked_workspace, state::has_session()) {
        (true, Ok(true)) => ("syncing", "Syncing"),
        _ => ("idle", "Idle"),
    };
    serde_json::json!({
        "state": state,
        "label": label,
        "detail": JsonValue::Null,
        "progress": JsonValue::Null
    })
}

fn get_component_html_by_id(component_id: &str) -> Option<&'static str> {
    match component_id {
        "sync.snapshots" => Some(include_str!("ui/snapshots.html")),
        "sync.history" => Some(include_str!("ui/history.html")),
        _ => None,
    }
}

fn auth_headers(auth_token: Option<String>) -> Vec<(String, String)> {
    match auth_token {
        Some(token) if !token.trim().is_empty() => vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {}", token)),
        ],
        _ => vec![("Content-Type".to_string(), "application/json".to_string())],
    }
}

fn parse_http_status(response: &JsonValue) -> u64 {
    response.get("status").and_then(|v| v.as_u64()).unwrap_or(0)
}

fn parse_http_body(response: &JsonValue) -> String {
    response
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn parse_http_body_json(response: &JsonValue) -> Option<JsonValue> {
    let body = parse_http_body(response);
    if body.is_empty() {
        return None;
    }
    serde_json::from_str(&body).ok()
}

fn parse_http_body_bytes(response: &JsonValue) -> Result<Vec<u8>, String> {
    if let Some(body_b64) = response.get("body_base64").and_then(|v| v.as_str()) {
        if body_b64.is_empty() {
            return Ok(Vec::new());
        }
        use base64::Engine;
        return base64::engine::general_purpose::STANDARD
            .decode(body_b64)
            .map_err(|e| format!("Invalid HTTP response body_base64: {e}"));
    }
    Ok(parse_http_body(response).into_bytes())
}

fn normalize_snapshot_entry_path(path: &str) -> Option<String> {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        None
    } else {
        Some(normalized.to_string_lossy().replace('\\', "/"))
    }
}

fn should_skip_snapshot_entry(path: &str) -> bool {
    Path::new(path).components().any(|component| {
        let Component::Normal(part) = component else {
            return false;
        };
        let part = part.to_string_lossy();
        part.starts_with('.')
            || part == "__MACOSX"
            || part == "Thumbs.db"
            || part == "desktop.ini"
            || part.starts_with("._")
    })
}

/// Soft-delete a file in the workspace CRDT without using `mark_deleted()`,
/// which calls `chrono::Utc::now()` and panics in the Extism WASM sandbox.
fn soft_delete_file(ws: &diaryx_sync::WorkspaceCrdt, path: &str) -> bool {
    if let Some(mut meta) = ws.get_file(path) {
        if meta.deleted {
            return false;
        }
        meta.deleted = true;
        meta.modified_at = host::time::timestamp_millis()
            .map(|timestamp| timestamp as i64)
            .unwrap_or(meta.modified_at + 1);
        let _ = ws.set_file(path, meta);
        return true;
    }
    false
}

fn workspace_relative_path_for_sync(
    sync_plugin: &diaryx_sync::SyncPlugin<crate::host_fs::HostFs>,
    path: &str,
) -> String {
    let workspace_root = sync_plugin.sync_handler().get_workspace_root();
    relative_snapshot_path(workspace_root.as_ref().and_then(|root| root.to_str()), path)
        .unwrap_or_else(|| path.to_string())
}

fn parse_workspace_metadata_from_markdown(
    sync_plugin: &diaryx_sync::SyncPlugin<crate::host_fs::HostFs>,
    path: &str,
    content: &str,
) -> Option<(String, FileMetadata)> {
    let relative_path = workspace_relative_path_for_sync(sync_plugin, path);
    diaryx_sync::materialize::parse_snapshot_markdown(&relative_path, content)
        .ok()
        .map(|(metadata, _)| (relative_path, metadata))
}

fn apply_workspace_metadata(
    sync_plugin: &diaryx_sync::SyncPlugin<crate::host_fs::HostFs>,
    relative_path: &str,
    metadata: &FileMetadata,
) -> bool {
    sync_plugin
        .sync_manager()
        .track_metadata(relative_path, metadata);

    let workspace = sync_plugin.workspace_crdt();
    let metadata_changed = workspace
        .get_file(relative_path)
        .map(|current| !current.is_content_equal(metadata))
        .unwrap_or(true);

    if metadata_changed {
        let _ = workspace.set_file(relative_path, metadata.clone());
        return true;
    }

    false
}

fn flush_workspace_metadata_changes(sync_plugin: &diaryx_sync::SyncPlugin<crate::host_fs::HostFs>) {
    sync_plugin.sync_manager().rebuild_uuid_maps();
    let _ = sync_plugin.sync_manager().emit_workspace_update();
}

fn refresh_workspace_metadata_from_disk(
    sync_plugin: &diaryx_sync::SyncPlugin<crate::host_fs::HostFs>,
    relative_path: &str,
) -> bool {
    let workspace_root = sync_plugin.sync_handler().get_workspace_root();
    let host_path = resolve_workspace_path(
        workspace_root.as_ref().and_then(|root| root.to_str()),
        relative_path,
    );
    match host::fs::read_file(&host_path) {
        Ok(content) => parse_workspace_metadata_from_markdown(sync_plugin, &host_path, &content)
            .map(|(relative_path, metadata)| {
                apply_workspace_metadata(sync_plugin, &relative_path, &metadata)
            })
            .unwrap_or(false),
        Err(e) => {
            host::log::log(
                "warn",
                &format!(
                    "[refresh_workspace_metadata_from_disk] read_file FAILED for {}: {}",
                    host_path, e
                ),
            );
            false
        }
    }
}

fn refresh_related_parent_indexes(
    sync_plugin: &diaryx_sync::SyncPlugin<crate::host_fs::HostFs>,
    parent_paths: &[Option<String>],
) -> bool {
    let mut changed = false;
    let mut visited = Vec::new();

    for parent_path in parent_paths.iter().flatten() {
        let trimmed = parent_path.trim();
        if trimmed.is_empty() || visited.iter().any(|seen| seen == trimmed) {
            continue;
        }
        visited.push(trimmed.to_string());
        changed |= refresh_workspace_metadata_from_disk(sync_plugin, trimmed);
    }

    changed
}

fn resolve_workspace_path(workspace_root: Option<&str>, relative_path: &str) -> String {
    let root = workspace_root.map(str::trim).unwrap_or_default();
    if root.is_empty() || root == "." {
        return relative_path.to_string();
    }
    let mut full_path = PathBuf::from(root);
    full_path.push(relative_path);
    full_path.to_string_lossy().replace('\\', "/")
}

fn ensure_parent_dirs_for_binary(path: &str) -> Result<(), String> {
    let Some(parent) = Path::new(path).parent() else {
        return Ok(());
    };
    let parent_str = parent.to_string_lossy();
    if parent_str.is_empty() || parent_str == "." {
        return Ok(());
    }
    let marker_path = format!(
        "{}/.diaryx_sync_tmp_parent",
        parent_str.trim_end_matches('/').trim_end_matches('\\')
    );
    host::fs::write_file(&marker_path, "")?;
    let _ = host::fs::delete_file(&marker_path);
    Ok(())
}

fn relative_snapshot_path(workspace_root: Option<&str>, path: &str) -> Option<String> {
    let mut candidate = path.replace('\\', "/");

    if let Some(root) = workspace_root {
        let normalized_root = root
            .trim()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        if !normalized_root.is_empty() && normalized_root != "." {
            if candidate == normalized_root {
                return None;
            }
            if let Some(stripped) = candidate.strip_prefix(&(normalized_root.clone() + "/")) {
                candidate = stripped.to_string();
            }
        }
    }

    let candidate = candidate
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    if candidate.is_empty() {
        return None;
    }
    normalize_snapshot_entry_path(&candidate)
}

fn build_workspace_snapshot_zip(
    workspace_root: Option<&str>,
    include_attachments: bool,
) -> Result<(Vec<u8>, usize), String> {
    let prefix = workspace_root
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .unwrap_or(".");
    let mut files = host::fs::list_files(prefix)?;
    files.sort();

    let cursor = Cursor::new(Vec::<u8>::new());
    let mut zip = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut files_added = 0usize;

    for file_path in files {
        let Some(relative_path) = relative_snapshot_path(workspace_root, &file_path) else {
            continue;
        };
        if should_skip_snapshot_entry(&relative_path) {
            continue;
        }

        if relative_path.ends_with(".md") {
            let content = host::fs::read_file(&file_path)?;
            zip.start_file(relative_path, options)
                .map_err(|e| format!("Failed to add markdown entry to zip: {e}"))?;
            zip.write_all(content.as_bytes())
                .map_err(|e| format!("Failed to write markdown entry to zip: {e}"))?;
            files_added += 1;
            continue;
        }

        if include_attachments {
            let bytes = host::fs::read_binary(&file_path)?;
            zip.start_file(relative_path, options)
                .map_err(|e| format!("Failed to add binary entry to zip: {e}"))?;
            zip.write_all(&bytes)
                .map_err(|e| format!("Failed to write binary entry to zip: {e}"))?;
            files_added += 1;
        }
    }

    let cursor = zip
        .finish()
        .map_err(|e| format!("Failed to finalize snapshot zip: {e}"))?;
    Ok((cursor.into_inner(), files_added))
}

fn upload_workspace_snapshot(
    params: &JsonValue,
    config: &SyncExtismConfig,
    remote_id: &str,
    workspace_root: Option<&str>,
    mode: &str,
    include_attachments: bool,
) -> Result<usize, String> {
    let server = resolve_server_url(params, config).ok_or("Missing server_url")?;
    let mut headers = auth_headers(resolve_auth_token(params, config));
    headers.push(("Content-Type".to_string(), "application/zip".to_string()));

    let (snapshot_zip, files_added) =
        build_workspace_snapshot_zip(workspace_root, include_attachments)?;
    let response = http_request_binary_compat(
        "POST",
        &format!(
            "{server}/api/workspaces/{remote_id}/snapshot?mode={mode}&include_attachments={include_attachments}"
        ),
        &headers,
        &snapshot_zip,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }

    Ok(files_added)
}

fn provider_supported(params: &JsonValue) -> bool {
    command_param_str(params, "provider_id")
        .map(|id| id == "sync" || id == "diaryx.sync")
        .unwrap_or(true)
}

fn handle_get_provider_status(params: &JsonValue) -> Result<JsonValue, String> {
    if !provider_supported(params) {
        return Ok(serde_json::json!({
            "ready": false,
            "message": "Unsupported provider"
        }));
    }

    let config = load_extism_config();
    let has_server = resolve_server_url(params, &config)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_auth = resolve_auth_token(params, &config)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    if !has_server {
        return Ok(serde_json::json!({
            "ready": false,
            "message": "Sync server URL is not configured"
        }));
    }
    if !has_auth {
        return Ok(serde_json::json!({
            "ready": false,
            "message": "Sign in to enable sync"
        }));
    }

    Ok(serde_json::json!({
        "ready": true,
        "message": JsonValue::Null
    }))
}

fn handle_list_remote_workspaces(params: &JsonValue) -> Result<JsonValue, String> {
    if !provider_supported(params) {
        return Ok(serde_json::json!({ "workspaces": Vec::<JsonValue>::new() }));
    }
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response =
        http_request_compat("GET", &format!("{server}/api/workspaces"), &headers, None)?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    let body = parse_http_body_json(&response).unwrap_or(JsonValue::Array(Vec::new()));
    let workspaces = body
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.to_string();
            let name = value.get("name")?.as_str()?.to_string();
            Some(serde_json::json!({ "id": id, "name": name }))
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "workspaces": workspaces }))
}

fn handle_link_workspace(params: &JsonValue) -> Result<JsonValue, String> {
    if !provider_supported(params) {
        return Err("Unsupported provider".to_string());
    }
    let config = load_extism_config();
    let mut remote_id = command_param_str(params, "remote_id");
    let mut created_remote = false;
    let workspace_root = command_param_str(params, "workspace_root");
    let include_attachments = command_param_bool(params, "include_attachments").unwrap_or(true);

    if remote_id.is_none() {
        let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
        let headers = auth_headers(resolve_auth_token(params, &config));
        let name = command_param_str(params, "name").ok_or("Missing name")?;
        let response = http_request_compat(
            "POST",
            &format!("{server}/api/workspaces"),
            &headers,
            Some(serde_json::json!({ "name": name })),
        )?;
        let status = parse_http_status(&response);
        if status != 200 {
            return Err(http_error(status, &parse_http_body(&response)));
        }
        let body = parse_http_body_json(&response).ok_or("Invalid workspace response")?;
        remote_id = body
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        created_remote = true;
    }

    let remote_id = remote_id.ok_or("Missing remote_id")?;
    let files_uploaded = upload_workspace_snapshot(
        params,
        &config,
        &remote_id,
        workspace_root.as_deref(),
        "replace",
        include_attachments,
    )?;

    let mut updated = config;
    updated.workspace_id = Some(remote_id.clone());
    save_extism_config(&updated);
    reconcile_sync_transport(Some(true));

    Ok(serde_json::json!({
        "remote_id": remote_id,
        "created_remote": created_remote,
        "snapshot_uploaded": true,
        "files_uploaded": files_uploaded
    }))
}

fn handle_unlink_workspace(_params: &JsonValue) -> JsonValue {
    let mut config = load_extism_config();
    config.workspace_id = None;
    save_extism_config(&config);
    reconcile_sync_transport(None);
    serde_json::json!({ "ok": true })
}

fn handle_upload_workspace_snapshot(params: &JsonValue) -> Result<JsonValue, String> {
    if !provider_supported(params) {
        return Err("Unsupported provider".to_string());
    }

    let config = load_extism_config();
    let remote_id = command_param_str(params, "remote_id")
        .or_else(|| config.workspace_id.clone())
        .ok_or("Missing remote_id")?;
    let workspace_root = command_param_str(params, "workspace_root");
    let mode = command_param_str(params, "mode").unwrap_or_else(|| "replace".to_string());
    let include_attachments = command_param_bool(params, "include_attachments").unwrap_or(true);
    let files_uploaded = upload_workspace_snapshot(
        params,
        &config,
        &remote_id,
        workspace_root.as_deref(),
        &mode,
        include_attachments,
    )?;

    Ok(serde_json::json!({
        "remote_id": remote_id,
        "files_uploaded": files_uploaded,
        "snapshot_uploaded": true
    }))
}

fn handle_download_workspace(_params: &JsonValue) -> Result<JsonValue, String> {
    if !provider_supported(_params) {
        return Err("Unsupported provider".to_string());
    }

    let config = load_extism_config();
    let server = resolve_server_url(_params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(_params, &config));
    let remote_id = command_param_str(_params, "remote_id").ok_or("Missing remote_id")?;
    let workspace_root = command_param_str(_params, "workspace_root");
    let include_attachments = command_param_bool(_params, "include_attachments").unwrap_or(true);
    let link_after_import = command_param_bool(_params, "link").unwrap_or(false);

    let response = http_request_compat(
        "GET",
        &format!(
            "{server}/api/workspaces/{remote_id}/snapshot?include_attachments={include_attachments}"
        ),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }

    let snapshot_bytes = parse_http_body_bytes(&response)?;
    if snapshot_bytes.is_empty() {
        return Err("Snapshot download returned empty body".to_string());
    }

    let mut archive = ZipArchive::new(Cursor::new(snapshot_bytes))
        .map_err(|e| format!("Invalid snapshot zip: {e}"))?;

    let mut files_imported = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("Failed to read zip entry #{index}: {e}"))?;
        if entry.is_dir() {
            continue;
        }

        let raw_name = entry.name().to_string();
        if should_skip_snapshot_entry(&raw_name) {
            continue;
        }
        let Some(relative_path) = normalize_snapshot_entry_path(&raw_name) else {
            continue;
        };

        let target_path = resolve_workspace_path(workspace_root.as_deref(), &relative_path);
        if relative_path.ends_with(".md") {
            let mut content = String::new();
            entry
                .read_to_string(&mut content)
                .map_err(|e| format!("Failed to read markdown entry {relative_path}: {e}"))?;
            host::fs::write_file(&target_path, &content)?;
        } else {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Failed to read binary entry {relative_path}: {e}"))?;
            ensure_parent_dirs_for_binary(&target_path)?;
            host::fs::write_binary(&target_path, &bytes)?;
        }
        files_imported += 1;
    }

    if link_after_import {
        let mut updated = config;
        updated.workspace_id = Some(remote_id);
        save_extism_config(&updated);
        reconcile_sync_transport(Some(true));
    }

    Ok(serde_json::json!({ "files_imported": files_imported }))
}

fn resolve_runtime_workspace_id(
    params: &JsonValue,
    config: &SyncExtismConfig,
) -> Result<String, String> {
    command_param_str(params, "workspace_id")
        .or_else(|| config.workspace_id.clone())
        .filter(|value| !value.trim().is_empty())
        .ok_or("Missing workspace_id".to_string())
}

fn ensure_runtime_session(workspace_id: &str, write_to_disk: bool) -> Result<(), String> {
    state::create_session(workspace_id, write_to_disk).map_err(|e| e.to_string())
}

fn handle_prepare_live_share_runtime(params: &JsonValue) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let workspace_id = resolve_runtime_workspace_id(params, &config)?;
    let write_to_disk = command_param_bool(params, "write_to_disk").unwrap_or(true);

    ensure_runtime_session(&workspace_id, write_to_disk)?;

    Ok(serde_json::json!({
        "workspace_id": workspace_id,
        "write_to_disk": write_to_disk,
        "runtime_owner": "diaryx.sync"
    }))
}

fn handle_connect_live_share_session(params: &JsonValue) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let workspace_id = resolve_runtime_workspace_id(params, &config)?;
    let write_to_disk = command_param_bool(params, "write_to_disk").unwrap_or(false);
    let server_url = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let auth_token = command_param_str(params, "auth_token")
        .or_else(|| config.auth_token.clone())
        .filter(|value| !value.trim().is_empty());
    let session_code = command_param_str(params, "session_code")
        .or_else(|| command_param_str(params, "join_code"))
        .map(|value| value.to_uppercase())
        .ok_or("Missing session_code")?;

    ensure_runtime_session(&workspace_id, write_to_disk)?;
    host::ws::connect(
        &server_url,
        &workspace_id,
        auth_token.as_deref(),
        Some(&session_code),
        Some(write_to_disk),
    )?;

    Ok(serde_json::json!({
        "workspace_id": workspace_id,
        "session_code": session_code,
        "write_to_disk": write_to_disk,
        "connected": true
    }))
}

fn handle_disconnect_live_share_session(_params: &JsonValue) -> Result<JsonValue, String> {
    host::ws::disconnect()?;
    reconcile_sync_transport(Some(true));

    Ok(serde_json::json!({
        "ok": true,
        "connected": false
    }))
}

// ============================================================================
// JSON exports
// ============================================================================

/// Return the plugin manifest (metadata + UI contributions).
fn build_manifest() -> GuestManifest {
    let sync_settings_tab = UiContribution::SettingsTab {
        id: "sync-settings".into(),
        label: "Sync".into(),
        icon: None,
        fields: vec![
            SettingsField::AuthStatus {
                label: "Account".into(),
                description: Some("Sign in to enable sync.".into()),
            },
            SettingsField::UpgradeBanner {
                feature: "Sync".into(),
                description: Some("Upgrade to sync workspaces across devices.".into()),
            },
            SettingsField::Conditional {
                condition: "plus".into(),
                fields: vec![
                    SettingsField::Section {
                        label: "Connection".into(),
                        description: None,
                    },
                    SettingsField::Text {
                        key: "server_url".into(),
                        label: "Server URL".into(),
                        description: Some("Automatically configured when you sign in.".into()),
                        placeholder: Some("https://sync.diaryx.org".into()),
                    },
                    SettingsField::Button {
                        label: "Check Status".into(),
                        command: "GetProviderStatus".into(),
                        variant: Some("outline".into()),
                    },
                ],
            },
        ],
        component: None,
    };

    let snapshots_tab = UiContribution::SidebarTab {
        id: "snapshots".into(),
        label: "Snapshots".into(),
        icon: Some("history".into()),
        side: SidebarSide::Left,
        component: ComponentRef::Iframe {
            component_id: "sync.snapshots".into(),
        },
    };

    let history_tab = UiContribution::SidebarTab {
        id: "history".into(),
        label: "History".into(),
        icon: Some("history".into()),
        side: SidebarSide::Right,
        component: ComponentRef::Iframe {
            component_id: "sync.history".into(),
        },
    };

    let status_bar_item = UiContribution::StatusBarItem {
        id: "sync-status".into(),
        label: "Sync".into(),
        position: StatusBarPosition::Right,
        plugin_command: Some("GetSyncStatus".into()),
    };

    GuestManifest {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        id: "diaryx.sync".into(),
        name: "Sync".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "Real-time CRDT sync across devices".into(),
        capabilities: vec![
            "workspace_events".into(),
            "file_events".into(),
            "crdt_commands".into(),
            "sync_transport".into(),
            "custom_commands".into(),
        ],
        ui: vec![
            serde_json::to_value(&sync_settings_tab).unwrap_or_default(),
            serde_json::to_value(&snapshots_tab).unwrap_or_default(),
            serde_json::to_value(&history_tab).unwrap_or_default(),
            serde_json::to_value(&status_bar_item).unwrap_or_default(),
            serde_json::json!({
                "slot": "WorkspaceProvider",
                "id": "diaryx.sync",
                "label": "Diaryx Sync",
                "icon": "cloud",
            }),
        ],
        commands: all_commands(),
        requested_permissions: Some(GuestRequestedPermissions {
            defaults: serde_json::json!({
                "plugin_storage": { "include": ["all"], "exclude": [] },
                "http_requests": { "include": ["all"], "exclude": [] },
                "read_files": { "include": ["all"], "exclude": [] },
                "edit_files": { "include": ["all"], "exclude": [] },
                "create_files": { "include": ["all"], "exclude": [] },
                "delete_files": { "include": ["all"], "exclude": [] },
            }),
            reasons: [
                ("plugin_storage", "Store sync configuration and CRDT state"),
                ("http_requests", "Communicate with the sync server"),
                ("read_files", "Read workspace files for syncing"),
                ("edit_files", "Apply remote changes to workspace files"),
                ("create_files", "Create files received from remote sync"),
                ("delete_files", "Delete files removed by remote sync"),
            ].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }),
        cli: vec![serde_json::json!({
            "name": "sync",
            "about": "Sync workspace with remote server",
            "aliases": ["sy"],
            "subcommands": [
                {
                    "name": "login", "about": "Authenticate via magic link",
                    "native_handler": "sync_login", "requires_workspace": false,
                    "args": [
                        {"name": "email", "required": true, "help": "Email address"},
                        {"name": "server", "short": "s", "long": "server", "help": "Server URL"}
                    ]
                },
                {
                    "name": "verify", "about": "Complete authentication",
                    "native_handler": "sync_verify", "requires_workspace": false,
                    "args": [
                        {"name": "token", "required": true, "help": "Verification token"},
                        {"name": "device-name", "long": "device-name", "help": "Device name"}
                    ]
                },
                {
                    "name": "logout", "about": "Clear credentials",
                    "native_handler": "sync_logout", "requires_workspace": false
                },
                {
                    "name": "status", "about": "Show sync status",
                    "native_handler": "sync_status"
                },
                {
                    "name": "start", "about": "Start continuous sync",
                    "native_handler": "sync_start",
                    "args": [
                        {"name": "background", "short": "b", "long": "background",
                         "is_flag": true, "help": "Run in background"}
                    ]
                },
                {
                    "name": "push", "about": "Push local changes",
                    "native_handler": "sync_push"
                },
                {
                    "name": "pull", "about": "Pull remote changes",
                    "native_handler": "sync_pull"
                },
                {
                    "name": "config", "about": "Configure sync settings",
                    "native_handler": "sync_config",
                    "args": [
                        {"name": "server", "long": "server", "help": "Set server URL"},
                        {"name": "workspace-id", "long": "workspace-id", "help": "Set workspace ID"},
                        {"name": "show", "long": "show", "is_flag": true, "help": "Show current config"}
                    ]
                }
            ]
        })],
    }
}

#[plugin_fn]
pub fn manifest(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_manifest())?)
}

/// Initialize the plugin with workspace configuration.
#[plugin_fn]
pub fn init(input: String) -> FnResult<String> {
    let params: InitParams = serde_json::from_str(&input).unwrap_or(InitParams {
        workspace_root: None,
        workspace_id: None,
        write_to_disk: None,
        server_url: None,
        auth_token: None,
    });

    let mut extism_config = load_extism_config();
    if let Some(workspace_id) = &params.workspace_id {
        extism_config.workspace_id = Some(workspace_id.clone());
    }
    if let Some(server_url) = &params.server_url {
        extism_config.server_url = Some(server_url.clone());
    }
    if let Some(auth_token) = &params.auth_token {
        extism_config.auth_token = Some(auth_token.clone());
    }
    save_extism_config(&extism_config);

    state::init_state(params.workspace_id.clone()).map_err(extism_pdk::Error::msg)?;

    // If workspace_root is provided, configure the sync handler
    if let Some(root) = &params.workspace_root {
        let init_result = state::with_sync_plugin(|sync_plugin| {
            let ctx = diaryx_core::plugin::PluginContext {
                workspace_root: Some(std::path::PathBuf::from(root)),
                link_format: diaryx_core::link_parser::LinkFormat::default(),
            };
            // block_on the async init
            poll_future(diaryx_core::plugin::Plugin::init(sync_plugin, &ctx))
                .map_err(|e| format!("Plugin init failed: {e}"))
        })
        .map_err(|e| extism_pdk::Error::msg(e))?;
        init_result.map_err(extism_pdk::Error::msg)?;
    }

    // If workspace_id provided, create a session
    if let Some(ws_id) = &params.workspace_id {
        let write_to_disk = params.write_to_disk.unwrap_or(true);
        state::create_session(ws_id, write_to_disk).map_err(extism_pdk::Error::msg)?;
    }

    reconcile_sync_transport(params.write_to_disk);

    host::log::log("info", "Sync plugin initialized");
    Ok(String::new())
}

/// Shut down the plugin (persist state).
#[plugin_fn]
pub fn shutdown(_input: String) -> FnResult<String> {
    let _ = host::ws::disconnect();
    if let Err(e) = state::shutdown_state() {
        host::log::log("warn", &format!("Shutdown state cleanup failed: {e}"));
    }
    host::log::log("info", "Sync plugin shut down");
    Ok(String::new())
}

/// Handle a structured command.
fn command_response(result: Result<JsonValue, String>) -> CommandResponse {
    match result {
        Ok(data) => CommandResponse::ok(data),
        Err(error) => CommandResponse::err(error),
    }
}

fn execute_command(req: CommandRequest) -> CommandResponse {
    let CommandRequest { command, params } = req;
    if matches!(
        command.as_str(),
        "get_component_html" | "get_config" | "set_config"
    ) {
        host::log::log("debug", &format!("[sync] handle_command: {command}"));
    }

    let custom_result: Option<Result<JsonValue, String>> = match command.as_str() {
        "get_component_html" => {
            let component_id = params
                .get("component_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            host::log::log(
                "debug",
                &format!("[sync] get_component_html requested: {component_id}"),
            );
            Some(
                get_component_html_by_id(component_id)
                    .map(|html| JsonValue::String(html.to_string()))
                    .ok_or_else(|| format!("Unknown sync component: {component_id}")),
            )
        }
        "get_config" => Some(Ok(
            serde_json::to_value(load_extism_config()).unwrap_or_default()
        )),
        "set_config" => {
            let mut config = load_extism_config();
            apply_config_patch(&mut config, &params);
            save_extism_config(&config);
            reconcile_sync_transport(None);
            Some(Ok(JsonValue::Null))
        }
        "GetSyncStatus" => Some(Ok(sync_status_from_state())),
        "GetProviderStatus" => Some(handle_get_provider_status(&params)),
        "ListRemoteWorkspaces" => Some(handle_list_remote_workspaces(&params)),
        "LinkWorkspace" => Some(handle_link_workspace(&params)),
        "UploadWorkspaceSnapshot" => Some(handle_upload_workspace_snapshot(&params)),
        "UnlinkWorkspace" => Some(Ok(handle_unlink_workspace(&params))),
        "DownloadWorkspace" => Some(handle_download_workspace(&params)),
        "PrepareLiveShareRuntime" => Some(handle_prepare_live_share_runtime(&params)),
        "ConnectLiveShareSession" => Some(handle_connect_live_share_session(&params)),
        "DisconnectLiveShareSession" => Some(handle_disconnect_live_share_session(&params)),
        _ => None,
    };

    if let Some(result) = custom_result {
        return command_response(result);
    }

    let result = match state::with_sync_plugin(|sync_plugin| {
        poll_future(diaryx_core::plugin::WorkspacePlugin::handle_command(
            sync_plugin,
            &command,
            params,
        ))
    }) {
        Ok(result) => result,
        Err(e) => return command_response(Err(e.to_string())),
    };

    match result {
        Some(Ok(data)) => CommandResponse::ok(data),
        Some(Err(e)) => CommandResponse::err(e.to_string()),
        None => CommandResponse::err(format!("Unknown command: {command}")),
    }
}

#[plugin_fn]
pub fn handle_command(input: String) -> FnResult<String> {
    let req: CommandRequest = serde_json::from_str(&input)?;
    let response = execute_command(req);

    Ok(serde_json::to_string(&response)?)
}

/// Handle a filesystem/workspace event from the host.
#[plugin_fn]
pub fn on_event(input: String) -> FnResult<String> {
    let event: GuestEvent = serde_json::from_str(&input)?;
    let mut session_actions = Vec::new();

    match event.event_type.as_str() {
        "file_saved" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                let body_changed = event
                    .payload
                    .get("body_changed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                host::log::log(
                    "debug",
                    &format!(
                        "[on_event:file_saved] path={} body_changed={}",
                        path, body_changed
                    ),
                );
                if let Err(e) = state::with_sync_plugin(|sync_plugin| match host::fs::read_file(
                    path,
                ) {
                    Ok(content) => {
                        let existing_parent_path =
                            parse_workspace_metadata_from_markdown(sync_plugin, path, &content)
                                .map(|(_, metadata)| metadata.part_of)
                                .unwrap_or(None);
                        let relative_path = workspace_relative_path_for_sync(sync_plugin, path);
                        let previous_parent_path = sync_plugin
                            .workspace_crdt()
                            .get_file(&relative_path)
                            .and_then(|metadata| metadata.part_of);
                        if body_changed {
                            let body = frontmatter::extract_body(&content);
                            let body_docs = sync_plugin.body_docs();
                            let existing_doc = body_docs.get(path);
                            let should_emit_local_body_update = existing_doc.is_some()
                                || sync_plugin.sync_manager().is_body_synced(path);
                            if should_emit_local_body_update {
                                let doc =
                                    existing_doc.unwrap_or_else(|| body_docs.get_or_create(path));
                                let _ = doc.set_body(&body);
                            } else {
                                host::log::log(
                                    "debug",
                                    &format!(
                                        "[on_event:file_saved] skipping local body queue for unsynced doc {}",
                                        path
                                    ),
                                );
                            }
                            sync_plugin.sync_manager().track_content(path, &body);
                        }
                        let mut changed =
                            parse_workspace_metadata_from_markdown(sync_plugin, path, &content)
                                .map(|(relative_path, metadata)| {
                                    apply_workspace_metadata(sync_plugin, &relative_path, &metadata)
                                })
                                .unwrap_or(false);
                        changed |= refresh_related_parent_indexes(
                            sync_plugin,
                            &[previous_parent_path, existing_parent_path],
                        );
                        if changed {
                            flush_workspace_metadata_changes(sync_plugin);
                        }
                    }
                    Err(e) => {
                        host::log::log(
                            "warn",
                            &format!("[on_event:file_saved] read_file FAILED for {}: {}", path, e),
                        );
                    }
                }) {
                    host::log::log("warn", &format!("[on_event:file_saved] {e}"));
                }
            }
        }
        "file_created" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    if let Ok(content) = host::fs::read_file(path) {
                        let body_docs = sync_plugin.body_docs();
                        let doc = body_docs.get_or_create(path);
                        let _ = doc.set_body(frontmatter::extract_body(&content));
                        let changed =
                            parse_workspace_metadata_from_markdown(sync_plugin, path, &content)
                                .map(|(relative_path, metadata)| {
                                    let parent_path = metadata.part_of.clone();
                                    let mut changed = apply_workspace_metadata(
                                        sync_plugin,
                                        &relative_path,
                                        &metadata,
                                    );
                                    changed |=
                                        refresh_related_parent_indexes(sync_plugin, &[parent_path]);
                                    changed
                                })
                                .unwrap_or(false);
                        if changed {
                            flush_workspace_metadata_changes(sync_plugin);
                        }
                    }
                }) {
                    host::log::log("warn", &format!("[on_event:file_created] {e}"));
                }
            }
        }
        "file_deleted" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    let body_docs = sync_plugin.body_docs();
                    let _ = body_docs.delete(path);
                    let relative_path = workspace_relative_path_for_sync(sync_plugin, path);
                    let workspace = sync_plugin.workspace_crdt();
                    let previous_parent_path = workspace
                        .get_file(&relative_path)
                        .and_then(|metadata| metadata.part_of);
                    let mut changed = soft_delete_file(&workspace, &relative_path);
                    changed |= refresh_related_parent_indexes(sync_plugin, &[previous_parent_path]);
                    if changed {
                        flush_workspace_metadata_changes(sync_plugin);
                    }
                }) {
                    host::log::log("warn", &format!("[on_event:file_deleted] {e}"));
                }
            }
        }
        "file_renamed" | "file_moved" => {
            let old_path = event.payload.get("old_path").and_then(|v| v.as_str());
            let new_path = event.payload.get("new_path").and_then(|v| v.as_str());
            if let (Some(old), Some(new)) = (old_path, new_path) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    sync_plugin
                        .sync_manager()
                        .migrate_body_docs_for_renames(&[(old.to_string(), new.to_string())]);
                    let old_relative_path = workspace_relative_path_for_sync(sync_plugin, old);
                    let new_relative_path = workspace_relative_path_for_sync(sync_plugin, new);
                    let workspace = sync_plugin.workspace_crdt();
                    let previous_parent_path = workspace
                        .get_file(&old_relative_path)
                        .and_then(|metadata| metadata.part_of);
                    if let Ok(content) = host::fs::read_file(new) {
                        if let Some((_, metadata)) =
                            parse_workspace_metadata_from_markdown(sync_plugin, new, &content)
                        {
                            let next_parent_path = metadata.part_of.clone();
                            let mut changed = soft_delete_file(&workspace, &old_relative_path);
                            changed |= apply_workspace_metadata(
                                sync_plugin,
                                &new_relative_path,
                                &metadata,
                            );
                            changed |= refresh_related_parent_indexes(
                                sync_plugin,
                                &[previous_parent_path, next_parent_path],
                            );
                            if changed {
                                flush_workspace_metadata_changes(sync_plugin);
                            }
                        }
                    } else {
                        host::log::log(
                            "warn",
                            &format!(
                                "[on_event:{}] host_read_file({}) failed",
                                event.event_type, new
                            ),
                        );
                    }
                }) {
                    host::log::log("warn", &format!("[on_event:file_renamed] {e}"));
                }
            }
        }
        "workspace_opened" => {
            if let Some(root) = event.payload.get("workspace_root").and_then(|v| v.as_str()) {
                if let Err(e) = state::with_sync_plugin(|sync_plugin| {
                    let event = diaryx_core::plugin::WorkspaceOpenedEvent {
                        workspace_root: std::path::PathBuf::from(root),
                    };
                    poll_future(diaryx_core::plugin::WorkspacePlugin::on_workspace_opened(
                        sync_plugin,
                        &event,
                    ));
                }) {
                    host::log::log("warn", &format!("[on_event:workspace_opened] {e}"));
                }
            }
        }
        "file_opened" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                session_actions = with_session_actions("on_event:file_opened", |session| {
                    poll_future(session.process(IncomingEvent::SyncBodyFiles {
                        file_paths: vec![path.to_string()],
                    }))
                })
            }
        }
        other => {
            host::log::log("debug", &format!("Unhandled event type: {other}"));
        }
    }

    execute_session_actions(session_actions);
    Ok(String::new())
}

/// Get plugin configuration.
#[plugin_fn]
pub fn get_config(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&load_extism_config())?)
}

/// Set plugin configuration.
#[plugin_fn]
pub fn set_config(input: String) -> FnResult<String> {
    let incoming: JsonValue = serde_json::from_str(&input)?;
    let mut config = load_extism_config();
    apply_config_patch(&mut config, &incoming);
    save_extism_config(&config);
    reconcile_sync_transport(None);
    Ok(String::new())
}

// ============================================================================
// Binary exports (hot path)
// ============================================================================

/// Handle an incoming binary WebSocket message.
/// Input: raw framed v2 sync message bytes.
/// Output: binary action envelope for compatibility with native callers.
#[plugin_fn]
pub fn handle_binary_message(input: Vec<u8>) -> FnResult<Vec<u8>> {
    let actions = with_session_actions("handle_binary_message", |session| {
        poll_future(session.process(IncomingEvent::BinaryMessage(input)))
    });
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions);
    Ok(encoded)
}

/// Handle an incoming text WebSocket message (control/handshake).
/// Input: JSON text message.
/// Output: binary action envelope for compatibility with native callers.
#[plugin_fn]
pub fn handle_text_message(input: String) -> FnResult<Vec<u8>> {
    let actions = with_session_actions("handle_text_message", |session| {
        poll_future(session.process(IncomingEvent::TextMessage(input)))
    });
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions);
    Ok(encoded)
}

/// Called when a WebSocket connection is established.
/// Input: connection info JSON (workspace_id, etc.)
/// Output: binary action envelope for compatibility with native callers.
#[plugin_fn]
pub fn on_connected(input: String) -> FnResult<Vec<u8>> {
    // Parse connection params if provided
    if let Ok(params) = serde_json::from_str::<InitParams>(&input) {
        if let Some(ws_id) = params.workspace_id {
            let write_to_disk = params.write_to_disk.unwrap_or(true);
            if let Err(e) = state::create_session(&ws_id, write_to_disk) {
                host::log::log(
                    "warn",
                    &format!("[on_connected] create_session failed: {e}"),
                );
            }
        }
    }

    let actions = with_session_actions("on_connected", |session| {
        poll_future(session.process(IncomingEvent::Connected))
    });
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions);
    Ok(encoded)
}

/// Called when the WebSocket disconnects.
/// Output: binary action envelope for compatibility with native callers.
#[plugin_fn]
pub fn on_disconnected(_input: String) -> FnResult<Vec<u8>> {
    let actions = with_session_actions("on_disconnected", |session| {
        poll_future(session.process(IncomingEvent::Disconnected))
    });

    // Persist state on disconnect
    if let Err(e) = state::persist_state() {
        host::log::log(
            "warn",
            &format!("[on_disconnected] persist_state failed: {e}"),
        );
    }

    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions);
    Ok(encoded)
}

/// Queue a local CRDT update to be sent to the server.
/// Input: JSON `{"doc_id": "...", "data": "base64..."}`.
/// Output: binary action envelope for compatibility with native callers.
#[plugin_fn]
pub fn queue_local_update(input: String) -> FnResult<Vec<u8>> {
    let params: JsonValue = serde_json::from_str(&input)?;
    let doc_id = params
        .get("doc_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let data_b64 = params.get("data").and_then(|v| v.as_str()).unwrap_or("");

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .unwrap_or_default();

    let actions = with_session_actions("queue_local_update", |session| {
        poll_future(session.process(IncomingEvent::LocalUpdate { doc_id, data }))
    });
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions);
    Ok(encoded)
}

/// Called after a snapshot has been imported.
/// Output: binary action envelope for compatibility with native callers.
#[plugin_fn]
pub fn on_snapshot_imported(_input: String) -> FnResult<Vec<u8>> {
    let actions = with_session_actions("on_snapshot_imported", |session| {
        poll_future(session.process(IncomingEvent::SnapshotImported))
    });
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions);
    Ok(encoded)
}

/// Request body sync for specific files.
/// Input: JSON `{"file_paths": ["path1", "path2"]}`.
/// Output: binary action envelope for compatibility with native callers.
#[plugin_fn]
pub fn sync_body_files(input: String) -> FnResult<Vec<u8>> {
    let params: JsonValue = serde_json::from_str(&input)?;
    let file_paths: Vec<String> = params
        .get("file_paths")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let actions = with_session_actions("sync_body_files", |session| {
        poll_future(session.process(IncomingEvent::SyncBodyFiles { file_paths }))
    });
    let encoded = binary_protocol::encode_actions(&actions);
    execute_session_actions(actions);
    Ok(encoded)
}

// ============================================================================
// Helpers
// ============================================================================

/// Execute a typed Command (same format as Diaryx::execute).
///
/// Takes a JSON object with `type` and optional `params` fields, extracts
/// them, and calls `handle_command` on the inner SyncPlugin.
/// Returns the result as a serialized JSON string.
/// Returns empty string if the command is not handled by this plugin.
#[plugin_fn]
pub fn execute_typed_command(input: String) -> FnResult<String> {
    let parsed: serde_json::Value = serde_json::from_str(&input)
        .map_err(|e| extism_pdk::Error::msg(format!("Invalid JSON: {e}")))?;

    // Extract command type and params from the tagged enum format
    // Commands are serialized as { "type": "CommandName", "params": { ... } }
    let cmd_type = parsed["type"]
        .as_str()
        .ok_or_else(|| extism_pdk::Error::msg("Missing 'type' field in command"))?;

    let params = parsed
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // Route through execute_command which handles custom commands (provider,
    // share, config) before falling through to the core WorkspacePlugin trait.
    let resp = execute_command(CommandRequest {
        command: cmd_type.to_string(),
        params,
    });

    if resp.success {
        let response = serde_json::json!({ "type": "PluginResult", "data": resp.data });
        let json = serde_json::to_string(&response)
            .map_err(|e| extism_pdk::Error::msg(format!("Serialize error: {e}")))?;
        Ok(json)
    } else if let Some(ref error) = resp.error {
        if error.starts_with("Unknown command:") {
            // Not handled by this plugin — return empty so caller can fall through
            Ok(String::new())
        } else {
            // Command was handled but failed
            Err(extism_pdk::Error::msg(error.clone()).into())
        }
    } else {
        Ok(String::new())
    }
}

/// List all commands this plugin handles.
fn all_commands() -> Vec<String> {
    vec![
        // Workspace CRDT State
        "GetSyncState",
        "GetFullState",
        "ApplyRemoteUpdate",
        "GetMissingUpdates",
        "SaveCrdtState",
        // File Metadata
        "GetCrdtFile",
        "SetCrdtFile",
        "ListCrdtFiles",
        // Body Documents
        "GetBodyContent",
        "SetBodyContent",
        "ResetBodyDoc",
        "GetBodySyncState",
        "GetBodyFullState",
        "ApplyBodyUpdate",
        "GetBodyMissingUpdates",
        "SaveBodyDoc",
        "SaveAllBodyDocs",
        "ListLoadedBodyDocs",
        "UnloadBodyDoc",
        // Y-Sync Protocol
        "CreateSyncStep1",
        "HandleSyncMessage",
        "CreateUpdateMessage",
        // Sync Handler
        "ConfigureSyncHandler",
        "GetStoragePath",
        "GetCanonicalPath",
        "ApplyRemoteWorkspaceUpdateWithEffects",
        "ApplyRemoteBodyUpdateWithEffects",
        // Sync Manager
        "HandleWorkspaceSyncMessage",
        "HandleCrdtState",
        "CreateWorkspaceSyncStep1",
        "CreateWorkspaceUpdate",
        "InitBodySync",
        "CloseBodySync",
        "HandleBodySyncMessage",
        "CreateBodySyncStep1",
        "CreateBodyUpdate",
        "IsSyncComplete",
        "IsWorkspaceSynced",
        "IsBodySynced",
        "MarkSyncComplete",
        "GetActiveSyncs",
        "TrackContent",
        "IsEcho",
        "ClearTrackedContent",
        "ResetSyncState",
        "TriggerWorkspaceSync",
        // History
        "GetHistory",
        "GetFileHistory",
        "RestoreVersion",
        "GetVersionDiff",
        "GetStateAt",
        // Workspace Initialization
        "InitializeWorkspaceCrdt",
        // Status
        "GetSyncStatus",
        // Workspace Provider
        "GetProviderStatus",
        "ListRemoteWorkspaces",
        "LinkWorkspace",
        "UploadWorkspaceSnapshot",
        "UnlinkWorkspace",
        "DownloadWorkspace",
        // Iframe Components
        "get_component_html",
        "get_config",
        "set_config",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Simple single-poll executor for immediately-ready futures.
///
/// In the Extism guest (single-threaded WASM), all async operations complete
/// synchronously because host function calls are synchronous. This function
/// polls the future once and returns the result.
fn poll_future<F: std::future::Future>(f: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    let raw_waker = RawWaker::new(std::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw_waker) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = pin!(f);

    match pinned.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("Future was not immediately ready in Extism guest"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_settings_tab_uses_declarative_fields() {
        let manifest = build_manifest();
        let tab = manifest
            .ui
            .iter()
            .find(|ui| {
                ui.get("slot").and_then(|v| v.as_str()) == Some("SettingsTab")
                    && ui.get("id").and_then(|v| v.as_str()) == Some("sync-settings")
            })
            .expect("sync settings tab should exist");

        // Should NOT have an iframe component
        assert!(
            tab.get("component").unwrap().is_null(),
            "settings tab component should be null (no iframe)"
        );

        // Should have declarative fields
        let fields = tab
            .get("fields")
            .and_then(|v| v.as_array())
            .expect("fields should be an array");
        assert!(!fields.is_empty(), "fields should not be empty");

        // First field: AuthStatus
        assert_eq!(
            fields[0].get("type").and_then(|v| v.as_str()),
            Some("AuthStatus")
        );

        // Second field: UpgradeBanner
        assert_eq!(
            fields[1].get("type").and_then(|v| v.as_str()),
            Some("UpgradeBanner")
        );
        assert_eq!(
            fields[1].get("feature").and_then(|v| v.as_str()),
            Some("Sync")
        );

        // Third field: Conditional with condition "plus"
        assert_eq!(
            fields[2].get("type").and_then(|v| v.as_str()),
            Some("Conditional")
        );
        assert_eq!(
            fields[2].get("condition").and_then(|v| v.as_str()),
            Some("plus")
        );

        // Nested fields inside the Conditional
        let nested = fields[2]
            .get("fields")
            .and_then(|v| v.as_array())
            .expect("conditional should have nested fields");
        assert_eq!(nested.len(), 3);
        assert_eq!(
            nested[0].get("type").and_then(|v| v.as_str()),
            Some("Section")
        );
        assert_eq!(nested[1].get("type").and_then(|v| v.as_str()), Some("Text"));
        assert_eq!(
            nested[1].get("key").and_then(|v| v.as_str()),
            Some("server_url")
        );
        assert_eq!(
            nested[2].get("type").and_then(|v| v.as_str()),
            Some("Button")
        );
        assert_eq!(
            nested[2].get("command").and_then(|v| v.as_str()),
            Some("GetProviderStatus")
        );
    }

    #[test]
    fn manifest_no_longer_exposes_share_tab() {
        let manifest = build_manifest();
        assert!(
            manifest
                .ui
                .iter()
                .all(|ui| { ui.get("id").and_then(|v| v.as_str()) != Some("share") })
        );
    }

    #[test]
    fn settings_html_not_served() {
        // Removed iframe-backed UI entrypoints should not resolve.
        assert!(get_component_html_by_id("sync.settings").is_none());
        assert!(get_component_html_by_id("sync.share").is_none());
    }

    #[test]
    fn public_command_list_does_not_include_share_commands() {
        let commands = all_commands();
        assert!(!commands.iter().any(|cmd| cmd == "CreateShareSession"));
        assert!(!commands.iter().any(|cmd| cmd == "JoinShareSession"));
        assert!(!commands.iter().any(|cmd| cmd == "EndShareSession"));
        assert!(!commands.iter().any(|cmd| cmd == "SetShareReadOnly"));
    }

    #[test]
    fn manifest_declares_requested_permissions() {
        let manifest = build_manifest();
        let perms = manifest
            .requested_permissions
            .as_ref()
            .expect("manifest should declare requested_permissions");
        let defaults = perms.get("defaults").expect("should have defaults");

        assert!(defaults.get("plugin_storage").is_some());
        assert!(defaults.get("http_requests").is_some());
        let read_include = defaults
            .get("read_files")
            .and_then(|rule| rule.get("include"))
            .and_then(|include| include.as_array())
            .expect("read_files should declare include rules");
        let edit_include = defaults
            .get("edit_files")
            .and_then(|rule| rule.get("include"))
            .and_then(|include| include.as_array())
            .expect("edit_files should declare include rules");

        assert!(
            read_include
                .iter()
                .any(|value| value.as_str() == Some("all"))
        );
        assert!(
            edit_include
                .iter()
                .any(|value| value.as_str() == Some("all"))
        );
    }

    #[test]
    fn apply_config_patch_clears_and_sets_values() {
        let mut cfg = SyncExtismConfig {
            server_url: Some("https://old.example".to_string()),
            auth_token: Some("old-token".to_string()),
            workspace_id: Some("old-workspace".to_string()),
        };

        let patch = serde_json::json!({
            "server_url": null,
            "auth_token": "  ",
            "workspace_id": "new-workspace"
        });
        apply_config_patch(&mut cfg, &patch);

        assert_eq!(cfg.server_url, None);
        assert_eq!(cfg.auth_token, None);
        assert_eq!(cfg.workspace_id.as_deref(), Some("new-workspace"));
    }

    #[test]
    fn normalize_server_base_strips_sync_suffixes_and_trailing_slashes() {
        assert_eq!(
            normalize_server_base("https://sync.diaryx.org/sync2/"),
            "https://sync.diaryx.org"
        );
        assert_eq!(
            normalize_server_base("https://sync.diaryx.org/sync/"),
            "https://sync.diaryx.org"
        );
        assert_eq!(
            normalize_server_base("https://sync.diaryx.org/sync2/sync/"),
            "https://sync.diaryx.org"
        );
    }
}
