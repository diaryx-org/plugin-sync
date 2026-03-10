//! Integration tests for the diaryx-sync Extism plugin.
//!
//! These tests load the pre-built WASM module via `PluginTestHarness` and
//! exercise the plugin's exports through the Extism runtime.
//!
//! Prerequisites: `cargo build --target wasm32-unknown-unknown --release`

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diaryx_core::plugin::manifest::{PluginCapability, UiContribution};
use diaryx_extism::testing::*;
use serde_json::{Value as JsonValue, json};
use tokio::time::timeout;

const WASM_PATH: &str = "target/wasm32-unknown-unknown/release/diaryx_sync_extism.wasm";

/// Early-return if the WASM file hasn't been built.
macro_rules! require_wasm {
    () => {
        if !std::path::Path::new(WASM_PATH).exists() {
            eprintln!(
                "Skipping: WASM not built. Run: cargo build --target wasm32-unknown-unknown --release"
            );
            return;
        }
    };
}

fn load_sync_plugin() -> PluginTestHarness {
    PluginTestHarness::load(WASM_PATH).expect("Failed to load sync plugin WASM")
}

fn load_with_storage(storage: Arc<RecordingStorage>) -> PluginTestHarness {
    PluginTestHarnessBuilder::new(WASM_PATH)
        .with_storage(storage)
        .build()
        .expect("Failed to load sync plugin WASM")
}

fn load_with_storage_and_emitter(
    storage: Arc<RecordingStorage>,
    emitter: Arc<RecordingEventEmitter>,
) -> PluginTestHarness {
    PluginTestHarnessBuilder::new(WASM_PATH)
        .with_storage(storage)
        .with_event_emitter(emitter)
        .build()
        .expect("Failed to load sync plugin WASM")
}

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("diaryx-plugin-sync-{label}-{nanos}"))
}

// ============================================================================
// Category 1: Manifest & Metadata
// ============================================================================

#[test]
fn manifest_has_correct_id_and_name() {
    require_wasm!();
    let harness = load_sync_plugin();
    let manifest = harness.manifest();
    assert_eq!(manifest.id.0, "diaryx.sync");
    assert_eq!(manifest.name, "Sync");
}

#[test]
fn manifest_declares_expected_capabilities() {
    require_wasm!();
    let harness = load_sync_plugin();
    let manifest = harness.manifest();

    let has_file_events = manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, PluginCapability::FileEvents));
    let has_workspace_events = manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, PluginCapability::WorkspaceEvents));
    let has_crdt_commands = manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, PluginCapability::CrdtCommands));
    let has_sync_transport = manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, PluginCapability::SyncTransport));
    let has_custom_commands = manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, PluginCapability::CustomCommands { .. }));

    assert!(has_file_events, "Missing FileEvents capability");
    assert!(has_workspace_events, "Missing WorkspaceEvents capability");
    assert!(has_crdt_commands, "Missing CrdtCommands capability");
    assert!(has_sync_transport, "Missing SyncTransport capability");
    assert!(has_custom_commands, "Missing CustomCommands capability");
}

#[test]
fn manifest_declares_commands() {
    require_wasm!();
    let harness = load_sync_plugin();
    let manifest = harness.manifest();

    // Commands are embedded in CustomCommands capability
    let commands: Vec<&str> = manifest
        .capabilities
        .iter()
        .filter_map(|c| match c {
            PluginCapability::CustomCommands { commands } => Some(commands),
            _ => None,
        })
        .flatten()
        .map(|s| s.as_str())
        .collect();

    for expected in [
        "GetSyncStatus",
        "SyncFocusedFileNow",
        "GetProviderStatus",
        "get_config",
        "set_config",
        "ListRemoteWorkspaces",
        "LinkWorkspace",
    ] {
        assert!(
            commands.contains(&expected),
            "Missing command: {expected}. Got: {commands:?}"
        );
    }
}

#[test]
fn manifest_declares_ui_contributions() {
    require_wasm!();
    let harness = load_sync_plugin();
    let manifest = harness.manifest();

    assert!(
        !manifest.ui.is_empty(),
        "Manifest should declare UI contributions"
    );

    let has_settings_tab = manifest
        .ui
        .iter()
        .any(|ui| matches!(ui, UiContribution::SettingsTab { id, .. } if id == "sync-settings"));
    assert!(has_settings_tab, "Should have sync-settings SettingsTab");

    let has_sidebar_tab = manifest.ui.iter().any(|ui| {
        matches!(ui, UiContribution::SidebarTab { id, .. } if id == "snapshots" || id == "history")
    });
    assert!(has_sidebar_tab, "Should have sidebar tabs");

    let has_status_bar = manifest
        .ui
        .iter()
        .any(|ui| matches!(ui, UiContribution::StatusBarItem { .. }));
    assert!(has_status_bar, "Should have StatusBarItem");
}

// ============================================================================
// Category 2: Init & Lifecycle
// ============================================================================

#[tokio::test]
async fn init_succeeds() {
    require_wasm!();
    let harness = load_sync_plugin();
    let result = harness.init().await;
    assert!(result.is_ok(), "Init should succeed: {result:?}");
}

#[tokio::test]
async fn init_with_workspace_root() {
    require_wasm!();
    let tmp = std::env::temp_dir().join("diaryx-test-workspace");
    let _ = std::fs::create_dir_all(&tmp);

    let harness = PluginTestHarnessBuilder::new(WASM_PATH)
        .with_workspace_root(&tmp)
        .build()
        .expect("Failed to load");

    let result = harness.init().await;
    assert!(
        result.is_ok(),
        "Init with workspace root should succeed: {result:?}"
    );
}

#[tokio::test]
async fn shutdown_persists_crdt_state() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let harness = load_with_storage(storage.clone());

    harness.init().await.expect("init should succeed");

    // Call shutdown to persist state
    let _ = harness.call_raw("shutdown", "");

    // Check that workspace_crdt was written to storage (key is prefixed with plugin id)
    let ops = storage.ops();
    let has_crdt_set = ops
        .iter()
        .any(|op| matches!(op, StorageOp::Set(key, _) if key.ends_with("workspace_crdt")));
    assert!(
        has_crdt_set,
        "Shutdown should persist workspace_crdt to storage. Ops: {ops:?}"
    );
}

// ============================================================================
// Category 3: Config Management
// ============================================================================

#[tokio::test]
async fn get_config_returns_defaults_when_empty() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command("get_config", json!({}))
        .await
        .expect("get_config should return Some")
        .expect("get_config should succeed");

    // Default config should be a JSON object
    assert!(result.is_object(), "get_config should return a JSON object");
}

#[tokio::test]
async fn set_config_stores_server_url() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let harness = load_with_storage(storage.clone());
    harness.init().await.expect("init");

    // Set server_url
    harness
        .command(
            "set_config",
            json!({ "server_url": "https://test.example.com" }),
        )
        .await
        .expect("set_config should return Some")
        .expect("set_config should succeed");

    // Read it back
    let config = harness
        .command("get_config", json!({}))
        .await
        .expect("get_config should return Some")
        .expect("get_config should succeed");

    assert_eq!(
        config.get("server_url").and_then(|v| v.as_str()),
        Some("https://test.example.com"),
        "server_url should be stored. Got: {config}"
    );
}

#[tokio::test]
async fn set_config_roundtrip() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let harness = load_with_storage(storage);
    harness.init().await.expect("init");

    // Set all three fields
    harness
        .command(
            "set_config",
            json!({
                "server_url": "https://sync.example.com",
                "auth_token": "test-token-123",
                "workspace_id": "ws-abc-456"
            }),
        )
        .await
        .expect("Some")
        .expect("set_config should succeed");

    // Read back
    let config = harness
        .command("get_config", json!({}))
        .await
        .expect("Some")
        .expect("get_config should succeed");

    assert_eq!(
        config.get("server_url").and_then(|v| v.as_str()),
        Some("https://sync.example.com")
    );
    assert_eq!(
        config.get("auth_token").and_then(|v| v.as_str()),
        None,
        "effective config should redact auth_token and rely on runtime context"
    );
    assert_eq!(
        config.get("workspace_id").and_then(|v| v.as_str()),
        Some("ws-abc-456")
    );
}

#[tokio::test]
async fn set_config_null_clears_fields() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let harness = load_with_storage(storage);
    harness.init().await.expect("init");

    // Set a field
    harness
        .command(
            "set_config",
            json!({ "server_url": "https://sync.example.com" }),
        )
        .await
        .expect("Some")
        .expect("set_config");

    // Clear it with null
    harness
        .command("set_config", json!({ "server_url": null }))
        .await
        .expect("Some")
        .expect("set_config should succeed");

    // Verify cleared
    let config = harness
        .command("get_config", json!({}))
        .await
        .expect("Some")
        .expect("get_config");

    let server_url = config.get("server_url");
    assert!(
        server_url.is_none() || server_url == Some(&JsonValue::Null),
        "server_url should be cleared after setting null. Got: {config}"
    );
}

// ============================================================================
// Category 4: Status Commands
// ============================================================================

#[tokio::test]
async fn get_sync_status_returns_idle_initially() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command("GetSyncStatus", json!({}))
        .await
        .expect("GetSyncStatus should return Some")
        .expect("GetSyncStatus should succeed");

    assert_eq!(
        result.get("state").and_then(|v| v.as_str()),
        Some("idle"),
        "Initial sync status should be idle. Got: {result}"
    );
    assert_eq!(
        result.get("label").and_then(|v| v.as_str()),
        Some("Idle"),
        "Initial sync label should be Idle. Got: {result}"
    );
}

#[tokio::test]
async fn get_provider_status_not_ready_without_config() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command("GetProviderStatus", json!({}))
        .await
        .expect("GetProviderStatus should return Some")
        .expect("GetProviderStatus should succeed");

    assert_eq!(
        result.get("ready").and_then(|v| v.as_bool()),
        Some(false),
        "Provider should not be ready without credentials. Got: {result}"
    );
}

#[tokio::test]
async fn get_provider_status_ready_with_credentials() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let harness = load_with_storage(storage);
    harness.init().await.expect("init");

    // Configure server and auth
    harness
        .command(
            "set_config",
            json!({
                "server_url": "https://sync.example.com",
                "auth_token": "valid-token"
            }),
        )
        .await
        .expect("Some")
        .expect("set_config");

    let result = harness
        .command("GetProviderStatus", json!({}))
        .await
        .expect("Some")
        .expect("GetProviderStatus should succeed");

    assert_eq!(
        result.get("ready").and_then(|v| v.as_bool()),
        Some(true),
        "Provider should be ready with server_url + auth_token. Got: {result}"
    );
}

// ============================================================================
// Category 5: CRDT State Commands
// ============================================================================

#[tokio::test]
async fn get_sync_state_after_init() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command("GetSyncState", json!({}))
        .await
        .expect("GetSyncState should return Some")
        .expect("GetSyncState should succeed");

    assert!(
        !result.is_null(),
        "GetSyncState should return non-null data. Got: {result}"
    );
}

#[tokio::test]
async fn list_crdt_files_empty_initially() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command("ListCrdtFiles", json!({}))
        .await
        .expect("ListCrdtFiles should return Some")
        .expect("ListCrdtFiles should succeed");

    let files = result
        .as_array()
        .expect("ListCrdtFiles should return an array");
    assert!(
        files.is_empty(),
        "CRDT files should be empty initially. Got: {result}"
    );
}

#[tokio::test]
async fn initialize_workspace_crdt_rejects_invalid_workspace() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    // An empty temp directory is not a valid workspace
    let tmp = std::env::temp_dir().join("diaryx-test-init-crdt-empty");
    let _ = std::fs::create_dir_all(&tmp);

    let result = harness
        .command(
            "InitializeWorkspaceCrdt",
            json!({ "workspace_path": tmp.to_string_lossy() }),
        )
        .await
        .expect("InitializeWorkspaceCrdt should return Some");

    assert!(
        result.is_err(),
        "InitializeWorkspaceCrdt should fail for invalid workspace. Got: {result:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn initialize_workspace_crdt_reloads_new_files_after_local_create() {
    require_wasm!();

    let workspace_dir = unique_temp_dir("reinit-after-create");
    std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");

    let readme_path = workspace_dir.join("README.md");
    let child_path = workspace_dir.join("live-create.md");

    std::fs::write(
        &readme_path,
        "---\n\
title: Workspace Root\n\
contents: []\n\
---\n\
\n\
Root body\n",
    )
    .expect("write README");

    let harness = PluginTestHarnessBuilder::new(WASM_PATH)
        .with_workspace_root(&workspace_dir)
        .build()
        .expect("Failed to load");
    harness.init().await.expect("init");

    timeout(
        Duration::from_secs(5),
        harness.command(
            "InitializeWorkspaceCrdt",
            json!({ "workspace_path": readme_path.to_string_lossy() }),
        ),
    )
    .await
    .expect("initial InitializeWorkspaceCrdt timed out")
    .expect("InitializeWorkspaceCrdt should return Some")
    .expect("initial InitializeWorkspaceCrdt should succeed");

    std::fs::write(
        &child_path,
        "---\n\
title: Live Create\n\
part_of: /README.md\n\
---\n\
\n\
Child body\n",
    )
    .expect("write child");
    std::fs::write(
        &readme_path,
        "---\n\
title: Workspace Root\n\
contents:\n\
  - live-create.md\n\
---\n\
\n\
Root body\n",
    )
    .expect("update README");

    harness.send_file_created("live-create.md").await;
    harness.send_file_saved("README.md").await;

    timeout(
        Duration::from_secs(5),
        harness.command(
            "InitializeWorkspaceCrdt",
            json!({ "workspace_path": readme_path.to_string_lossy() }),
        ),
    )
    .await
    .expect("second InitializeWorkspaceCrdt timed out")
    .expect("InitializeWorkspaceCrdt should return Some")
    .expect("second InitializeWorkspaceCrdt should succeed");

    let files = harness
        .command("ListCrdtFiles", json!({}))
        .await
        .expect("ListCrdtFiles should return Some")
        .expect("ListCrdtFiles should succeed");
    let files = files
        .as_array()
        .expect("ListCrdtFiles should return an array");
    assert!(
        files.iter().any(|value| {
            value
                .as_array()
                .and_then(|entry| entry.first())
                .and_then(|path| path.as_str())
                == Some("live-create.md")
        }),
        "expected live-create.md in CRDT after second init, got {files:?}"
    );

    let _ = std::fs::remove_dir_all(&workspace_dir);
}

#[tokio::test]
async fn set_and_get_crdt_file() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    // Set a CRDT file entry
    harness
        .command(
            "SetCrdtFile",
            json!({
                "path": "docs/test-entry.md",
                "metadata": {
                    "title": "Test Entry",
                    "created": "2025-01-01T00:00:00Z",
                    "modified": "2025-01-01T00:00:00Z"
                }
            }),
        )
        .await
        .expect("SetCrdtFile should return Some")
        .expect("SetCrdtFile should succeed");

    // Read it back
    let get_result = harness
        .command("GetCrdtFile", json!({ "path": "docs/test-entry.md" }))
        .await
        .expect("GetCrdtFile should return Some")
        .expect("GetCrdtFile should succeed");

    // Should contain the path we set
    let result_str = serde_json::to_string(&get_result).unwrap();
    assert!(
        result_str.contains("test-entry") || result_str.contains("Test Entry"),
        "GetCrdtFile should return the file we set. Got: {get_result}"
    );
}

#[tokio::test]
async fn trigger_workspace_sync_emits_send_sync_message() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let harness = load_with_storage_and_emitter(storage, Arc::new(RecordingEventEmitter::new()));
    harness.init().await.expect("init");

    harness
        .command(
            "SetCrdtFile",
            json!({
                "path": "docs/test-entry.md",
                "metadata": {
                    "filename": "test-entry.md",
                    "title": "Test Entry",
                    "part_of": null,
                    "contents": null,
                    "attachments": [],
                    "deleted": false,
                    "audience": null,
                    "description": "Propagation target",
                    "extra": {},
                    "modified_at": 1
                }
            }),
        )
        .await
        .expect("Trigger setup should return Some")
        .expect("SetCrdtFile should succeed");

    let prepare = harness
        .call_raw(
            "handle_command",
            &json!({
                "command": "PrepareLiveShareRuntime",
                "params": {
                    "workspace_id": "workspace-1",
                    "write_to_disk": true
                }
            })
            .to_string(),
        )
        .expect("handle_command should succeed");
    let prepared: JsonValue =
        serde_json::from_str(&prepare).expect("prepare response should be valid json");

    assert_eq!(
        prepared.get("success").and_then(|value| value.as_bool()),
        Some(true),
        "PrepareLiveShareRuntime should succeed. Got: {prepared}"
    );

    let raw = harness
        .call_raw(
            "handle_command",
            &json!({
                "command": "TriggerWorkspaceSync",
                "params": {}
            })
            .to_string(),
        )
        .expect("handle_command should succeed");
    let result: JsonValue = serde_json::from_str(&raw).expect("response should be valid json");

    assert_eq!(
        result.get("success").and_then(|value| value.as_bool()),
        Some(true),
        "TriggerWorkspaceSync should succeed. Got: {result}"
    );
    assert_eq!(
        result
            .get("data")
            .and_then(|value| value.get("emitted"))
            .and_then(|value| value.as_bool()),
        Some(true),
        "TriggerWorkspaceSync should report an emitted update. Got: {result}"
    );

    assert!(
        result
            .get("data")
            .and_then(|value| value.get("data"))
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty()),
        "TriggerWorkspaceSync should return a non-empty workspace update payload. Got: {result}"
    );
}

// ============================================================================
// Category 6: Component HTML
// ============================================================================

#[tokio::test]
async fn get_component_html_snapshots() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command(
            "get_component_html",
            json!({ "component_id": "sync.snapshots" }),
        )
        .await
        .expect("get_component_html should return Some")
        .expect("get_component_html should succeed");

    let html = result.as_str().expect("Should return HTML string");
    assert!(
        html.contains("<") && html.contains(">"),
        "Should return valid HTML. Got first 200 chars: {}",
        &html[..html.len().min(200)]
    );
}

#[tokio::test]
async fn get_component_html_unknown_returns_error() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command(
            "get_component_html",
            json!({ "component_id": "nonexistent.component" }),
        )
        .await
        .expect("Should return Some");

    assert!(
        result.is_err(),
        "Unknown component ID should return an error. Got: {result:?}"
    );
}

// ============================================================================
// Category 7: Network-Dependent Error Paths
// ============================================================================

#[tokio::test]
async fn list_remote_workspaces_fails_without_server() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command("ListRemoteWorkspaces", json!({}))
        .await
        .expect("Should return Some");

    assert!(
        result.is_err(),
        "ListRemoteWorkspaces should fail without server config. Got: {result:?}"
    );
}

#[tokio::test]
async fn link_workspace_fails_without_server() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    let result = harness
        .command("LinkWorkspace", json!({}))
        .await
        .expect("Should return Some");

    assert!(
        result.is_err(),
        "LinkWorkspace should fail without server config. Got: {result:?}"
    );
}

// ============================================================================
// Category 8: Events
// ============================================================================

#[tokio::test]
async fn file_created_event_does_not_crash() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let emitter = Arc::new(RecordingEventEmitter::new());
    let harness = load_with_storage_and_emitter(storage, emitter);
    harness.init().await.expect("init");

    // Should not panic
    harness.send_file_created("docs/new-entry.md").await;
}

#[tokio::test]
async fn file_saved_event_does_not_crash() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let emitter = Arc::new(RecordingEventEmitter::new());
    let harness = load_with_storage_and_emitter(storage, emitter);
    harness.init().await.expect("init");

    // Should not panic
    harness.send_file_saved("docs/existing-entry.md").await;
}

#[tokio::test]
async fn file_deleted_event_does_not_crash() {
    require_wasm!();
    let storage = Arc::new(RecordingStorage::new());
    let emitter = Arc::new(RecordingEventEmitter::new());
    let harness = load_with_storage_and_emitter(storage, emitter);
    harness.init().await.expect("init");

    // Should not panic
    harness.send_file_deleted("docs/removed-entry.md").await;
}

#[tokio::test]
async fn file_deleted_event_tombstones_crdt_entry() {
    require_wasm!();
    let harness = load_sync_plugin();
    harness.init().await.expect("init");

    harness
        .command(
            "SetCrdtFile",
            json!({
                "path": "docs/removed-entry.md",
                "metadata": {
                    "filename": "removed-entry.md",
                    "title": "Removed Entry",
                    "part_of": null,
                    "contents": null,
                    "attachments": [],
                    "deleted": false,
                    "audience": null,
                    "description": "Delete me",
                    "extra": {},
                    "modified_at": 1
                }
            }),
        )
        .await
        .expect("SetCrdtFile should return Some")
        .expect("SetCrdtFile should succeed");

    harness.send_file_deleted("docs/removed-entry.md").await;

    let active = harness
        .command("ListCrdtFiles", json!({}))
        .await
        .expect("ListCrdtFiles should return Some")
        .expect("ListCrdtFiles should succeed");
    let active_files = active
        .as_array()
        .expect("ListCrdtFiles should return an array");
    assert!(
        active_files.iter().all(|entry| {
            entry
                .as_array()
                .and_then(|tuple| tuple.first())
                .and_then(|value| value.as_str())
                != Some("docs/removed-entry.md")
        }),
        "deleted file should drop out of active CRDT listings. Got: {active}"
    );

    let all_files = harness
        .command("ListCrdtFiles", json!({ "include_deleted": true }))
        .await
        .expect("ListCrdtFiles should return Some")
        .expect("ListCrdtFiles should succeed");
    let removed = all_files
        .as_array()
        .expect("ListCrdtFiles should return an array")
        .iter()
        .find_map(|entry| {
            let tuple = entry.as_array()?;
            if tuple.first().and_then(|value| value.as_str()) == Some("docs/removed-entry.md") {
                tuple.get(1)
            } else {
                None
            }
        })
        .expect("deleted entry should remain as a tombstone");

    assert_eq!(
        removed.get("deleted").and_then(|value| value.as_bool()),
        Some(true),
        "deleted file should remain tombstoned. Got: {all_files}"
    );
}
