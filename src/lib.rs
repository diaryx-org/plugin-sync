//! Extism guest plugin for LWW file sync across devices.
//!
//! This crate compiles to a `.wasm` module loaded by the Extism host runtime.
//! It syncs workspace files to the namespace object store using hash-based
//! diffing and last-writer-wins conflict resolution.
//!
//! ## JSON exports (standard Extism protocol)
//!
//! - `manifest()` — plugin metadata + UI contributions
//! - `init()` — initialize with workspace config
//! - `shutdown()` — persist state and clean up
//! - `handle_command()` — structured commands (sync push/pull/status, etc.)
//! - `on_event()` — filesystem events from the host
//! - `get_config()` / `set_config()` — plugin configuration

#[cfg(not(target_arch = "wasm32"))]
mod native_extism_stubs;
pub mod server_api;
pub mod state;
pub mod sync_engine;
pub mod sync_manifest;

use diaryx_plugin_sdk::prelude::*;
diaryx_plugin_sdk::register_getrandom_v02!();

use extism_pdk::*;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

use diaryx_core::plugin::{
    ComponentRef, SettingsField, SidebarSide, StatusBarPosition, UiContribution,
};
use diaryx_plugin_sdk::protocol::ServerFunctionDecl;

// ============================================================================
// HTTP compat helpers (adapt SDK's typed HttpResponse to old JsonValue API)
// ============================================================================

fn http_request_compat(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body_json: Option<JsonValue>,
) -> Result<JsonValue, String> {
    let header_map: HashMap<String, String> = headers.iter().cloned().collect();
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
    let header_map: HashMap<String, String> = headers.iter().cloned().collect();
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

fn http_error(status: u64, body: &str) -> String {
    if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
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

fn auth_headers(auth_token: Option<String>) -> Vec<(String, String)> {
    match auth_token {
        Some(token) if !token.trim().is_empty() => vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {}", token)),
        ],
        _ => vec![("Content-Type".to_string(), "application/json".to_string())],
    }
}

fn sync_status_from_state() -> JsonValue {
    let config = load_extism_config();
    let has_workspace_id = config
        .workspace_id
        .as_deref()
        .map(|id| !id.trim().is_empty())
        .unwrap_or(false);

    let (dirty, clean, last_sync, pending_deletes) = state::with_manifest(|m| {
        (
            m.dirty_count(),
            m.clean_count(),
            m.last_sync_at,
            m.pending_deletes.len(),
        )
    })
    .unwrap_or((0, 0, None, 0));

    let label = if !has_workspace_id {
        "Not linked"
    } else if dirty > 0 {
        "Modified"
    } else {
        "Synced"
    };

    serde_json::json!({
        "state": if dirty > 0 { "dirty" } else { "synced" },
        "label": label,
        "dirty_count": dirty,
        "clean_count": clean,
        "last_sync_at": last_sync,
        "pending_deletes": pending_deletes,
    })
}

fn get_component_html_by_id(component_id: &str) -> Option<&'static str> {
    match component_id {
        "sync.snapshots" => Some(include_str!("ui/snapshots.html")),
        "sync.history" => Some(include_str!("ui/history.html")),
        _ => None,
    }
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
    let response = http_request_compat("GET", &format!("{server}/api/workspaces"), &headers, None)?;
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
    let mut namespace_id = command_param_str(params, "namespace_id")
        .or_else(|| command_param_str(params, "remote_id"));
    let mut created = false;

    if namespace_id.is_none() {
        let name = command_param_str(params, "name").ok_or("Missing name or namespace_id")?;
        let ns = server_api::create_namespace(params, &name)?;
        namespace_id = ns.get("id").and_then(|v| v.as_str()).map(String::from);
        created = true;
    }

    let namespace_id = namespace_id.ok_or("Missing namespace_id")?;

    let mut updated = config;
    updated.workspace_id = Some(namespace_id.clone());
    save_extism_config(&updated);

    state::set_namespace_id(Some(namespace_id.clone()));

    // Do an initial push
    let result = handle_sync_full(params)?;

    Ok(serde_json::json!({
        "namespace_id": namespace_id,
        "created": created,
        "sync": result,
    }))
}

fn handle_unlink_workspace(_params: &JsonValue) -> JsonValue {
    let mut config = load_extism_config();
    config.workspace_id = None;
    save_extism_config(&config);
    state::set_namespace_id(None);
    serde_json::json!({ "ok": true })
}

// ---------------------------------------------------------------------------
// Sync command handlers
// ---------------------------------------------------------------------------

fn handle_sync_push(params: &JsonValue) -> Result<JsonValue, String> {
    let namespace_id = resolve_namespace_id(params)?;
    let workspace_root = resolve_workspace_root()?;

    let result = state::with_manifest_mut(|manifest| {
        let local_scan = sync_engine::scan_local(&workspace_root);
        let server_entries = sync_engine::fetch_server_manifest(params, &namespace_id)?;
        let plan = sync_engine::compute_diff(manifest, &local_scan, &server_entries);

        let (pushed, errors) = sync_engine::execute_push(
            params,
            &namespace_id,
            &workspace_root,
            &plan,
            &local_scan,
            manifest,
        );

        manifest.save();

        Ok(serde_json::json!({
            "pushed": pushed,
            "errors": errors,
        }))
    })
    .unwrap_or_else(|| Err("Plugin state not initialized".to_string()))?;

    Ok(result)
}

fn handle_sync_pull(params: &JsonValue) -> Result<JsonValue, String> {
    let namespace_id = resolve_namespace_id(params)?;
    let workspace_root = resolve_workspace_root()?;

    let result = state::with_manifest_mut(|manifest| {
        let local_scan = sync_engine::scan_local(&workspace_root);
        let server_entries = sync_engine::fetch_server_manifest(params, &namespace_id)?;
        let plan = sync_engine::compute_diff(manifest, &local_scan, &server_entries);

        let (pulled, errors) = sync_engine::execute_pull(
            params,
            &namespace_id,
            &workspace_root,
            &plan,
            &server_entries,
            manifest,
        );

        manifest.save();

        Ok(serde_json::json!({
            "pulled": pulled,
            "errors": errors,
        }))
    })
    .unwrap_or_else(|| Err("Plugin state not initialized".to_string()))?;

    Ok(result)
}

fn handle_sync_full(params: &JsonValue) -> Result<JsonValue, String> {
    let namespace_id = resolve_namespace_id(params)?;
    let workspace_root = resolve_workspace_root()?;

    let result = state::with_manifest_mut(|manifest| {
        sync_engine::sync(params, &namespace_id, &workspace_root, manifest)
    })
    .ok_or_else(|| "Plugin state not initialized".to_string())?;

    serde_json::to_value(&result).map_err(|e| e.to_string())
}

fn handle_sync_status(_params: &JsonValue) -> Result<JsonValue, String> {
    Ok(sync_status_from_state())
}

fn resolve_namespace_id(params: &JsonValue) -> Result<String, String> {
    command_param_str(params, "namespace_id")
        .or_else(|| {
            let config = load_extism_config();
            config.workspace_id
        })
        .or_else(|| state::namespace_id())
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "No namespace linked. Use `sync link` first.".to_string())
}

fn resolve_workspace_root() -> Result<String, String> {
    state::workspace_root()
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| "Missing workspace_root".to_string())
}

// ---------------------------------------------------------------------------
// Namespace API command handlers
// ---------------------------------------------------------------------------

fn handle_ns_create_namespace(params: &JsonValue) -> Result<JsonValue, String> {
    let ns_id = command_param_str(params, "namespace_id").ok_or("Missing namespace_id")?;
    server_api::create_namespace(params, &ns_id)
}

fn handle_ns_list_namespaces(params: &JsonValue) -> Result<JsonValue, String> {
    server_api::list_namespaces(params)
}

fn handle_ns_put_object(params: &JsonValue) -> Result<JsonValue, String> {
    let ns_id = command_param_str(params, "namespace_id").ok_or("Missing namespace_id")?;
    let key = command_param_str(params, "key").ok_or("Missing key")?;
    let content_type = command_param_str(params, "content_type")
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let body: Vec<u8> = if let Some(b64) = command_param_str(params, "body_base64") {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64)
            .map_err(|e| format!("Invalid base64: {}", e))?
    } else if let Some(text) = command_param_str(params, "body") {
        text.into_bytes()
    } else {
        return Err("Missing body or body_base64".to_string());
    };

    server_api::put_object(params, &ns_id, &key, &body, &content_type)
}

fn handle_ns_get_object(params: &JsonValue) -> Result<JsonValue, String> {
    let ns_id = command_param_str(params, "namespace_id").ok_or("Missing namespace_id")?;
    let key = command_param_str(params, "key").ok_or("Missing key")?;
    server_api::get_object(params, &ns_id, &key)
}

fn handle_ns_delete_object(params: &JsonValue) -> Result<JsonValue, String> {
    let ns_id = command_param_str(params, "namespace_id").ok_or("Missing namespace_id")?;
    let key = command_param_str(params, "key").ok_or("Missing key")?;
    server_api::delete_object(params, &ns_id, &key)?;
    Ok(JsonValue::Null)
}

fn handle_ns_list_objects(params: &JsonValue) -> Result<JsonValue, String> {
    let ns_id = command_param_str(params, "namespace_id").ok_or("Missing namespace_id")?;
    server_api::list_objects(params, &ns_id)
}

// ============================================================================
// JSON exports
// ============================================================================

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

    GuestManifest::new(
        "diaryx.sync",
        "Sync",
        env!("CARGO_PKG_VERSION"),
        "File sync across devices",
        vec![
            "workspace_events".into(),
            "file_events".into(),
            "custom_commands".into(),
        ],
    )
    .ui(vec![
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
    ])
    .commands(all_commands())
    .server_functions(vec![
        ServerFunctionDecl {
            name: "create_namespace".into(),
            method: "POST".into(),
            path: "/namespaces".into(),
            description: "Create a user-owned namespace".into(),
        },
        ServerFunctionDecl {
            name: "list_namespaces".into(),
            method: "GET".into(),
            path: "/namespaces".into(),
            description: "List namespaces owned by the authenticated user".into(),
        },
        ServerFunctionDecl {
            name: "put_object".into(),
            method: "PUT".into(),
            path: "/namespaces/{id}/objects/{key}".into(),
            description: "Store bytes under the given key in a namespace".into(),
        },
        ServerFunctionDecl {
            name: "get_object".into(),
            method: "GET".into(),
            path: "/namespaces/{id}/objects/{key}".into(),
            description: "Retrieve bytes by key from a namespace".into(),
        },
        ServerFunctionDecl {
            name: "delete_object".into(),
            method: "DELETE".into(),
            path: "/namespaces/{id}/objects/{key}".into(),
            description: "Delete an object from a namespace".into(),
        },
        ServerFunctionDecl {
            name: "list_objects".into(),
            method: "GET".into(),
            path: "/namespaces/{id}/objects".into(),
            description: "List object metadata in a namespace".into(),
        },
    ])
    .requested_permissions(GuestRequestedPermissions {
        defaults: serde_json::json!({
            "plugin_storage": { "include": ["all"], "exclude": [] },
            "http_requests": { "include": ["all"], "exclude": [] },
            "read_files": { "include": ["all"], "exclude": [] },
            "edit_files": { "include": ["all"], "exclude": [] },
            "create_files": { "include": ["all"], "exclude": [] },
            "delete_files": { "include": ["all"], "exclude": [] },
        }),
        reasons: [
            ("plugin_storage", "Store sync configuration and manifest"),
            ("http_requests", "Communicate with the sync server"),
            ("read_files", "Read workspace files for syncing"),
            ("edit_files", "Apply remote changes to workspace files"),
            ("create_files", "Create files received from remote sync"),
            ("delete_files", "Delete files removed by remote sync"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    })
    .cli(vec![serde_json::json!({
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
                "name": "push", "about": "Push local changes to server",
                "native_handler": "sync_push"
            },
            {
                "name": "pull", "about": "Pull remote changes from server",
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
    })])
}

#[plugin_fn]
pub fn manifest(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_manifest())?)
}

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

    state::init_state(
        extism_config.workspace_id.clone(),
        params.workspace_root.clone(),
    );

    host::log::log("info", "Sync plugin initialized");
    Ok(String::new())
}

#[plugin_fn]
pub fn shutdown(_input: String) -> FnResult<String> {
    state::shutdown_state();
    host::log::log("info", "Sync plugin shut down");
    Ok(String::new())
}

fn command_response(result: Result<JsonValue, String>) -> CommandResponse {
    match result {
        Ok(data) => CommandResponse::ok(data),
        Err(error) => CommandResponse::err(error),
    }
}

fn execute_command(req: CommandRequest) -> CommandResponse {
    let CommandRequest { command, params } = req;

    let result: Option<Result<JsonValue, String>> = match command.as_str() {
        "get_component_html" => {
            let component_id = params
                .get("component_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
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
            Some(Ok(JsonValue::Null))
        }
        "GetSyncStatus" => Some(handle_sync_status(&params)),
        "GetProviderStatus" => Some(handle_get_provider_status(&params)),
        "ListRemoteWorkspaces" => Some(handle_list_remote_workspaces(&params)),
        "LinkWorkspace" => Some(handle_link_workspace(&params)),
        "UnlinkWorkspace" => Some(Ok(handle_unlink_workspace(&params))),
        // Sync commands
        "SyncPush" | "sync_push" => Some(handle_sync_push(&params)),
        "SyncPull" | "sync_pull" => Some(handle_sync_pull(&params)),
        "Sync" | "sync" => Some(handle_sync_full(&params)),
        "SyncStatus" | "sync_status" => Some(handle_sync_status(&params)),
        // Namespace API commands
        "NsCreateNamespace" => Some(handle_ns_create_namespace(&params)),
        "NsListNamespaces" => Some(handle_ns_list_namespaces(&params)),
        "NsPutObject" => Some(handle_ns_put_object(&params)),
        "NsGetObject" => Some(handle_ns_get_object(&params)),
        "NsDeleteObject" => Some(handle_ns_delete_object(&params)),
        "NsListObjects" => Some(handle_ns_list_objects(&params)),
        _ => None,
    };

    if let Some(result) = result {
        return command_response(result);
    }

    CommandResponse::err(format!("Unknown command: {command}"))
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

    match event.event_type.as_str() {
        "file_saved" | "file_created" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                let relative = workspace_relative_path(path);
                state::with_manifest_mut(|m| m.mark_dirty(&format!("files/{relative}")));
            }
        }
        "file_deleted" => {
            if let Some(path) = event.payload.get("path").and_then(|v| v.as_str()) {
                let relative = workspace_relative_path(path);
                state::with_manifest_mut(|m| m.record_delete(&format!("files/{relative}")));
            }
        }
        "file_renamed" | "file_moved" => {
            let old_path = event.payload.get("old_path").and_then(|v| v.as_str());
            let new_path = event.payload.get("new_path").and_then(|v| v.as_str());
            if let (Some(old), Some(new)) = (old_path, new_path) {
                let old_relative = workspace_relative_path(old);
                let new_relative = workspace_relative_path(new);
                state::with_manifest_mut(|m| {
                    m.record_delete(&format!("files/{old_relative}"));
                    m.mark_dirty(&format!("files/{new_relative}"));
                });
            }
        }
        _ => {}
    }

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
    Ok(String::new())
}

/// Execute a typed Command.
#[plugin_fn]
pub fn execute_typed_command(input: String) -> FnResult<String> {
    let parsed: JsonValue = serde_json::from_str(&input)
        .map_err(|e| extism_pdk::Error::msg(format!("Invalid JSON: {e}")))?;

    let cmd_type = parsed["type"]
        .as_str()
        .ok_or_else(|| extism_pdk::Error::msg("Missing 'type' field in command"))?;

    let params = parsed.get("params").cloned().unwrap_or(JsonValue::Null);

    let resp = execute_command(CommandRequest {
        command: cmd_type.to_string(),
        params,
    });

    if resp.success {
        let response = serde_json::json!({ "type": "PluginResult", "data": resp.data });
        Ok(serde_json::to_string(&response)?)
    } else if let Some(ref error) = resp.error {
        if error.starts_with("Unknown command:") {
            Ok(String::new())
        } else {
            Err(extism_pdk::Error::msg(error.clone()).into())
        }
    } else {
        Ok(String::new())
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn workspace_relative_path(path: &str) -> String {
    let root = state::workspace_root().unwrap_or_default();
    let root = root.trim();
    if root.is_empty() || root == "." {
        return path.replace('\\', "/");
    }
    let normalized_root = root.replace('\\', "/");
    let normalized_root = normalized_root.trim_end_matches('/');
    let normalized_path = path.replace('\\', "/");

    if let Some(stripped) = normalized_path.strip_prefix(&format!("{normalized_root}/")) {
        stripped.to_string()
    } else {
        normalized_path
    }
}

fn all_commands() -> Vec<String> {
    vec![
        // Sync
        "SyncPush",
        "SyncPull",
        "Sync",
        "SyncStatus",
        "GetSyncStatus",
        // Provider
        "GetProviderStatus",
        "ListRemoteWorkspaces",
        "LinkWorkspace",
        "UnlinkWorkspace",
        // Iframe Components
        "get_component_html",
        "get_config",
        "set_config",
        // Namespace API
        "NsCreateNamespace",
        "NsListNamespaces",
        "NsPutObject",
        "NsGetObject",
        "NsDeleteObject",
        "NsListObjects",
    ]
    .into_iter()
    .map(String::from)
    .collect()
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

        assert!(
            tab.get("component").unwrap().is_null(),
            "settings tab component should be null (no iframe)"
        );

        let fields = tab
            .get("fields")
            .and_then(|v| v.as_array())
            .expect("fields should be an array");
        assert!(!fields.is_empty(), "fields should not be empty");

        assert_eq!(
            fields[0].get("type").and_then(|v| v.as_str()),
            Some("AuthStatus")
        );

        assert_eq!(
            fields[1].get("type").and_then(|v| v.as_str()),
            Some("UpgradeBanner")
        );

        assert_eq!(
            fields[2].get("type").and_then(|v| v.as_str()),
            Some("Conditional")
        );
    }

    #[test]
    fn manifest_declares_requested_permissions() {
        let manifest = build_manifest();
        let perms = manifest
            .requested_permissions
            .as_ref()
            .expect("manifest should declare requested_permissions");

        assert!(perms.defaults.get("plugin_storage").is_some());
        assert!(perms.defaults.get("http_requests").is_some());
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
    }

    #[test]
    fn workspace_relative_path_strips_root() {
        // Simulate state not initialized — falls back to returning path as-is
        let result = workspace_relative_path("/workspace/doc.md");
        assert_eq!(result, "/workspace/doc.md");
    }

    #[test]
    fn cli_has_push_pull_no_start() {
        let manifest = build_manifest();
        let cli = &manifest.cli;
        let sync_cmd = cli[0].as_object().unwrap();
        let subcommands = sync_cmd["subcommands"].as_array().unwrap();
        let names: Vec<&str> = subcommands
            .iter()
            .filter_map(|s: &JsonValue| s.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"push"));
        assert!(names.contains(&"pull"));
        assert!(!names.contains(&"start"));
    }
}
