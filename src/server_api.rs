//! Namespace-based server API helpers for the new generic resource backend.
//!
//! These functions call the `/namespaces/…` endpoints added in the greenfield
//! server redesign. They complement the existing `/api/workspaces/…` endpoints
//! which remain for backward compatibility.

use serde_json::Value as JsonValue;

use crate::{
    auth_headers, http_error, http_request_binary_compat, http_request_compat, load_extism_config,
    parse_http_body, parse_http_body_json, parse_http_status, resolve_auth_token,
    resolve_server_url,
};

// ---------------------------------------------------------------------------
// Namespaces
// ---------------------------------------------------------------------------

/// POST /namespaces — create a namespace (returns `{ id, owner_user_id, created_at }`).
pub fn create_namespace(
    params: &JsonValue,
    namespace_id: &str,
) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "POST",
        &format!("{server}/namespaces"),
        &headers,
        Some(serde_json::json!({ "id": namespace_id })),
    )?;
    let status = parse_http_status(&response);
    if status != 201 && status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    parse_http_body_json(&response).ok_or_else(|| "Invalid namespace response".to_string())
}

/// GET /namespaces — list namespaces owned by the authenticated user.
pub fn list_namespaces(params: &JsonValue) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "GET",
        &format!("{server}/namespaces"),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    parse_http_body_json(&response).ok_or_else(|| "Invalid response".to_string())
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

/// PUT /namespaces/{ns_id}/objects/{key} — store bytes under the given key.
pub fn put_object(
    params: &JsonValue,
    namespace_id: &str,
    key: &str,
    body: &[u8],
    content_type: &str,
) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let token = resolve_auth_token(params, &config);
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), content_type.to_string()),
    ];
    if let Some(t) = &token {
        headers.push(("Authorization".to_string(), format!("Bearer {}", t)));
    }
    let response = http_request_binary_compat(
        "PUT",
        &format!("{server}/namespaces/{namespace_id}/objects/{key}"),
        &headers,
        body,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    parse_http_body_json(&response).ok_or_else(|| "Invalid response".to_string())
}

/// GET /namespaces/{ns_id}/objects/{key} — retrieve bytes by key.
///
/// Returns raw body bytes as base64-decoded from the response.
pub fn get_object(
    params: &JsonValue,
    namespace_id: &str,
    key: &str,
) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "GET",
        &format!("{server}/namespaces/{namespace_id}/objects/{key}"),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status == 404 {
        return Ok(JsonValue::Null);
    }
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    Ok(response)
}

/// DELETE /namespaces/{ns_id}/objects/{key} — delete an object.
pub fn delete_object(
    params: &JsonValue,
    namespace_id: &str,
    key: &str,
) -> Result<(), String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "DELETE",
        &format!("{server}/namespaces/{namespace_id}/objects/{key}"),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status != 204 && status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    Ok(())
}

/// GET /namespaces/{ns_id}/objects — list object metadata.
pub fn list_objects(
    params: &JsonValue,
    namespace_id: &str,
) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "GET",
        &format!("{server}/namespaces/{namespace_id}/objects"),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    parse_http_body_json(&response).ok_or_else(|| "Invalid response".to_string())
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// POST /sessions — create a session for a namespace.
pub fn create_session(
    params: &JsonValue,
    namespace_id: &str,
    read_only: bool,
) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "POST",
        &format!("{server}/sessions"),
        &headers,
        Some(serde_json::json!({
            "namespace_id": namespace_id,
            "read_only": read_only,
        })),
    )?;
    let status = parse_http_status(&response);
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    parse_http_body_json(&response).ok_or_else(|| "Invalid response".to_string())
}

/// GET /sessions/{code} — look up a session (unauthenticated).
pub fn get_session(
    params: &JsonValue,
    code: &str,
) -> Result<JsonValue, String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "GET",
        &format!("{server}/sessions/{code}"),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status == 404 {
        return Ok(JsonValue::Null);
    }
    if status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    parse_http_body_json(&response).ok_or_else(|| "Invalid response".to_string())
}

/// DELETE /sessions/{code} — end a session (owner only).
pub fn delete_session(
    params: &JsonValue,
    code: &str,
) -> Result<(), String> {
    let config = load_extism_config();
    let server = resolve_server_url(params, &config).ok_or("Missing server_url")?;
    let headers = auth_headers(resolve_auth_token(params, &config));
    let response = http_request_compat(
        "DELETE",
        &format!("{server}/sessions/{code}"),
        &headers,
        None,
    )?;
    let status = parse_http_status(&response);
    if status != 204 && status != 200 {
        return Err(http_error(status, &parse_http_body(&response)));
    }
    Ok(())
}
