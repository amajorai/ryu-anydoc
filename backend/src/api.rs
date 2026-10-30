//! HTTP routes for the Core provider contract and the standalone API.

use axum::{
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    convert::{
        format_name, parse_format, ConversionFailure, ExtractionResult, ANYDOC_LIBRARY_VERSION,
        BACKEND, SUPPORTED_EXTENSIONS,
    },
    jobs::{convert_input, InputSource, JobStatus, JobSubmitError, PreparedInput},
    paths::{filename_from_path, resolve_input, safe_basename, validate_sha256, InputError},
    state::AnyDocState,
};

pub const MOUNT: &str = "/api/anydoc";
pub const API_VERSION: &str = "v1";

/// Build the complete router used both by Core's managed sidecar and by a
/// directly deployed AnyDoc service.
pub fn router(state: AnyDocState) -> Router {
    let protected = Router::new()
        // Core fetches this once to derive any manifest-declared HTTP tools. It is
        // protected because the document discloses the full writable API shape.
        .route("/openapi.json", get(openapi))
        .route("/api/anydoc/capability", get(capability))
        .route("/api/anydoc/parse", post(parse))
        .route("/api/anydoc/jobs", get(list_jobs))
        .route("/api/anydoc/jobs/:job_id", get(get_job).delete(cancel_job))
        // The same versioned API is available through a Ryu node's public_mount
        // and at the root when this binary is deployed as a standalone service.
        .route("/api/anydoc/v1/capability", get(public_capability))
        .route("/api/anydoc/v1/extract", post(public_extract))
        .route("/v1/capability", get(public_capability))
        .route("/v1/extract", post(public_extract))
        // GET /health is the only unauthenticated health route. A POST is routed
        // through the gate so adding a body to health can never open a hole.
        .route("/health", post(health_post))
        .layer(from_fn_with_state(state.clone(), bearer_gate));

    Router::new()
        .route("/health", get(health))
        .route("/api/anydoc/health", get(health))
        .merge(protected)
        .layer(axum::extract::DefaultBodyLimit::max(
            state.limits.max_http_body_bytes,
        ))
        .with_state(state)
}

async fn bearer_gate(State(state): State<AnyDocState>, request: Request, next: Next) -> Response {
    let request_id = request_id(request.headers());
    if !state
        .auth
        .authorized(request.uri().path(), request.headers())
    {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid bearer API key is required",
            Value::Null,
            &request_id,
        );
    }
    let mut response = next.run(request).await;
    set_request_id(&mut response, &request_id);
    response
}

async fn health(State(state): State<AnyDocState>) -> Response {
    Json(json!({
        "ok": true,
        "backend": BACKEND,
        "version": env!("CARGO_PKG_VERSION"),
        "available": true,
        "library_version": ANYDOC_LIBRARY_VERSION,
        "missing_dependencies": [],
        "api_version": API_VERSION,
        "max_input_bytes": state.limits.max_input_bytes,
    }))
    .into_response()
}

async fn health_post(State(_state): State<AnyDocState>, headers: HeaderMap) -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "only GET /health is available",
        Value::Null,
        &request_id(&headers),
    )
}

async fn capability(State(state): State<AnyDocState>, headers: HeaderMap) -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "capability": "document.parse",
            "backend": BACKEND,
            "version": env!("CARGO_PKG_VERSION"),
            "available": true,
            "library_version": ANYDOC_LIBRARY_VERSION,
            "formats": SUPPORTED_EXTENSIONS,
            "system_dependencies": [],
            "missing_dependencies": [],
            "limits": limits_json(&state),
        }),
        &request_id(&headers),
    )
}

async fn public_capability(State(state): State<AnyDocState>, headers: HeaderMap) -> Response {
    let request_id = request_id(&headers);
    let mut response = json_response(
        StatusCode::OK,
        json!({
            "apiVersion": API_VERSION,
            "backend": BACKEND,
            "backendVersion": ANYDOC_LIBRARY_VERSION,
            "formats": SUPPORTED_EXTENSIONS,
            "limits": {
                "maxInputBytes": state.limits.max_input_bytes,
                "maxOutputBytes": state.limits.max_output_bytes,
                "timeoutSeconds": state.limits.timeout_secs(),
            },
        }),
        &request_id,
    );
    set_request_id(&mut response, &request_id);
    response
}

async fn parse(State(state): State<AnyDocState>, headers: HeaderMap, body: Bytes) -> Response {
    let request_id = request_id(&headers);
    if !is_json(&headers) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "the provider parse route expects an application/json body",
            Value::Null,
            &request_id,
        );
    }
    let request = match decode_json_request(&body, &state, &request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let input = match prepare_json_input(request, &state, state.auth.is_core_hop(&headers)) {
        Ok(input) => input,
        Err(error) => return input_error_response(error, &request_id),
    };
    // `None` is the Core/system scope; a standalone API key resolves to its
    // configured tenant and can only see jobs in that same scope.
    let tenant_id = state.auth.tenant_for(&headers).map(str::to_owned);
    let snapshot = match state.jobs.submit(input, tenant_id.as_deref()).await {
        Ok(snapshot) => snapshot,
        Err(JobSubmitError::AtCapacity) => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "the AnyDoc job queue is at capacity",
                Value::Null,
                &request_id,
            )
        }
    };
    json_response(
        StatusCode::ACCEPTED,
        json!({
            "job_id": snapshot.job_id,
            "status": job_status_name(snapshot.status),
            "filename": snapshot.filename,
        }),
        &request_id,
    )
}

async fn list_jobs(
    State(state): State<AnyDocState>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let request_id = request_id(&headers);
    let limit = query.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_limit",
            "limit must be between 1 and 100",
            Value::Null,
            &request_id,
        );
    }
    json_response(
        StatusCode::OK,
        json!({
            "jobs": state.jobs.list(limit, state.auth.tenant_for(&headers)).await
        }),
        &request_id,
    )
}

async fn get_job(
    State(state): State<AnyDocState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = request_id(&headers);
    if !valid_job_id(&job_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_job_id",
            "job_id is invalid",
            Value::Null,
            &request_id,
        );
    }
    match state
        .jobs
        .get(&job_id, state.auth.tenant_for(&headers))
        .await
    {
        Some(snapshot) => json_response(StatusCode::OK, snapshot, &request_id),
        None => error_response(
            StatusCode::NOT_FOUND,
            "unknown_job",
            "parse job was not found",
            Value::Null,
            &request_id,
        ),
    }
}

async fn cancel_job(
    State(state): State<AnyDocState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = request_id(&headers);
    if !valid_job_id(&job_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_job_id",
            "job_id is invalid",
            Value::Null,
            &request_id,
        );
    }
    match state
        .jobs
        .cancel(&job_id, state.auth.tenant_for(&headers))
        .await
    {
        Some(snapshot) => json_response(StatusCode::OK, snapshot, &request_id),
        None => error_response(
            StatusCode::NOT_FOUND,
            "unknown_job",
            "parse job was not found",
            Value::Null,
            &request_id,
        ),
    }
}

async fn public_extract(
    State(state): State<AnyDocState>,
    Query(query): Query<ExtractQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = request_id(&headers);
    if is_multipart(&headers) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "use application/json or raw document bytes; multipart uploads are not supported",
            Value::Null,
            &request_id,
        );
    }
    let input = if is_json(&headers) {
        let request = match decode_json_request(&body, &state, &request_id) {
            Ok(request) => request,
            Err(response) => return response,
        };
        match prepare_json_input(request, &state, false) {
            Ok(input) => input,
            Err(error) => return input_error_response(error, &request_id),
        }
    } else {
        match prepare_raw_input(&body, &headers, query.format.as_deref(), &state) {
            Ok(input) => input,
            Err(error) => return input_error_response(error, &request_id),
        }
    };

    match convert_direct(input, &state).await {
        Ok(result) => json_response(StatusCode::OK, public_result(result), &request_id),
        Err(error) => conversion_error_response(error, &request_id),
    }
}

async fn convert_direct(
    input: PreparedInput,
    state: &AnyDocState,
) -> Result<ExtractionResult, ConversionFailure> {
    let permit = state.jobs.try_acquire_worker().ok_or_else(|| {
        ConversionFailure::new(
            "busy",
            "the AnyDoc service is at its concurrent conversion limit",
        )
    })?;
    let limits = &state.limits;
    let timeout = limits.timeout;
    let limits = limits.clone();
    let mut conversion = tokio::task::spawn_blocking(move || convert_input(input, &limits));
    match tokio::time::timeout(timeout, &mut conversion).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(ConversionFailure::new(
            "conversion_failed",
            format!("document worker failed: {error}"),
        )),
        Err(_) => {
            // Tokio cannot cancel a blocking task once it has started. Keep the
            // worker permit until the task exits so timed-out inputs cannot
            // create more native conversion work than max_workers.
            let _ = tokio::spawn(async move {
                let _permit = permit;
                let _ = conversion.await;
            });
            Err(ConversionFailure::new(
                "timeout",
                format!(
                    "document extraction exceeded the {}s limit",
                    timeout.as_secs()
                ),
            ))
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParseRequest {
    path: Option<String>,
    #[serde(alias = "content_base64")]
    content_base64: Option<String>,
    filename: Option<String>,
    #[serde(alias = "blob_sha256")]
    blob_sha256: Option<String>,
    #[serde(alias = "size_bytes")]
    size_bytes: Option<u64>,
    mime: Option<String>,
    format: Option<String>,
    options: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct ExtractQuery {
    format: Option<String>,
}

fn decode_json_request(
    body: &[u8],
    state: &AnyDocState,
    request_id: &str,
) -> Result<ParseRequest, Response> {
    if body.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "empty_request",
            "request body is empty",
            Value::Null,
            request_id,
        ));
    }
    if body.len() > state.limits.max_json_body_bytes {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "JSON request body exceeds the configured limit",
            json!({ "maxBytes": state.limits.max_json_body_bytes }),
            request_id,
        ));
    }
    serde_json::from_slice(body).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "request body is not valid JSON",
            Value::Null,
            request_id,
        )
    })
}

fn prepare_json_input(
    request: ParseRequest,
    state: &AnyDocState,
    allow_path: bool,
) -> Result<PreparedInput, InputError> {
    // These fields are advisory or backend-specific in the shared provider
    // contract. Read them explicitly so they remain accepted without silently
    // becoming a future second dispatch/configuration surface.
    let _advisory = (&request.size_bytes, &request.mime, &request.options);
    if request.path.is_some() == request.content_base64.is_some() {
        return Err(InputError::new(
            "invalid_request",
            "provide exactly one of path or contentBase64",
        ));
    }
    if request.path.is_some() && !allow_path {
        return Err(InputError::new(
            "input_rejected",
            "path input requires the Core-managed provider credential",
        ));
    }
    let requested_format = request
        .format
        .as_deref()
        .map(parse_format)
        .transpose()
        .map_err(|error| InputError::new("invalid_format", error.message))?
        .map(crate::convert::format_name)
        .map(str::to_owned);

    if let Some(raw_path) = request.path {
        let path = resolve_input(&raw_path, &state.roots, state.limits.max_input_bytes)?;
        let filename = request
            .filename
            .as_deref()
            .map(safe_basename)
            .transpose()?
            .or_else(|| filename_from_path(&path))
            .unwrap_or_else(|| "document".to_owned());
        let expected_sha256 = request
            .blob_sha256
            .as_deref()
            .map(validate_sha256)
            .transpose()?;
        return Ok(PreparedInput {
            source: InputSource::Path(path),
            filename,
            requested_format,
            expected_sha256,
        });
    }

    let filename = request
        .filename
        .as_deref()
        .map(safe_basename)
        .transpose()?
        .ok_or_else(|| {
            InputError::new("invalid_filename", "filename is required for inline input")
        })?;
    let encoded = request.content_base64.unwrap_or_default();
    let max_encoded = ((state.limits.max_input_bytes + 2) / 3) * 4 + 4;
    if encoded.len() > max_encoded {
        return Err(InputError::new(
            "input_too_large",
            "contentBase64 exceeds the configured input limit",
        ));
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| {
        InputError::new(
            "invalid_base64",
            "contentBase64 is not valid standard base64",
        )
    })?;
    if bytes.is_empty() {
        return Err(InputError::new(
            "empty_input",
            "decoded document input is empty",
        ));
    }
    if bytes.len() > state.limits.max_input_bytes {
        return Err(InputError::new(
            "input_too_large",
            "decoded document input exceeds the configured input limit",
        ));
    }
    Ok(PreparedInput {
        source: InputSource::Bytes(bytes),
        filename,
        requested_format,
        expected_sha256: None,
    })
}

fn prepare_raw_input(
    body: &[u8],
    headers: &HeaderMap,
    requested_format: Option<&str>,
    state: &AnyDocState,
) -> Result<PreparedInput, InputError> {
    if body.is_empty() {
        return Err(InputError::new("empty_input", "document input is empty"));
    }
    if body.len() > state.limits.max_input_bytes {
        return Err(InputError::new(
            "input_too_large",
            "document input exceeds the configured input limit",
        ));
    }
    let filename = headers
        .get("x-filename")
        .or_else(|| headers.get("x-file-name"))
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| InputError::new("invalid_filename", "X-Filename is required"))
        .and_then(safe_basename)?;
    let requested_format = requested_format
        .map(parse_format)
        .transpose()
        .map_err(|error| InputError::new("invalid_format", error.message))?
        .map(format_name)
        .map(str::to_owned);
    Ok(PreparedInput {
        source: InputSource::Bytes(body.to_vec()),
        filename,
        requested_format,
        expected_sha256: None,
    })
}

fn public_result(result: ExtractionResult) -> Value {
    json!({
        "apiVersion": API_VERSION,
        "backend": result.backend,
        "backendVersion": result.backend_version,
        "filename": result.metadata.filename,
        "format": result.metadata.format,
        "inputBytes": result.metadata.input_bytes,
        "sourceSha256": result.source_sha256,
        "markdown": result.markdown,
        "warnings": result.warnings,
        "truncated": result.truncated,
    })
}

fn limits_json(state: &AnyDocState) -> Value {
    json!({
        "max_input_bytes": state.limits.max_input_bytes,
        "max_output_bytes": state.limits.max_output_bytes,
        "timeout_secs": state.limits.timeout_secs(),
        "max_workers": state.limits.max_workers,
        "max_jobs": state.limits.max_jobs,
    })
}

fn conversion_error_response(error: ConversionFailure, request_id: &str) -> Response {
    let status = match error.code.as_str() {
        "input_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "needs_ocr" | "unsupported_format" | "malformed_document" | "encrypted_document"
        | "missing_part" | "empty_document" => StatusCode::UNPROCESSABLE_ENTITY,
        "timeout" => StatusCode::GATEWAY_TIMEOUT,
        "resource_limit" => StatusCode::PAYLOAD_TOO_LARGE,
        "busy" => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    };
    error_response(
        status,
        &error.code,
        &error.message,
        error.details,
        request_id,
    )
}

fn input_error_response(error: InputError, request_id: &str) -> Response {
    let status = match error.code {
        "input_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "invalid_format" | "invalid_base64" | "invalid_filename" | "empty_input" => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::BAD_REQUEST,
    };
    error_response(status, error.code, &error.message, Value::Null, request_id)
}

fn error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    details: Value,
    request_id: &str,
) -> Response {
    json_response(
        status,
        json!({
            "error": message,
            "code": code,
            "error_code": code,
            "details": details,
            "request_id": request_id,
        }),
        request_id,
    )
}

fn json_response<T: serde::Serialize>(status: StatusCode, body: T, request_id: &str) -> Response {
    let mut response = (status, Json(body)).into_response();
    set_request_id(&mut response, request_id);
    response
}

fn set_request_id(response: &mut Response, request_id: &str) {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
}

fn request_id(headers: &HeaderMap) -> String {
    let incoming = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        });
    incoming
        .map(str::to_owned)
        .unwrap_or_else(|| format!("req_{}", Uuid::new_v4().simple()))
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn is_multipart(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("multipart/form-data"))
}

fn valid_job_id(job_id: &str) -> bool {
    job_id.len() > 6
        && job_id.len() <= 80
        && job_id.starts_with("parse_")
        && job_id[6..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn job_status_name(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Succeeded => "succeeded",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}

/// The OpenAPI contract for the customer-facing `/v1/extract` endpoint.
///
/// The Core provider routes remain intentionally job-shaped; this versioned
/// endpoint is a synchronous service call for customers using AnyDoc directly.
pub async fn openapi() -> Json<Value> {
    Json(json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Ryu AnyDoc API",
            "version": API_VERSION,
            "description": "Extract GitHub-Flavored Markdown from supported office, ebook, CSV, and PDF documents using AnyDoc. The local Rust service does not perform OCR or fetch URLs."
        },
        "paths": {
            "/v1/capability": {
                "get": {
                    "operationId": "getCapability",
                    "responses": { "200": { "description": "Supported formats and limits." } }
                }
            },
            "/v1/extract": {
                "post": {
                    "operationId": "extractDocument",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/octet-stream": {
                                "schema": { "type": "string", "format": "binary" },
                                "description": "Raw document bytes; send the original filename in X-Filename."
                            },
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/ExtractRequest" }
                            }
                        }
                    },
                    "responses": {
                        "200": { "description": "Markdown extraction result.", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ExtractResponse" } } } },
                        "401": { "description": "Missing or invalid API key." },
                        "413": { "description": "Input exceeds the configured limit." },
                        "429": { "description": "Concurrent conversion capacity is exhausted." },
                        "422": { "description": "AnyDoc cannot produce complete Markdown." }
                    }
                }
            }
        },
        "components": {
            "securitySchemes": { "bearerAuth": { "type": "http", "scheme": "bearer" } },
            "schemas": {
                "ExtractRequest": {
                    "type": "object",
                    "properties": {
                        "contentBase64": { "type": "string", "description": "Base64 document bytes." },
                        "filename": { "type": "string", "description": "Original basename used for format fallback." },
                        "format": { "type": "string", "description": "Optional format override such as csv or docx." }
                    },
                    "required": ["contentBase64", "filename"]
                },
                "ExtractResponse": {
                    "type": "object",
                    "required": ["apiVersion", "backend", "backendVersion", "filename", "format", "markdown", "truncated"],
                    "properties": {
                        "apiVersion": { "type": "string" },
                        "backend": { "type": "string", "example": "anydoc" },
                        "backendVersion": { "type": "string", "example": "0.2.4" },
                        "filename": { "type": "string" },
                        "format": { "type": "string" },
                        "inputBytes": { "type": "integer" },
                        "sourceSha256": { "type": "string" },
                        "markdown": { "type": "string" },
                        "warnings": { "type": "array", "items": { "type": "string" } },
                        "truncated": { "type": "boolean" }
                    }
                }
            }
        },
        "security": [{ "bearerAuth": [] }]
    }))
}

#[cfg(test)]
mod tests {
    use super::{is_json, job_status_name, request_id, valid_job_id};
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn request_ids_are_bounded_and_preserve_safe_correlation_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("req_test"));
        assert_eq!(request_id(&headers), "req_test");
        assert!(request_id(&HeaderMap::new()).starts_with("req_"));
    }

    #[test]
    fn provider_job_ids_are_opaque_but_path_safe() {
        assert!(valid_job_id("parse_0123456789abcdef"));
        assert!(!valid_job_id("parse_../secret"));
        assert!(!valid_job_id("other_0123"));
        assert_eq!(job_status_name(super::JobStatus::Succeeded), "succeeded");
    }

    #[test]
    fn json_detection_requires_the_json_media_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(is_json(&headers));
        headers.insert("content-type", HeaderValue::from_static("application/pdf"));
        assert!(!is_json(&headers));
    }

    #[tokio::test]
    async fn loopback_http_preserves_customer_uploads_without_file_authority() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let limits = crate::limits::Limits::default();
        let state = crate::state::AnyDocState {
            auth: crate::auth::AuthConfig::test_config(
                "core-fixture",
                &[("customer-a", "tenant-a")],
            ),
            jobs: crate::jobs::JobStore::new(limits.clone()),
            limits,
            roots: vec![],
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, super::router(state)).await.unwrap();
        });
        for (payload, expected) in [
            (
                serde_json::json!({"path":"fixture.csv","format":"csv"}),
                "HTTP/1.1 400",
            ),
            (
                serde_json::json!({"contentBase64":"bmFtZSx2YWx1ZQpmaXh0dXJlLG9ubHkK","filename":"fixture.csv","format":"csv"}),
                "HTTP/1.1 202",
            ),
        ] {
            let body = payload.to_string();
            let request = format!("POST /api/anydoc/parse HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer customer-a\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                socket.read_to_end(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(String::from_utf8(response).unwrap().starts_with(expected));
        }
        server.abort();
    }

    #[tokio::test]
    async fn customer_credentials_cannot_admit_local_files() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let root = tempfile::tempdir().expect("synthetic input root");
        let file = root.path().join("fixture.txt");
        std::fs::write(&file, "name,value\nfixture,only\n").expect("fixture document");
        let limits = crate::limits::Limits::default();
        let state = crate::state::AnyDocState {
            auth: crate::auth::AuthConfig::test_config(
                "core-fixture",
                &[("customer-a", "tenant-a"), ("customer-b", "tenant-b")],
            ),
            jobs: crate::jobs::JobStore::new(limits.clone()),
            limits,
            roots: vec![root.path().canonicalize().expect("root")],
        };
        let app = super::router(state.clone());
        let payload = serde_json::json!({"path":file,"format":"csv"});
        for token in ["customer-a", "customer-b"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/anydoc/parse")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        }
        assert!(state.jobs.list(10, Some("tenant-a")).await.is_empty());
        for route in ["/v1/extract", "/api/anydoc/v1/extract"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(route)
                        .header("authorization", "Bearer customer-a")
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        }
        let inline = serde_json::json!({
            "contentBase64": "bmFtZSx2YWx1ZQpmaXh0dXJlLG9ubHkK",
            "filename": "fixture.csv", "format": "csv"
        });
        for route in ["/api/anydoc/parse", "/v1/extract", "/api/anydoc/v1/extract"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(route)
                        .header("authorization", "Bearer customer-a")
                        .header("content-type", "application/json")
                        .body(Body::from(inline.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let expected = if route.ends_with("/parse") {
                axum::http::StatusCode::ACCEPTED
            } else {
                axum::http::StatusCode::OK
            };
            assert_eq!(response.status(), expected);
        }
        assert_eq!(state.jobs.list(10, Some("tenant-a")).await.len(), 1);
        assert!(state.jobs.list(10, Some("tenant-b")).await.is_empty());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/anydoc/parse")
                    .header("authorization", "Bearer core-fixture")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    }

    // ── jobs list / get / cancel and capability, through the real router ──

    fn jobs_state(max_workers: usize) -> crate::state::AnyDocState {
        let mut limits = crate::limits::Limits::default();
        limits.max_workers = max_workers;
        crate::state::AnyDocState {
            auth: crate::auth::AuthConfig::test_config(
                "core-fixture",
                &[("customer-a", "tenant-a"), ("customer-b", "tenant-b")],
            ),
            jobs: crate::jobs::JobStore::new(limits.clone()),
            limits,
            roots: vec![],
        }
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use axum::body::Body;
        use tower::ServiceExt;
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn submit_csv(app: &axum::Router, token: &str) -> String {
        let (status, body) = send(
            app,
            "POST",
            "/api/anydoc/parse",
            Some(token),
            Some(serde_json::json!({
                "contentBase64": "bmFtZSx2YWx1ZQpmaXh0dXJlLG9ubHkK",
                "filename": "fixture.csv",
                "format": "csv",
            })),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::ACCEPTED, "{body}");
        body["job_id"].as_str().expect("job id").to_owned()
    }

    #[tokio::test]
    async fn job_and_capability_routes_require_a_valid_bearer_key() {
        use axum::http::StatusCode;
        let app = super::router(jobs_state(2));
        for (method, uri) in [
            ("GET", "/api/anydoc/capability"),
            ("GET", "/api/anydoc/jobs"),
            ("GET", "/api/anydoc/jobs/parse_0123456789abcdef"),
            ("DELETE", "/api/anydoc/jobs/parse_0123456789abcdef"),
        ] {
            for token in [None, Some("wrong-key")] {
                let (status, body) = send(&app, method, uri, token, None).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
                assert_eq!(body["code"], "unauthorized");
            }
        }
        let (status, capability) = send(
            &app,
            "GET",
            "/api/anydoc/capability",
            Some("core-fixture"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(capability["capability"], "document.parse");
        assert_eq!(capability["limits"]["max_workers"], 2);
        assert!(capability["formats"]
            .as_array()
            .expect("formats")
            .iter()
            .any(|format| format == ".csv"));
    }

    #[tokio::test]
    async fn cancelling_a_queued_job_is_final_even_after_a_worker_frees_up() {
        use axum::http::StatusCode;
        let state = jobs_state(1);
        let app = super::router(state.clone());
        // Hold the only worker so the job is still queued when it is cancelled.
        let worker = state.jobs.try_acquire_worker().expect("idle worker");
        let job_id = submit_csv(&app, "customer-a").await;
        let uri = format!("/api/anydoc/jobs/{job_id}");

        let (status, queued) = send(&app, "GET", &uri, Some("customer-a"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(queued["status"], "queued");

        let (status, cancelled) = send(&app, "DELETE", &uri, Some("customer-a"), None).await;
        assert_eq!(status, StatusCode::OK, "{cancelled}");
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["error_code"], "cancelled");
        assert!(cancelled["finished_at"].is_string());

        // Release the worker and let the queued task run to completion: the
        // semaphore hands the permit to the waiting job first, so it is free
        // again only once that task has finished.
        drop(worker);
        let mut drained = false;
        for _ in 0..10_000 {
            tokio::task::yield_now().await;
            if let Some(permit) = state.jobs.try_acquire_worker() {
                drop(permit);
                drained = true;
                break;
            }
        }
        assert!(drained, "the queued job never released its worker");

        let (status, after) = send(&app, "GET", &uri, Some("customer-a"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(after["status"], "cancelled");
        assert!(after["result"].is_null());
        assert!(
            after["started_at"].is_null(),
            "a cancelled job never starts"
        );
    }

    #[tokio::test]
    async fn unknown_and_malformed_job_ids_are_distinct_errors() {
        use axum::http::StatusCode;
        let app = super::router(jobs_state(2));
        for method in ["GET", "DELETE"] {
            let (status, body) = send(
                &app,
                method,
                "/api/anydoc/jobs/parse_0123456789abcdef",
                Some("customer-a"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
            assert_eq!(body["code"], "unknown_job");

            let (status, body) = send(
                &app,
                method,
                "/api/anydoc/jobs/parse_not-hex",
                Some("customer-a"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method}");
            assert_eq!(body["code"], "invalid_job_id");
        }
        for limit in ["0", "101"] {
            let (status, body) = send(
                &app,
                "GET",
                &format!("/api/anydoc/jobs?limit={limit}"),
                Some("customer-a"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "limit={limit}");
            assert_eq!(body["code"], "invalid_limit");
        }
    }

    #[tokio::test]
    async fn another_tenant_can_neither_see_nor_cancel_a_job() {
        use axum::http::StatusCode;
        let state = jobs_state(1);
        let app = super::router(state.clone());
        let worker = state.jobs.try_acquire_worker().expect("idle worker");
        let job_id = submit_csv(&app, "customer-a").await;
        let uri = format!("/api/anydoc/jobs/{job_id}");

        for method in ["GET", "DELETE"] {
            let (status, body) = send(&app, method, &uri, Some("customer-b"), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
            assert_eq!(body["code"], "unknown_job");
        }
        // Core's own scope is separate from every customer tenant.
        let (status, _) = send(&app, "DELETE", &uri, Some("core-fixture"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (_, other) = send(&app, "GET", "/api/anydoc/jobs", Some("customer-b"), None).await;
        assert_eq!(other["jobs"], serde_json::json!([]));
        let (_, core) = send(&app, "GET", "/api/anydoc/jobs", Some("core-fixture"), None).await;
        assert_eq!(core["jobs"], serde_json::json!([]));

        let (status, own) = send(&app, "GET", "/api/anydoc/jobs", Some("customer-a"), None).await;
        assert_eq!(status, StatusCode::OK);
        let jobs = own["jobs"].as_array().expect("jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["job_id"], job_id.as_str());
        assert_eq!(
            jobs[0]["status"], "queued",
            "the refused cancels changed nothing"
        );
        drop(worker);
    }
}
