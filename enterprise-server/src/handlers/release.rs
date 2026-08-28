use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::auth::middleware::{AdminGuard, OptionalAuth};
use crate::error::AppError;
use crate::models::release::{ChannelInfo, FeatureFlagsResponse};
use crate::routes::AppState;

/// GET /worker/releases — Get release version info
pub async fn get_releases(
    State(state): State<AppState>,
    _auth: OptionalAuth,
) -> Result<Json<Value>, AppError> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT channel, version, checksum FROM release_channels")
            .fetch_all(&state.db)
            .await
            .map_err(|e| AppError::Database(e))?;

    let mut channels_map: HashMap<String, ChannelInfo> = HashMap::new();
    for (channel, version, checksum) in rows {
        channels_map.insert(channel, ChannelInfo { version, checksum });
    }

    let response = crate::models::release::ReleasesResponse {
        channels: channels_map,
    };
    Ok(Json(serde_json::to_value(response).unwrap()))
}

/// GET /worker/releases/{channel}/download/{filename} — Download release asset
pub async fn download_release(
    State(state): State<AppState>,
    Path((channel, filename)): Path<(String, String)>,
) -> Result<Response, AppError> {
    // A channel is valid when it has been published in release_channels. This
    // includes moving aliases such as `latest` and immutable version channels
    // such as `1.3.3` generated for pinned installers.
    let Some(version) = release_channel_version(&state, &channel).await? else {
        return Err(AppError::NotFound(format!(
            "Unknown release channel: {}",
            channel
        )));
    };

    let row: Option<(String,)> = sqlx::query_as(
        "SELECT storage_path FROM release_assets WHERE version = $1 AND filename = $2",
    )
    .bind(&version)
    .bind(&filename)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let Some((storage_path,)) = row else {
        return Err(AppError::NotFound(format!(
            "Release asset not found: {}/{}",
            channel, filename
        )));
    };

    match state.cas_store.get_release_path(&storage_path).await {
        Ok(Some(data)) => {
            let content_type = guess_content_type(&filename);
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CACHE_CONTROL, "public, max-age=3600");

            // For install scripts, set executable content type
            if filename.ends_with(".sh") {
                response =
                    response.header(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8");
            } else if filename.ends_with(".ps1") {
                response = response.header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
            }

            return Ok(response
                .body(Body::from(data))
                .map_err(|e| AppError::Internal(format!("Failed to build response: {}", e)))?);
        }
        Ok(None) => {
            // Asset exists in DB but not in object storage.
            Err(AppError::NotFound("Release asset not yet uploaded".into()))
        }
        Err(e) => {
            tracing::warn!("S3 release download failed: {}", e);
            Err(AppError::NotFound(format!(
                "Release asset not found: {}/{}",
                channel, filename
            )))
        }
    }
}

/// GET /worker/config/feature-flags — Get feature flags
pub async fn get_feature_flags(State(state): State<AppState>) -> Json<Value> {
    // Try to load from DB first
    let rows: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT key, value FROM feature_flags")
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();

    if !rows.is_empty() {
        let flags: serde_json::Map<String, Value> = rows.into_iter().map(|(k, v)| (k, v)).collect();
        return Json(Value::Object(flags));
    }

    // Fall back to defaults
    Json(serde_json::to_value(FeatureFlagsResponse::default()).unwrap())
}

/// POST /api/admin/releases/channel — Create or update a release channel
#[derive(Debug, Deserialize)]
pub struct UpdateChannelRequest {
    pub channel: String,
    pub version: String,
    pub checksum: String,
}

pub async fn update_release_channel(
    State(state): State<AppState>,
    _auth: AdminGuard,
    Json(req): Json<UpdateChannelRequest>,
) -> Result<Json<Value>, AppError> {
    publish_release_channel(&state, &req.channel, &req.version, &req.checksum).await?;

    Ok(Json(json!({
        "success": true,
        "channel": req.channel,
        "version": req.version,
    })))
}

/// POST /api/admin/releases/upload — Upload a release asset
pub async fn upload_release_asset(
    State(state): State<AppState>,
    _auth: AdminGuard,
    multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut channel = String::new();
    let mut version = String::new();
    let mut filename = String::new();
    let mut sha256 = String::new();
    let mut data: Option<Vec<u8>> = None;

    let mut multipart = multipart;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {}", e)))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "channel" => {
                channel = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Field error: {}", e)))?;
            }
            "version" => {
                version = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Field error: {}", e)))?;
            }
            "filename" => {
                filename = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Field error: {}", e)))?;
            }
            "sha256" => {
                sha256 = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Field error: {}", e)))?;
            }
            "file" => {
                data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| AppError::BadRequest(format!("File error: {}", e)))?
                        .to_vec(),
                );
            }
            _ => {}
        }
    }

    let data = data.ok_or_else(|| AppError::BadRequest("file is required".into()))?;
    let asset = store_release_asset(
        &state,
        &channel,
        optional_text(&version),
        &filename,
        optional_text(&sha256),
        &data,
    )
    .await?;

    tracing::info!(
        "Release asset uploaded: channel={} version={} filename={} ({} bytes)",
        asset.channel,
        asset.version,
        asset.filename,
        asset.size_bytes
    );

    Ok(Json(json!({
        "success": true,
        "channel": asset.channel,
        "version": asset.version,
        "filename": asset.filename,
        "sha256": asset.sha256,
        "size_bytes": asset.size_bytes,
    })))
}

const INSTALL_SH_TEMPLATE: &str = include_str!("../../../install.sh");
const INSTALL_PS1_TEMPLATE: &str = include_str!("../../../install.ps1");

pub(crate) const RELEASE_BINARY_MAX_BYTES: usize = 100 * 1024 * 1024;
pub(crate) const RELEASE_UPLOAD_MAX_BYTES: usize = 500 * 1024 * 1024;
pub(crate) const RELEASE_UPLOAD_BODY_MAX_BYTES: usize = 512 * 1024 * 1024;

const RELEASE_BINARY_FILES: [&str; 6] = [
    "git-ai-linux-x64",
    "git-ai-linux-arm64",
    "git-ai-windows-x64.exe",
    "git-ai-windows-arm64.exe",
    "git-ai-macos-x64",
    "git-ai-macos-arm64",
];

const RELEASE_BUNDLE_FILES: [&str; 8] = [
    "git-ai-linux-x64",
    "git-ai-linux-arm64",
    "git-ai-windows-x64.exe",
    "git-ai-windows-arm64.exe",
    "git-ai-macos-x64",
    "git-ai-macos-arm64",
    "install.sh",
    "install.ps1",
];

/// POST /api/admin/releases/publish — Upload, validate, and publish a complete release.
pub async fn publish_release_bundle(
    State(state): State<AppState>,
    _auth: AdminGuard,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut version = String::new();
    let mut promote_to_latest = true;
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    let mut total_file_bytes = 0;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {}", e)))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "version" => {
                version = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Field error: {}", e)))?;
            }
            "promote_to_latest" => {
                let value = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Field error: {}", e)))?;
                promote_to_latest = matches!(value.as_str(), "true" | "1" | "on");
            }
            "files" | "file" => {
                let filename = field
                    .file_name()
                    .ok_or_else(|| AppError::BadRequest("Release file needs a filename".into()))?
                    .to_string();
                if !RELEASE_BINARY_FILES.contains(&filename.as_str()) {
                    return Err(AppError::BadRequest(format!(
                        "Unexpected release file: {}",
                        filename
                    )));
                }
                if files.contains_key(&filename) {
                    return Err(AppError::BadRequest(format!(
                        "Duplicate release file: {}",
                        filename
                    )));
                }
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("File error: {}", e)))?
                    .to_vec();
                total_file_bytes =
                    validate_release_binary_size(&filename, data.len(), total_file_bytes)?;
                files.insert(filename, data);
            }
            _ => {}
        }
    }

    let version = validate_release_version(&version)?.to_string();
    if release_channel_version(&state, &version).await?.is_some() {
        return Err(AppError::Conflict(format!(
            "Release version {} has already been published",
            version
        )));
    }

    let missing = RELEASE_BINARY_FILES
        .iter()
        .filter(|filename| !files.contains_key(**filename))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Missing release files: {}",
            missing.join(", ")
        )));
    }
    let mut binary_manifest = String::new();
    for filename in RELEASE_BINARY_FILES {
        let data = &files[filename];
        binary_manifest.push_str(&format!("{}  {}\n", sha256_hex(data), filename));
    }
    let embedded_checksums = binary_manifest.trim_end().replace('\n', "|");
    let release_base_url = state.config.base_url.trim_end_matches('/');
    let install_sh = render_installer_template(
        INSTALL_SH_TEMPLATE,
        &version,
        &embedded_checksums,
        release_base_url,
    );
    let install_ps1 = render_installer_template(
        INSTALL_PS1_TEMPLATE,
        &version,
        &embedded_checksums,
        release_base_url,
    );
    files.insert("install.sh".into(), install_sh.into_bytes());
    files.insert("install.ps1".into(), install_ps1.into_bytes());

    validate_pinned_installer(&version, "install.sh", &files["install.sh"])?;
    validate_pinned_installer(&version, "install.ps1", &files["install.ps1"])?;

    let mut manifest = String::new();
    for filename in RELEASE_BUNDLE_FILES {
        manifest.push_str(&format!("{}  {}\n", sha256_hex(&files[filename]), filename));
    }
    let manifest_bytes = manifest.into_bytes();
    let manifest_checksum = sha256_hex(&manifest_bytes);

    for filename in RELEASE_BUNDLE_FILES {
        store_release_asset(
            &state,
            &version,
            Some(&version),
            filename,
            None,
            &files[filename],
        )
        .await?;
    }
    store_release_asset(
        &state,
        &version,
        Some(&version),
        "SHA256SUMS",
        Some(&manifest_checksum),
        &manifest_bytes,
    )
    .await?;

    publish_release_channel(&state, &version, &version, &manifest_checksum).await?;
    if promote_to_latest {
        publish_release_channel(&state, "latest", &version, &manifest_checksum).await?;
    }

    Ok(Json(json!({
        "success": true,
        "version": version,
        "version_channel": version,
        "latest_updated": promote_to_latest,
        "checksum": manifest_checksum,
        "asset_count": RELEASE_BUNDLE_FILES.len() + 1,
        "install_url": format!("/worker/releases/{}/download/install.sh", version),
    })))
}

/// GET /api/admin/releases/assets — List all release assets
pub async fn list_release_assets(
    State(state): State<AppState>,
    _auth: AdminGuard,
) -> Result<Json<Value>, AppError> {
    let rows: Vec<(String, String, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT channel, version, filename, sha256, size_bytes FROM release_assets ORDER BY channel, version, filename"
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let assets: Vec<Value> = rows
        .iter()
        .map(|(channel, version, filename, sha256, size)| {
            json!({
                "channel": channel,
                "version": version,
                "filename": filename,
                "sha256": sha256,
                "size_bytes": size,
            })
        })
        .collect();

    Ok(Json(json!({ "assets": assets })))
}

/// DELETE /api/admin/releases/assets/{channel}/{filename} — Delete a release asset
pub async fn delete_release_asset(
    State(state): State<AppState>,
    _auth: AdminGuard,
    Path((channel, filename)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let result = sqlx::query("DELETE FROM release_assets WHERE channel = $1 AND filename = $2")
        .bind(&channel)
        .bind(&filename)
        .execute(&state.db)
        .await
        .map_err(|e| AppError::Database(e))?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Release asset not found".into()));
    }

    Ok(Json(json!({ "success": true })))
}

#[derive(Debug)]
struct StoredReleaseAsset {
    channel: String,
    version: String,
    filename: String,
    sha256: String,
    size_bytes: i64,
}

async fn release_channel_version(
    state: &AppState,
    channel: &str,
) -> Result<Option<String>, AppError> {
    sqlx::query_scalar("SELECT version FROM release_channels WHERE channel = $1")
        .bind(channel)
        .fetch_optional(&state.db)
        .await
        .map_err(AppError::Database)
}

async fn publish_release_channel(
    state: &AppState,
    channel: &str,
    version: &str,
    checksum: &str,
) -> Result<(), AppError> {
    let channel = require_text(channel, "channel")?;
    let version = require_text(version, "version")?;
    let checksum = require_text(checksum, "checksum")?;

    ensure_release_manifest_ready(state, version, checksum).await?;

    sqlx::query(
        r#"INSERT INTO release_channels (channel, version, checksum)
        VALUES ($1, $2, $3)
        ON CONFLICT (channel) DO UPDATE SET
            version = EXCLUDED.version,
            checksum = EXCLUDED.checksum,
            updated_at = now()"#,
    )
    .bind(channel)
    .bind(version)
    .bind(checksum)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    tracing::info!("Release channel updated: {}={}", channel, version);
    Ok(())
}

async fn ensure_release_manifest_ready(
    state: &AppState,
    version: &str,
    checksum: &str,
) -> Result<(), AppError> {
    let manifest_row: Option<(String, String)> = sqlx::query_as(
        "SELECT sha256, storage_path FROM release_assets WHERE version = $1 AND filename = 'SHA256SUMS'",
    )
    .bind(version)
    .fetch_optional(&state.db)
    .await
    .map_err(AppError::Database)?;

    let Some((manifest_sha256, manifest_path)) = manifest_row else {
        return Err(AppError::BadRequest(
            "Release version is missing a SHA256SUMS asset matching checksum".into(),
        ));
    };

    if !manifest_sha256.eq_ignore_ascii_case(checksum) {
        return Err(AppError::BadRequest(
            "Release version is missing a SHA256SUMS asset matching checksum".into(),
        ));
    }

    let manifest_data = state
        .cas_store
        .get_release_path(&manifest_path)
        .await?
        .ok_or_else(|| {
            AppError::BadRequest("Release version is missing uploaded SHA256SUMS content".into())
        })?;
    let actual_manifest_sha256 = sha256_hex(&manifest_data);
    if !actual_manifest_sha256.eq_ignore_ascii_case(checksum) {
        return Err(AppError::BadRequest(
            "SHA256SUMS content does not match release checksum".into(),
        ));
    }

    let manifest = std::str::from_utf8(&manifest_data)
        .map_err(|_| AppError::BadRequest("SHA256SUMS is not valid UTF-8".into()))?;
    for (filename, expected_sha256) in parse_release_manifest(manifest)? {
        let asset_row: Option<(String, String)> = sqlx::query_as(
            "SELECT sha256, storage_path FROM release_assets WHERE version = $1 AND filename = $2",
        )
        .bind(version)
        .bind(&filename)
        .fetch_optional(&state.db)
        .await
        .map_err(AppError::Database)?;

        let Some((asset_sha256, asset_path)) = asset_row else {
            return Err(AppError::BadRequest(format!(
                "Release version is missing asset listed in SHA256SUMS: {}",
                filename
            )));
        };

        if !asset_sha256.eq_ignore_ascii_case(&expected_sha256) {
            return Err(AppError::BadRequest(format!(
                "Release asset checksum mismatch for {}",
                filename
            )));
        }

        let asset_data = state
            .cas_store
            .get_release_path(&asset_path)
            .await?
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "Release version is missing uploaded asset content: {}",
                    filename
                ))
            })?;
        let actual_sha256 = sha256_hex(&asset_data);
        if !actual_sha256.eq_ignore_ascii_case(&expected_sha256) {
            return Err(AppError::BadRequest(format!(
                "Release asset content checksum mismatch for {}",
                filename
            )));
        }
    }

    Ok(())
}

async fn store_release_asset(
    state: &AppState,
    channel: &str,
    version: Option<&str>,
    filename: &str,
    sha256: Option<&str>,
    data: &[u8],
) -> Result<StoredReleaseAsset, AppError> {
    let channel = require_text(channel, "channel")?.to_string();
    let filename = require_text(filename, "filename")?.to_string();
    let version = match version {
        Some(version) => require_text(version, "version")?.to_string(),
        None => release_channel_version(state, &channel)
            .await?
            .ok_or_else(|| {
                AppError::BadRequest("version is required for release asset upload".into())
            })?,
    };
    let actual_sha256 = sha256_hex(data);

    if let Some(expected_sha256) = sha256 {
        let expected_sha256 = require_text(expected_sha256, "sha256")?;
        if !expected_sha256.eq_ignore_ascii_case(&actual_sha256) {
            return Err(AppError::BadRequest(
                "sha256 does not match uploaded file".into(),
            ));
        }
    }

    let size_bytes = data.len() as i64;
    let storage_path = crate::services::cas::CasStore::release_asset_path(&version, &filename);

    state
        .cas_store
        .put_release(&version, &filename, data)
        .await?;

    sqlx::query(
        r#"INSERT INTO release_assets (channel, version, filename, sha256, size_bytes, storage_path)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (version, filename) DO UPDATE SET
            channel = EXCLUDED.channel,
            sha256 = EXCLUDED.sha256,
            size_bytes = EXCLUDED.size_bytes,
            storage_path = EXCLUDED.storage_path"#,
    )
    .bind(&channel)
    .bind(&version)
    .bind(&filename)
    .bind(&actual_sha256)
    .bind(size_bytes)
    .bind(&storage_path)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    Ok(StoredReleaseAsset {
        channel,
        version,
        filename,
        sha256: actual_sha256,
        size_bytes,
    })
}

fn require_text<'a>(value: &'a str, field: &str) -> Result<&'a str, AppError> {
    let value = value.trim();
    if value.is_empty() {
        Err(AppError::BadRequest(format!("{} is required", field)))
    } else {
        Ok(value)
    }
}

fn optional_text(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn validate_release_version(value: &str) -> Result<&str, AppError> {
    let value = require_text(value, "version")?;
    if matches!(value, "." | "..")
        || value.len() > 80
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'))
    {
        return Err(AppError::BadRequest(
            "version contains invalid characters".into(),
        ));
    }
    Ok(value)
}

fn validate_release_binary_size(
    filename: &str,
    size_bytes: usize,
    current_total_bytes: usize,
) -> Result<usize, AppError> {
    if size_bytes == 0 {
        return Err(AppError::BadRequest(format!(
            "{} must not be empty",
            filename
        )));
    }
    if size_bytes > RELEASE_BINARY_MAX_BYTES {
        return Err(AppError::BadRequest(format!(
            "{} exceeds the {} MiB release file limit",
            filename,
            RELEASE_BINARY_MAX_BYTES / 1024 / 1024
        )));
    }
    let total_bytes = current_total_bytes
        .checked_add(size_bytes)
        .ok_or_else(|| AppError::BadRequest("Release upload size overflow".into()))?;
    if total_bytes > RELEASE_UPLOAD_MAX_BYTES {
        return Err(AppError::BadRequest(format!(
            "Release files exceed the {} MiB total limit",
            RELEASE_UPLOAD_MAX_BYTES / 1024 / 1024
        )));
    }
    Ok(total_bytes)
}

fn validate_pinned_installer(version: &str, filename: &str, data: &[u8]) -> Result<(), AppError> {
    let content = std::str::from_utf8(data)
        .map_err(|_| AppError::BadRequest(format!("{} is not valid UTF-8", filename)))?;
    let valid = match filename {
        "install.sh" => {
            content.contains(&format!("PINNED_VERSION=\"{}\"", version))
                && content.contains(&format!("ENTERPRISE_RELEASE_CHANNEL=\"{}\"", version))
        }
        "install.ps1" => {
            content.contains(&format!("$PinnedVersion = '{}'", version))
                && content.contains(&format!("$EnterpriseReleaseChannel = '{}'", version))
        }
        _ => false,
    };
    if !valid {
        return Err(AppError::BadRequest(format!(
            "{} is not pinned to release channel {}",
            filename, version
        )));
    }
    Ok(())
}

fn render_installer_template(
    template: &str,
    version: &str,
    embedded_checksums: &str,
    release_base_url: &str,
) -> String {
    template
        .replace(
            "PINNED_VERSION=\"__VERSION_PLACEHOLDER__\"",
            &format!("PINNED_VERSION=\"{}\"", version),
        )
        .replace(
            "EMBEDDED_CHECKSUMS=\"__CHECKSUMS_PLACEHOLDER__\"",
            &format!("EMBEDDED_CHECKSUMS=\"{}\"", embedded_checksums),
        )
        .replace(
            "ENTERPRISE_RELEASE_BASE_URL=\"__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__\"",
            &format!("ENTERPRISE_RELEASE_BASE_URL=\"{}\"", release_base_url),
        )
        .replace(
            "ENTERPRISE_RELEASE_CHANNEL=\"__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__\"",
            &format!("ENTERPRISE_RELEASE_CHANNEL=\"{}\"", version),
        )
        .replace(
            "ENTERPRISE_API_BASE_URL=\"__ENTERPRISE_API_BASE_URL_PLACEHOLDER__\"",
            &format!("ENTERPRISE_API_BASE_URL=\"{}\"", release_base_url),
        )
        .replace(
            "$PinnedVersion = '__VERSION_PLACEHOLDER__'",
            &format!("$PinnedVersion = '{}'", version),
        )
        .replace(
            "$EmbeddedChecksums = '__CHECKSUMS_PLACEHOLDER__'",
            &format!("$EmbeddedChecksums = '{}'", embedded_checksums),
        )
        .replace(
            "$EnterpriseReleaseBaseUrl = '__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__'",
            &format!("$EnterpriseReleaseBaseUrl = '{}'", release_base_url),
        )
        .replace(
            "$EnterpriseReleaseChannel = '__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__'",
            &format!("$EnterpriseReleaseChannel = '{}'", version),
        )
        .replace(
            "$EnterpriseApiBaseUrl = '__ENTERPRISE_API_BASE_URL_PLACEHOLDER__'",
            &format!("$EnterpriseApiBaseUrl = '{}'", release_base_url),
        )
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn parse_release_manifest(content: &str) -> Result<Vec<(String, String)>, AppError> {
    let mut entries = Vec::new();
    for (line_index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((sha256, filename)) = line.split_once("  ") else {
            return Err(AppError::BadRequest(format!(
                "Invalid SHA256SUMS line {}",
                line_index + 1
            )));
        };
        let sha256 = sha256.trim();
        if sha256.len() != 64 || !sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(AppError::BadRequest(format!(
                "Invalid SHA256SUMS hash on line {}",
                line_index + 1
            )));
        }
        let filename = require_text(filename.trim_start_matches('*'), "SHA256SUMS filename")?;
        entries.push((filename.to_string(), sha256.to_ascii_lowercase()));
    }

    if entries.is_empty() {
        return Err(AppError::BadRequest(
            "SHA256SUMS does not contain any asset entries".into(),
        ));
    }

    Ok(entries)
}

fn guess_content_type(filename: &str) -> &'static str {
    if filename.ends_with(".exe") {
        "application/octet-stream"
    } else if filename.ends_with(".sh") {
        "text/x-shellscript; charset=utf-8"
    } else if filename.ends_with(".ps1") {
        "text/plain; charset=utf-8"
    } else if filename.ends_with("SHA256SUMS") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use object_store::local::LocalFileSystem;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use std::sync::Arc;
    use uuid::Uuid;

    struct TestDatabase {
        state: AppState,
        admin_pool: PgPool,
        db_name: String,
        _object_store_dir: tempfile::TempDir,
    }

    impl TestDatabase {
        async fn new() -> anyhow::Result<Option<Self>> {
            let object_store_dir = tempfile::tempdir()?;
            let cas_store = local_cas_store(object_store_dir.path())?;
            let database_url = test_database_url();
            let db_name = unique_test_database_name();
            let admin_url = database_url_for_database(&database_url, "postgres")?;
            let test_url = database_url_for_database(&database_url, &db_name)?;

            let admin_pool = match PgPoolOptions::new()
                .max_connections(2)
                .connect(&admin_url)
                .await
            {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!(
                        "skipping release test: could not connect to admin database: {error}"
                    );
                    return Ok(None);
                }
            };

            if let Err(error) = create_database(&admin_pool, &db_name).await {
                eprintln!(
                    "skipping release test: could not create isolated database {db_name}: {error}"
                );
                return Ok(None);
            }

            let pool = PgPoolOptions::new()
                .max_connections(6)
                .connect(&test_url)
                .await?;
            crate::db::run_migrations(&pool).await?;

            let config = test_config(&test_url);
            let redis = redis::Client::open(config.redis_url.clone())?;
            let auth_password_limiter = crate::routes::auth_password_limiter(&config);
            let state = AppState {
                db: pool,
                redis,
                config,
                cas_store,
                rate_limiter: crate::services::rate_limit::RateLimiter::new(),
                auth_password_limiter,
            };

            Ok(Some(Self {
                state,
                admin_pool,
                db_name,
                _object_store_dir: object_store_dir,
            }))
        }

        async fn cleanup(self) -> anyhow::Result<()> {
            self.state.db.close().await;
            drop_database(&self.admin_pool, &self.db_name).await?;
            self.admin_pool.close().await;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn channel_downloads_published_version_until_channel_is_updated() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };

        let v1_binary = b"v1 binary";
        let v1_manifest = manifest_for(&[("git-ai", v1_binary)]);
        let v1_checksum = sha256_hex(&v1_manifest);
        store_release_asset(
            &db.state,
            "latest",
            Some("1.0.0"),
            "SHA256SUMS",
            Some(&v1_checksum),
            &v1_manifest,
        )
        .await?;
        store_release_asset(
            &db.state,
            "latest",
            Some("1.0.0"),
            "git-ai",
            None,
            v1_binary,
        )
        .await?;
        publish_release_channel(&db.state, "latest", "1.0.0", &v1_checksum).await?;

        assert_eq!(
            download_asset(&db.state, "latest", "git-ai").await?,
            b"v1 binary".to_vec()
        );

        let v2_binary = b"v2 binary";
        let v2_manifest = manifest_for(&[("git-ai", v2_binary)]);
        let v2_checksum = sha256_hex(&v2_manifest);
        store_release_asset(
            &db.state,
            "latest",
            Some("2.0.0"),
            "SHA256SUMS",
            Some(&v2_checksum),
            &v2_manifest,
        )
        .await?;
        store_release_asset(
            &db.state,
            "latest",
            Some("2.0.0"),
            "git-ai",
            None,
            v2_binary,
        )
        .await?;

        assert_eq!(
            download_asset(&db.state, "latest", "git-ai").await?,
            b"v1 binary".to_vec()
        );

        publish_release_channel(&db.state, "latest", "2.0.0", &v2_checksum).await?;

        assert_eq!(
            download_asset(&db.state, "latest", "git-ai").await?,
            b"v2 binary".to_vec()
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn immutable_version_channel_can_download_published_asset() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };

        let binary = b"versioned binary";
        let manifest = manifest_for(&[("git-ai-macos-x64", binary)]);
        let checksum = sha256_hex(&manifest);
        store_release_asset(
            &db.state,
            "1.3.3",
            Some("1.3.3"),
            "SHA256SUMS",
            Some(&checksum),
            &manifest,
        )
        .await?;
        store_release_asset(
            &db.state,
            "1.3.3",
            Some("1.3.3"),
            "git-ai-macos-x64",
            None,
            binary,
        )
        .await?;
        publish_release_channel(&db.state, "1.3.3", "1.3.3", &checksum).await?;

        assert_eq!(
            download_asset(&db.state, "1.3.3", "git-ai-macos-x64").await?,
            binary.to_vec()
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_downloads_stay_on_published_version_during_next_upload(
    ) -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };

        let v1_binary = b"v1 binary";
        let v1_manifest = manifest_for(&[("git-ai", v1_binary)]);
        let v1_checksum = sha256_hex(&v1_manifest);
        store_release_asset(
            &db.state,
            "latest",
            Some("1.0.0"),
            "SHA256SUMS",
            Some(&v1_checksum),
            &v1_manifest,
        )
        .await?;
        store_release_asset(
            &db.state,
            "latest",
            Some("1.0.0"),
            "git-ai",
            None,
            v1_binary,
        )
        .await?;
        publish_release_channel(&db.state, "latest", "1.0.0", &v1_checksum).await?;

        let v2_binary = b"v2 binary";
        let v2_manifest = manifest_for(&[("git-ai", v2_binary)]);
        let v2_checksum = sha256_hex(&v2_manifest);
        let upload_state = db.state.clone();
        let upload = tokio::spawn(async move {
            store_release_asset(
                &upload_state,
                "latest",
                Some("2.0.0"),
                "SHA256SUMS",
                Some(&v2_checksum),
                &v2_manifest,
            )
            .await?;
            store_release_asset(
                &upload_state,
                "latest",
                Some("2.0.0"),
                "git-ai",
                None,
                v2_binary,
            )
            .await
            .map(|_| ())
        });

        let downloads = (0..24)
            .map(|_| {
                let state = db.state.clone();
                tokio::spawn(async move { download_asset(&state, "latest", "git-ai").await })
            })
            .collect::<Vec<_>>();

        for download in downloads {
            assert_eq!(download.await??, b"v1 binary".to_vec());
        }
        upload.await??;
        assert_eq!(
            download_asset(&db.state, "latest", "git-ai").await?,
            b"v1 binary".to_vec()
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_release_channel_requires_matching_manifest() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };

        let result = publish_release_channel(&db.state, "latest", "1.0.0", "missing").await;

        assert!(matches!(result, Err(AppError::BadRequest(_))));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_release_channel_requires_manifest_listed_assets() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };

        let manifest = manifest_for(&[("git-ai", b"binary")]);
        let checksum = sha256_hex(&manifest);
        store_release_asset(
            &db.state,
            "latest",
            Some("1.0.0"),
            "SHA256SUMS",
            Some(&checksum),
            &manifest,
        )
        .await?;

        let result = publish_release_channel(&db.state, "latest", "1.0.0", &checksum).await;

        assert!(matches!(result, Err(AppError::BadRequest(_))));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_release_asset_rejects_sha_mismatch() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };

        let result = store_release_asset(
            &db.state,
            "latest",
            Some("1.0.0"),
            "git-ai",
            Some("not-the-real-sha"),
            b"binary",
        )
        .await;

        assert!(matches!(result, Err(AppError::BadRequest(_))));
        assert_eq!(table_count(&db.state.db, "release_assets").await?, 0);

        db.cleanup().await?;
        Ok(())
    }

    fn manifest_for(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut manifest = String::new();
        for (filename, data) in entries {
            manifest.push_str(&format!("{}  {}\n", sha256_hex(data), filename));
        }
        manifest.into_bytes()
    }

    #[test]
    fn release_bundle_validates_version_and_pinned_installers() {
        assert!(validate_release_version("..").is_err());
        assert!(validate_release_version("1.3.4").is_ok());

        let shell = br#"PINNED_VERSION="1.3.4"
ENTERPRISE_RELEASE_CHANNEL="1.3.4""#;
        let powershell = br#"$PinnedVersion = '1.3.4'
$EnterpriseReleaseChannel = '1.3.4'"#;
        assert!(validate_pinned_installer("1.3.4", "install.sh", shell).is_ok());
        assert!(validate_pinned_installer("1.3.4", "install.ps1", powershell).is_ok());
        assert!(validate_pinned_installer("1.3.5", "install.sh", shell).is_err());

        let checksums = "abc  git-ai-linux-x64|def  git-ai-macos-arm64";
        let generated_shell = render_installer_template(
            INSTALL_SH_TEMPLATE,
            "1.3.4",
            checksums,
            "http://example.test:38080",
        );
        assert!(generated_shell.contains("PINNED_VERSION=\"1.3.4\""));
        assert!(generated_shell
            .contains("EMBEDDED_CHECKSUMS=\"abc  git-ai-linux-x64|def  git-ai-macos-arm64\""));
        assert!(
            generated_shell.contains("ENTERPRISE_RELEASE_BASE_URL=\"http://example.test:38080\"")
        );
        assert!(generated_shell.contains("ENTERPRISE_RELEASE_CHANNEL=\"1.3.4\""));
        assert!(
            generated_shell.contains("ENTERPRISE_API_BASE_URL=\"http://example.test:38080\"")
        );

        let generated_powershell = render_installer_template(
            INSTALL_PS1_TEMPLATE,
            "1.3.4",
            checksums,
            "http://example.test:38080",
        );
        assert!(generated_powershell.contains("$PinnedVersion = '1.3.4'"));
        assert!(generated_powershell
            .contains("$EnterpriseReleaseBaseUrl = 'http://example.test:38080'"));
        assert!(generated_powershell.contains("$EnterpriseReleaseChannel = '1.3.4'"));
        assert!(generated_powershell
            .contains("$EnterpriseApiBaseUrl = 'http://example.test:38080'"));
    }

    #[test]
    fn release_bundle_enforces_per_file_and_total_size_limits() {
        assert!(validate_release_binary_size("git-ai-linux-x64", 0, 0).is_err());
        assert!(
            validate_release_binary_size("git-ai-linux-x64", RELEASE_BINARY_MAX_BYTES + 1, 0)
                .is_err()
        );
        assert_eq!(
            validate_release_binary_size(
                "git-ai-linux-x64",
                RELEASE_BINARY_MAX_BYTES,
                RELEASE_UPLOAD_MAX_BYTES - RELEASE_BINARY_MAX_BYTES,
            )
            .unwrap(),
            RELEASE_UPLOAD_MAX_BYTES
        );
        assert!(
            validate_release_binary_size("git-ai-linux-x64", 1, RELEASE_UPLOAD_MAX_BYTES).is_err()
        );
    }

    async fn download_asset(
        state: &AppState,
        channel: &str,
        filename: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let response = download_release(
            State(state.clone()),
            Path((channel.to_string(), filename.to_string())),
        )
        .await?;
        let bytes = to_bytes(response.into_body(), usize::MAX).await?;
        Ok(bytes.to_vec())
    }

    fn local_cas_store(path: &std::path::Path) -> anyhow::Result<crate::services::cas::CasStore> {
        let store = LocalFileSystem::new_with_prefix(path)?;
        Ok(crate::services::cas::CasStore::from_object_store(
            Arc::new(store),
            "test-cas".into(),
        ))
    }

    async fn table_count(pool: &PgPool, table: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await?)
    }

    fn test_config(database_url: &str) -> crate::config::AppConfig {
        crate::config::AppConfig {
            database_url: database_url.to_string(),
            database_max_connections: 20,
            database_min_connections: 1,
            database_acquire_timeout_seconds: 5,
            redis_url: "redis://127.0.0.1:6379".to_string(),
            jwt_secret: "release-test-secret".to_string(),
            s3_endpoint: "http://localhost:9000".to_string(),
            s3_bucket: "git-ai-cas".to_string(),
            s3_access_key: "minioadmin".to_string(),
            s3_secret_key: "minioadmin".to_string(),
            s3_region: "us-east-1".to_string(),
            cas_upload_concurrency: 8,
            auth_password_concurrency: 8,
            metrics_rollup_write_mode: crate::config::MetricsRollupWriteMode::Sync,
            metrics_rollup_worker_enabled: false,
            metrics_rollup_worker_interval_seconds: 5,
            metrics_rollup_worker_batch_size: 100,
            dashboard_use_rollups: false,
            rate_limit_metrics_max_requests: 60,
            rate_limit_metrics_window_seconds: 60,
            rate_limit_cas_upload_max_requests: 30,
            rate_limit_cas_upload_window_seconds: 60,
            rate_limit_cas_read_max_requests: 100,
            rate_limit_cas_read_window_seconds: 60,
            rate_limit_oauth_max_requests: 600,
            rate_limit_oauth_window_seconds: 60,
            rate_limit_auth_max_requests: 300,
            rate_limit_auth_window_seconds: 60,
            rate_limit_admin_max_requests: 30,
            rate_limit_admin_window_seconds: 60,
            rate_limit_default_max_requests: 300,
            rate_limit_default_window_seconds: 60,
            base_url: "http://localhost:8080".to_string(),
            sentry_dsn: String::new(),
            posthog_host: String::new(),
            posthog_api_key: String::new(),
        }
    }

    fn test_database_url() -> String {
        dotenvy::dotenv().ok();
        std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgresql://gitai:gitai@localhost:5433/gitai_enterprise".into())
    }

    fn unique_test_database_name() -> String {
        format!("git_ai_release_test_{}", Uuid::new_v4().simple())
    }

    fn database_url_for_database(database_url: &str, database: &str) -> anyhow::Result<String> {
        let mut url = url::Url::parse(database_url)?;
        url.set_path(database);
        Ok(url.to_string())
    }

    async fn create_database(pool: &PgPool, db_name: &str) -> anyhow::Result<()> {
        sqlx::query(&format!("CREATE DATABASE {}", quote_ident(db_name)))
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn drop_database(pool: &PgPool, db_name: &str) -> anyhow::Result<()> {
        sqlx::query(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_ident(db_name)
        ))
        .execute(pool)
        .await?;
        Ok(())
    }

    fn quote_ident(identifier: &str) -> String {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }
}
