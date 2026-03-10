//! Thread-local plugin state for the Extism guest.
//!
//! Since WASM is single-threaded, we use `RefCell`-based global state
//! to hold the SyncPlugin, SyncSession, and related objects.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::Arc;

use diaryx_core::fs::FileSystemEvent;
use diaryx_core::path_utils::normalize_sync_path;
use diaryx_sync::{
    CrdtStorage, MemoryStorage, SyncEvent, SyncPlugin, SyncSession, SyncSessionConfig,
    UpdateOrigin, format_body_doc_id, format_workspace_doc_id,
};

use crate::host_bridge;
use crate::host_fs::HostFs;

thread_local! {
    static SYNC_PLUGIN: RefCell<Option<SyncPlugin<HostFs>>> = const { RefCell::new(None) };
    static SESSION: RefCell<Option<SyncSession<HostFs>>> = const { RefCell::new(None) };
    static WORKSPACE_ID: RefCell<Option<String>> = const { RefCell::new(None) };
    static WRITE_TO_DISK: RefCell<bool> = const { RefCell::new(true) };
    static PENDING_LOCAL_UPDATES: RefCell<Vec<(String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
}

const BODY_DOC_MANIFEST_KEY: &str = "body_doc_manifest";

fn restore_persisted_body_docs(sync_plugin: &SyncPlugin<HostFs>) {
    let mut body_doc_names = BTreeSet::new();
    for (path, _) in sync_plugin.workspace_crdt().list_active_files() {
        body_doc_names.insert(path);
    }
    match host_bridge::storage_get(BODY_DOC_MANIFEST_KEY) {
        Ok(Some(data)) => match serde_json::from_slice::<Vec<String>>(&data) {
            Ok(paths) => {
                for path in paths {
                    body_doc_names.insert(path);
                }
            }
            Err(e) => host_bridge::log_message(
                "warn",
                &format!("Failed to decode persisted body doc manifest: {e}"),
            ),
        },
        Ok(None) => {}
        Err(e) => host_bridge::log_message(
            "warn",
            &format!("Failed to read persisted body doc manifest: {e}"),
        ),
    }
    if body_doc_names.is_empty() {
        return;
    }

    let body_docs = sync_plugin.body_docs();
    for path in body_doc_names {
        let key = format!("body:{path}");
        match host_bridge::storage_get(&key) {
            Ok(Some(data)) => {
                let doc = body_docs.get_or_create(&path);
                if let Err(e) = doc.apply_update(&data, UpdateOrigin::Remote) {
                    host_bridge::log_message(
                        "warn",
                        &format!("Failed to restore persisted body doc {path}: {e}"),
                    );
                }
            }
            Ok(None) => {}
            Err(e) => host_bridge::log_message(
                "warn",
                &format!("Failed to read persisted body doc {path}: {e}"),
            ),
        }
    }
}

/// Initialize the plugin state, restoring persisted CRDT state when available.
pub fn init_state(workspace_id: Option<String>) -> Result<(), &'static str> {
    let fs = HostFs;
    let storage: Arc<dyn CrdtStorage> = Arc::new(MemoryStorage::new());
    let mut restored_workspace = false;

    // Try to load persisted workspace CRDT state
    if let Ok(Some(data)) = host_bridge::storage_get("workspace_crdt") {
        if let Err(e) = storage.save_doc("workspace", &data) {
            host_bridge::log_message("warn", &format!("Failed to restore workspace CRDT: {e}"));
        } else {
            restored_workspace = true;
        }
    }

    let sync_plugin = if restored_workspace {
        match SyncPlugin::load(fs.clone(), Arc::clone(&storage)) {
            Ok(sync_plugin) => sync_plugin,
            Err(e) => {
                host_bridge::log_message(
                    "warn",
                    &format!("Failed to load persisted sync state, starting fresh: {e}"),
                );
                SyncPlugin::new(fs.clone(), Arc::clone(&storage))
            }
        }
    } else {
        SyncPlugin::new(fs.clone(), Arc::clone(&storage))
    };

    restore_persisted_body_docs(&sync_plugin);

    SYNC_PLUGIN.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Sync plugin state already mutably borrowed")?;
        *borrow = Some(sync_plugin);
        Ok::<_, &'static str>(())
    })?;
    SESSION.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Sync session state already mutably borrowed")?;
        *borrow = None;
        Ok::<_, &'static str>(())
    })?;
    WORKSPACE_ID.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Workspace identity state already mutably borrowed")?;
        *borrow = workspace_id;
        Ok::<_, &'static str>(())
    })?;
    WRITE_TO_DISK.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Write-to-disk state already mutably borrowed")?;
        *borrow = true;
        Ok::<_, &'static str>(())
    })?;
    Ok(())
}

/// Access the sync plugin without borrowing the session state.
pub fn with_sync_plugin<F, R>(f: F) -> Result<R, &'static str>
where
    F: FnOnce(&SyncPlugin<HostFs>) -> R,
{
    SYNC_PLUGIN.with(|s| {
        let borrow = s
            .try_borrow()
            .map_err(|_| "Sync plugin state already mutably borrowed")?;
        let plugin = borrow.as_ref().ok_or("Plugin state not initialized")?;
        Ok(f(plugin))
    })
}

pub fn has_session() -> Result<bool, &'static str> {
    SESSION.with(|s| {
        let borrow = s
            .try_borrow()
            .map_err(|_| "Sync session state already mutably borrowed")?;
        Ok(borrow.is_some())
    })
}

pub fn get_write_to_disk() -> Result<bool, &'static str> {
    WRITE_TO_DISK.with(|s| {
        let borrow = s
            .try_borrow()
            .map_err(|_| "Write-to-disk state already mutably borrowed")?;
        Ok(*borrow)
    })
}

/// Mutably access the current sync session without holding a RefCell borrow
/// for the duration of the session callback.
pub fn with_session_mut<F, R>(f: F) -> Result<Option<R>, &'static str>
where
    F: FnOnce(&mut SyncSession<HostFs>) -> R,
{
    let mut session = SESSION.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Sync session state already mutably borrowed (session_take)")?;
        Ok::<_, &'static str>(borrow.take())
    })?;

    let result = session.as_mut().map(f);

    SESSION.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Sync session state already mutably borrowed (session_restore)")?;
        *borrow = session;
        Ok::<_, &'static str>(())
    })?;

    Ok(result)
}

/// Create a SyncSession from current state.
pub fn create_session(workspace_id: &str, write_to_disk: bool) -> Result<(), &'static str> {
    let config = SyncSessionConfig {
        workspace_id: workspace_id.to_string(),
        write_to_disk,
    };
    let workspace_id = workspace_id.to_string();
    let callback_workspace_id = workspace_id.clone();
    let sync_manager = with_sync_plugin(|sync_plugin| {
        sync_plugin
            .sync_manager()
            .set_event_callback(Arc::new(move |event| {
                if let FileSystemEvent::SendSyncMessage {
                    doc_name,
                    message,
                    is_body,
                } = event
                {
                    let doc_id = if *is_body {
                        let canonical = normalize_sync_path(doc_name);
                        if canonical.is_empty() {
                            host_bridge::log_message(
                                "warn",
                                &format!(
                                    "[state] Dropping body sync message with empty canonical path (raw='{}')",
                                    doc_name
                                ),
                            );
                            return;
                        }
                        format_body_doc_id(&callback_workspace_id, &canonical)
                    } else {
                        format_workspace_doc_id(&callback_workspace_id)
                    };
                    enqueue_local_update(doc_id, message.clone());
                }
            }));
        sync_plugin.sync_manager()
    })?;
    let session = SyncSession::new(config, sync_manager);
    SESSION.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Sync session state already mutably borrowed")?;
        *borrow = Some(session);
        Ok::<_, &'static str>(())
    })?;
    WORKSPACE_ID.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Workspace identity state already mutably borrowed")?;
        *borrow = Some(workspace_id);
        Ok::<_, &'static str>(())
    })?;
    WRITE_TO_DISK.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Write-to-disk state already mutably borrowed")?;
        *borrow = write_to_disk;
        Ok::<_, &'static str>(())
    })?;
    Ok(())
}

/// Queue a local update for later processing outside the sync callback.
pub fn enqueue_local_update(doc_id: String, data: Vec<u8>) {
    if doc_id.is_empty() || data.is_empty() {
        return;
    }
    host_bridge::log_message(
        "debug",
        &format!(
            "[local_update] queued doc_id={} bytes={}",
            doc_id,
            data.len()
        ),
    );
    PENDING_LOCAL_UPDATES.with(|pending| {
        pending.borrow_mut().push((doc_id, data));
    });
}

/// Drain local updates queued by sync manager callbacks.
pub fn drain_local_updates() -> Vec<(String, Vec<u8>)> {
    PENDING_LOCAL_UPDATES.with(|pending| std::mem::take(&mut *pending.borrow_mut()))
}

/// Persist the current CRDT state via host storage.
pub fn persist_state() -> Result<(), &'static str> {
    SYNC_PLUGIN.with(|s| {
        let borrow = s
            .try_borrow()
            .map_err(|_| "Sync plugin state already mutably borrowed")?;
        let sync_plugin = borrow.as_ref().ok_or("Plugin state not initialized")?;

        // Save workspace CRDT
        let ws_crdt = sync_plugin.workspace_crdt();
        let ws_state = ws_crdt.encode_state_as_update();
        if let Err(e) = host_bridge::storage_set("workspace_crdt", &ws_state) {
            host_bridge::log_message("warn", &format!("Failed to persist workspace CRDT: {e}"));
        }

        // Save all body docs
        let body_docs = sync_plugin.body_docs();
        if let Err(e) = body_docs.save_all() {
            host_bridge::log_message("warn", &format!("Failed to save body docs: {e}"));
        }

        // Persist body docs for all active files plus any still-loaded transient docs.
        // Refresh/reload restores from host storage, so relying only on currently loaded
        // body docs causes focused-file content to duplicate when a doc was unloaded
        // before shutdown and later re-seeded from plain disk text.
        let mut body_doc_names = BTreeSet::new();
        for (path, _) in ws_crdt.list_active_files() {
            body_doc_names.insert(path);
        }
        for doc_name in body_docs.loaded_docs() {
            body_doc_names.insert(doc_name);
        }

        for doc_name in &body_doc_names {
            if let Some(doc) = body_docs.get(&doc_name) {
                let doc_state = doc.encode_state_as_update();
                let key = format!("body:{doc_name}");
                if let Err(e) = host_bridge::storage_set(&key, &doc_state) {
                    host_bridge::log_message(
                        "warn",
                        &format!("Failed to persist body doc {doc_name}: {e}"),
                    );
                }
            }
        }

        let manifest = body_doc_names.into_iter().collect::<Vec<_>>();
        if let Ok(data) = serde_json::to_vec(&manifest) {
            if let Err(e) = host_bridge::storage_set(BODY_DOC_MANIFEST_KEY, &data) {
                host_bridge::log_message(
                    "warn",
                    &format!("Failed to persist body doc manifest: {e}"),
                );
            }
        }
        Ok(())
    })
}

/// Emit a sync event to the host.
pub fn emit_sync_event(event: &SyncEvent) {
    let payload = match event {
        SyncEvent::StatusChanged { status } => serde_json::json!({
            "type": "SyncStatusChanged",
            "status": match status {
                diaryx_sync::SyncStatus::Connecting => "connecting",
                diaryx_sync::SyncStatus::Connected => "connected",
                diaryx_sync::SyncStatus::Syncing => "syncing",
                diaryx_sync::SyncStatus::Synced => "synced",
                diaryx_sync::SyncStatus::Reconnecting { .. } => "reconnecting",
                diaryx_sync::SyncStatus::Disconnected => "disconnected",
            }
        }),
        SyncEvent::Progress { completed, total } => serde_json::json!({
            "type": "SyncProgress",
            "completed": completed,
            "total": total,
        }),
        SyncEvent::FilesChanged { files } => serde_json::json!({
            "type": "SyncCompleted",
            "doc_name": "",
            "files_synced": files.len(),
        }),
        SyncEvent::BodyChanged { file_path, body } => serde_json::json!({
            "type": "ContentsChanged",
            "path": file_path,
            "body": body,
        }),
        SyncEvent::Error { message } => serde_json::json!({
            "type": "SyncStatusChanged",
            "status": "error",
            "error": message,
        }),
        SyncEvent::PeerJoined { peer_count } => serde_json::json!({
            "type": "PeerJoined",
            "peer_count": peer_count,
        }),
        SyncEvent::PeerLeft { peer_count } => serde_json::json!({
            "type": "PeerLeft",
            "peer_count": peer_count,
        }),
        SyncEvent::SyncComplete { files_synced } => serde_json::json!({
            "type": "SyncCompleted",
            "doc_name": "",
            "files_synced": files_synced,
        }),
        SyncEvent::FocusListChanged { files } => serde_json::json!({
            "type": "FocusListChanged",
            "files": files,
        }),
    };

    if let Ok(json) = serde_json::to_string(&payload) {
        let _ = host_bridge::emit_event(&json);
    }
}

/// Shut down the plugin state.
pub fn shutdown_state() -> Result<(), &'static str> {
    if let Err(e) = persist_state() {
        host_bridge::log_message(
            "warn",
            &format!("Failed to persist plugin state on shutdown: {e}"),
        );
    }
    SESSION.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Sync session state already mutably borrowed")?;
        *borrow = None;
        Ok::<_, &'static str>(())
    })?;
    SYNC_PLUGIN.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Sync plugin state already mutably borrowed")?;
        *borrow = None;
        Ok::<_, &'static str>(())
    })?;
    WORKSPACE_ID.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Workspace identity state already mutably borrowed")?;
        *borrow = None;
        Ok::<_, &'static str>(())
    })?;
    WRITE_TO_DISK.with(|s| {
        let mut borrow = s
            .try_borrow_mut()
            .map_err(|_| "Write-to-disk state already mutably borrowed")?;
        *borrow = true;
        Ok::<_, &'static str>(())
    })?;
    Ok(())
}

/// Check if state is initialized.
pub fn is_initialized() -> bool {
    SYNC_PLUGIN.with(|s| match s.try_borrow() {
        Ok(borrow) => borrow.is_some(),
        Err(_) => true,
    })
}
