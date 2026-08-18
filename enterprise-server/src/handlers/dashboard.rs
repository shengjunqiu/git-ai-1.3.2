use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Component, PathBuf};
use url::Url;
use uuid::Uuid;

use crate::auth::middleware::{DashboardAuth, OptionalAuth};
use crate::error::AppError;
use crate::pagination::{
    clamp_limit, decode_cursor, encode_cursor, fetch_limit, pagination_meta, truncate_to_limit,
    CURSOR_VERSION, DASHBOARD_MAX_LIMIT, DEFAULT_LIMIT,
};
use crate::routes::AppState;

const TREND_DAY_BUCKET_LIMIT: i64 = 366;
const TREND_WEEK_BUCKET_LIMIT: i64 = 260;
const TREND_MONTH_BUCKET_LIMIT: i64 = 120;
const DASHBOARD_HTML_CACHE_CONTROL: &str = "no-cache";
const DASHBOARD_ASSET_REVALIDATE_CACHE_CONTROL: &str = "public, max-age=0, must-revalidate";
const DASHBOARD_ASSET_IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const DASHBOARD_HELP_START_MARKER: &str = "<!-- DASHBOARD_HELP_START -->";
const DASHBOARD_HELP_END_MARKER: &str = "<!-- DASHBOARD_HELP_END -->";
const DASHBOARD_HELP_PLACEHOLDER: &str = r#"<div id="help-content" aria-busy="true">
                    <div class="empty-state"><div class="empty-icon">❓</div><p>正在加载安装与使用指南...</p></div>
                </div>"#;

/// GET / — Redirect to the dashboard entry point.
pub async fn dashboard_root() -> Redirect {
    Redirect::to("/me")
}

/// GET /me — Dashboard home page
pub async fn dashboard_me(auth: OptionalAuth) -> impl IntoResponse {
    let response = match auth.0 {
        Some(auth) => match render_dashboard_template(&auth) {
            Ok(html) => Html(html).into_response(),
            Err(error) => error.into_response(),
        },
        None => Redirect::to("/auth/login?return_to=/me").into_response(),
    };

    with_dashboard_html_cache_control(response)
}

/// GET /api/v1/dashboard/help — Authenticated, lazily loaded help content.
pub async fn dashboard_help(
    State(state): State<AppState>,
    _auth: DashboardAuth,
) -> impl IntoResponse {
    let response = match render_dashboard_help_template(&state.config.base_url) {
        Ok(html) => Json(json!({ "html": html })).into_response(),
        Err(error) => error.into_response(),
    };

    with_dashboard_html_cache_control(response)
}

/// GET /static/*path — Dashboard static assets.
#[derive(Debug, Default, Deserialize)]
pub struct DashboardStaticAssetQuery {
    pub v: Option<String>,
}

pub async fn dashboard_static_asset(
    Path(asset_path): Path<String>,
    Query(query): Query<DashboardStaticAssetQuery>,
    request_headers: HeaderMap,
) -> Response {
    let relative_path = match sanitize_static_asset_path(&asset_path) {
        Some(path) => path,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let static_dir = match dashboard_static_dir() {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let file_path = static_dir.join(relative_path);
    let bytes = match std::fs::read(&file_path) {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let content_type = dashboard_asset_content_type(&file_path);
    let version = dashboard_asset_version(&bytes);
    let etag = dashboard_asset_etag(&version);
    let cache_control = dashboard_asset_cache_control(&file_path, query.v.as_deref(), &version);
    let mut response = if if_none_match_matches(&request_headers, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        bytes.into_response()
    };
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("SHA256 ETag is a valid header value"),
    );
    if dashboard_asset_is_compressible(content_type) {
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    response
}

fn with_dashboard_html_cache_control(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(DASHBOARD_HTML_CACHE_CONTROL),
    );
    response
}

fn dashboard_asset_version(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn dashboard_asset_etag(version: &str) -> String {
    format!(r#"W/"sha256-{version}""#)
}

fn dashboard_asset_is_compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type.split(';').next().unwrap_or(content_type),
            "application/javascript" | "application/json" | "image/svg+xml"
        )
}

fn dashboard_asset_cache_control(
    path: &std::path::Path,
    requested_version: Option<&str>,
    current_version: &str,
) -> &'static str {
    if path.extension().and_then(|extension| extension.to_str()) == Some("html") {
        return DASHBOARD_HTML_CACHE_CONTROL;
    }
    if requested_version == Some(current_version) {
        DASHBOARD_ASSET_IMMUTABLE_CACHE_CONTROL
    } else {
        DASHBOARD_ASSET_REVALIDATE_CACHE_CONTROL
    }
}

fn if_none_match_matches(headers: &HeaderMap, current_etag: &str) -> bool {
    let current_etag = current_etag.strip_prefix("W/").unwrap_or(current_etag);
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == current_etag
        })
}

fn read_dashboard_template() -> Result<String, AppError> {
    let template_path = dashboard_template_path()?;
    std::fs::read_to_string(&template_path).map_err(|error| {
        AppError::Internal(format!(
            "Failed to read dashboard template {}: {}",
            template_path.display(),
            error
        ))
    })
}

fn split_dashboard_help_template(template: &str) -> Result<(String, String), AppError> {
    let help_start = template
        .find(DASHBOARD_HELP_START_MARKER)
        .ok_or_else(|| AppError::Internal("Dashboard help start marker is missing".to_string()))?;
    let content_start = help_start + DASHBOARD_HELP_START_MARKER.len();
    let relative_end = template[content_start..]
        .find(DASHBOARD_HELP_END_MARKER)
        .ok_or_else(|| AppError::Internal("Dashboard help end marker is missing".to_string()))?;
    let content_end = content_start + relative_end;
    let marker_end = content_end + DASHBOARD_HELP_END_MARKER.len();

    let mut shell = String::with_capacity(
        template.len() - (marker_end - help_start) + DASHBOARD_HELP_PLACEHOLDER.len(),
    );
    shell.push_str(&template[..help_start]);
    shell.push_str(DASHBOARD_HELP_PLACEHOLDER);
    shell.push_str(&template[marker_end..]);

    Ok((
        shell,
        template[content_start..content_end].trim().to_string(),
    ))
}

fn render_dashboard_template(auth: &crate::models::user::AuthIdentity) -> Result<String, AppError> {
    let template = read_dashboard_template()?;
    let (template, _) = split_dashboard_help_template(&template)?;
    let template_path = dashboard_template_path()?;
    let static_dir = template_path.parent().ok_or_else(|| {
        AppError::Internal("Dashboard template path has no parent directory".to_string())
    })?;
    let chart_js_version =
        dashboard_asset_file_version(static_dir, "assets/vendor/chart.js/chart.umd.js")?;
    let dashboard_css_version = dashboard_asset_file_version(static_dir, "dashboard.css")?;
    let dashboard_js_version = dashboard_asset_file_version(static_dir, "dashboard.js")?;

    let is_admin = auth.is_admin();
    let dashboard_bootstrap = serialize_script_safe_json(&DashboardBootstrap { is_admin })?;
    let dashboard_role_class = if is_admin {
        "dashboard-role-admin"
    } else {
        "dashboard-role-member"
    };
    let user_initial = auth
        .name
        .chars()
        .find(|ch| !ch.is_whitespace())
        .unwrap_or('G')
        .to_string();
    let user_role_label = if is_admin { "管理员" } else { "开发者" };

    Ok(template
        .replace("__GITAI_CHART_JS_VERSION__", &chart_js_version)
        .replace("__GITAI_DASHBOARD_CSS_VERSION__", &dashboard_css_version)
        .replace("__GITAI_DASHBOARD_JS_VERSION__", &dashboard_js_version)
        .replace(
            "__GITAI_RELEASE_FILE_MAX_BYTES__",
            &crate::handlers::release::RELEASE_BINARY_MAX_BYTES.to_string(),
        )
        .replace(
            "__GITAI_RELEASE_FILE_MAX_MIB__",
            &(crate::handlers::release::RELEASE_BINARY_MAX_BYTES / 1024 / 1024).to_string(),
        )
        .replace(
            "__GITAI_RELEASE_TOTAL_MAX_BYTES__",
            &crate::handlers::release::RELEASE_UPLOAD_MAX_BYTES.to_string(),
        )
        .replace(
            "__GITAI_RELEASE_TOTAL_MAX_MIB__",
            &(crate::handlers::release::RELEASE_UPLOAD_MAX_BYTES / 1024 / 1024).to_string(),
        )
        .replace(
            "__GITAI_MANAGED_FILE_MAX_BYTES__",
            &crate::handlers::managed_files::MANAGED_FILE_MAX_BYTES.to_string(),
        )
        .replace(
            "__GITAI_MANAGED_FILE_MAX_MIB__",
            &(crate::handlers::managed_files::MANAGED_FILE_MAX_BYTES / 1024 / 1024).to_string(),
        )
        .replace("__GITAI_DASHBOARD_BOOTSTRAP__", &dashboard_bootstrap)
        .replace("__GITAI_DASHBOARD_ROLE_CLASS__", dashboard_role_class)
        .replace("__GITAI_USER_NAME__", &html_escape(&auth.name))
        .replace("__GITAI_USER_EMAIL__", &html_escape(&auth.email))
        .replace("__GITAI_USER_INITIAL__", &html_escape(&user_initial))
        .replace("__GITAI_USER_ROLE_LABEL__", &html_escape(user_role_label)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardBootstrap {
    is_admin: bool,
}

fn serialize_script_safe_json<T: Serialize>(value: &T) -> Result<String, AppError> {
    let json = serde_json::to_string(value).map_err(|error| {
        AppError::Internal(format!(
            "Failed to serialize dashboard bootstrap data: {error}"
        ))
    })?;
    Ok(json
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

fn render_dashboard_help_template(public_base_url: &str) -> Result<String, AppError> {
    let template = read_dashboard_template()?;
    let (_, help_template) = split_dashboard_help_template(&template)?;
    let public_base_url = public_base_url.trim_end_matches('/');
    let install_security_notice = r#"<div class="help-callout success"><strong>一键安装</strong><p>安装脚本会自动识别操作系统和处理器架构，并校验下载的客户端二进制文件。</p></div>"#;

    Ok(help_template
        .replace("__GITAI_PUBLIC_BASE_URL__", &html_escape(public_base_url))
        .replace("__GITAI_INSTALL_SECURITY_NOTICE__", install_security_notice))
}

fn dashboard_asset_file_version(
    static_dir: &std::path::Path,
    relative_path: &str,
) -> Result<String, AppError> {
    let file_path = static_dir.join(relative_path);
    let bytes = std::fs::read(&file_path).map_err(|error| {
        AppError::Internal(format!(
            "Failed to read dashboard asset {}: {}",
            file_path.display(),
            error
        ))
    })?;
    Ok(dashboard_asset_version(&bytes))
}

pub fn dashboard_static_dir() -> Result<std::path::PathBuf, AppError> {
    if let Ok(path) = std::env::var("GIT_AI_DASHBOARD_TEMPLATE") {
        return std::path::PathBuf::from(path)
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| AppError::Internal("Invalid dashboard template path".to_string()));
    }

    let current_dir_path = std::env::current_dir()
        .map_err(|error| {
            AppError::Internal(format!("Failed to resolve current directory: {}", error))
        })?
        .join("static");
    if current_dir_path.join("dashboard.html").exists() {
        return Ok(current_dir_path);
    }

    Ok(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"))
}

fn dashboard_template_path() -> Result<std::path::PathBuf, AppError> {
    if let Ok(path) = std::env::var("GIT_AI_DASHBOARD_TEMPLATE") {
        return Ok(std::path::PathBuf::from(path));
    }

    Ok(dashboard_static_dir()?.join("dashboard.html"))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn sanitize_static_asset_path(asset_path: &str) -> Option<PathBuf> {
    let mut sanitized = PathBuf::new();
    for component in std::path::Path::new(asset_path).components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if sanitized.as_os_str().is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn dashboard_asset_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Deserialize)]
pub struct AggregateQuery {
    pub org: Option<String>,
    pub parent_id: Option<Uuid>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OrganizationAggregateCursor {
    v: u8,
    name: String,
    slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DepartmentAggregateCursor {
    v: u8,
    parent_id: Option<Uuid>,
    org_name: String,
    sort_path: Vec<String>,
    department_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ProjectAggregateCursor {
    v: u8,
    project_name: String,
    project_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DeveloperAggregateCursor {
    v: u8,
    sort_by: String,
    sort_order: String,
    ai_added_lines: i64,
    total_added_lines: i64,
    total_commits: i64,
    name: String,
    user_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ToolAggregateCursor {
    v: u8,
    ai_additions: i64,
    tool_model: String,
}

fn decode_dashboard_cursor<T>(cursor: Option<&str>) -> Result<Option<T>, AppError>
where
    T: serde::de::DeserializeOwned + DashboardCursor,
{
    let cursor: Option<T> = cursor.map(decode_cursor).transpose()?;
    if let Some(cursor) = &cursor {
        cursor.validate_version()?;
    }
    Ok(cursor)
}

trait DashboardCursor {
    fn version(&self) -> u8;

    fn validate_version(&self) -> Result<(), AppError> {
        if self.version() == CURSOR_VERSION {
            Ok(())
        } else {
            Err(AppError::BadRequest(format!(
                "Unsupported pagination cursor version: {}",
                self.version()
            )))
        }
    }
}

impl DashboardCursor for OrganizationAggregateCursor {
    fn version(&self) -> u8 {
        self.v
    }
}

impl DashboardCursor for DepartmentAggregateCursor {
    fn version(&self) -> u8 {
        self.v
    }
}

impl DashboardCursor for ProjectAggregateCursor {
    fn version(&self) -> u8 {
        self.v
    }
}

impl DashboardCursor for DeveloperAggregateCursor {
    fn version(&self) -> u8 {
        self.v
    }
}

impl DashboardCursor for ToolAggregateCursor {
    fn version(&self) -> u8 {
        self.v
    }
}

#[derive(Debug, Clone)]
struct ProjectAggregate {
    project_id: Option<i64>,
    repo_url: String,
    project_name: String,
    branch: Option<String>,
    organization: Option<String>,
    department: Option<String>,
    total_commits: i64,
    total_code: i64,
    total_ai: i64,
}

impl ProjectAggregate {
    fn total_human(&self) -> i64 {
        (self.total_code - self.total_ai).max(0)
    }

    fn pct_ai(&self) -> f64 {
        if self.total_code > 0 {
            (self.total_ai as f64 / self.total_code as f64) * 100.0
        } else {
            0.0
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "project_id": self.project_id,
            "repo_url": self.repo_url,
            "project_name": self.project_name,
            "branch": self.branch,
            "organization": self.organization,
            "department": self.department,
            "total_commits": self.total_commits,
            "total_code": self.total_code,
            "total_ai": self.total_ai,
            "total_human": self.total_human(),
            "pct_ai": self.pct_ai(),
            "is_unassigned": self.repo_url.trim().is_empty(),
        })
    }
}

fn repo_project_key(repo_url: &str) -> String {
    let normalized = normalize_repo_url(repo_url).unwrap_or_else(|_| repo_url.trim().to_string());
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn normalize_repo_url(url_str: &str) -> Result<String, String> {
    let url_str = url_str.trim();

    if !url_str.contains("://") {
        if let Some((user_host, path)) = url_str.split_once(':') {
            if let Some((_, host)) = user_host.rsplit_once('@') {
                return normalize_ssh_url(host, path);
            }
        }
    }

    let url = Url::parse(url_str).map_err(|e| format!("Invalid URL: {}", e))?;
    let scheme = url.scheme();
    if !["https", "http", "git", "ssh"].contains(&scheme) {
        return Err(format!("Unsupported URL scheme: {}", scheme));
    }

    let host = url.host_str().ok_or("URL must have a host")?;
    let path = url.path().trim_end_matches('/').trim_end_matches(".git");
    let canonical = format!("https://{}{}", host, path);
    validate_normalized_repo_url(&canonical)?;
    Ok(canonical)
}

fn normalize_ssh_url(host: &str, path: &str) -> Result<String, String> {
    if host.is_empty() || path.is_empty() {
        return Err("Invalid SSH URL format".to_string());
    }

    let path = path
        .trim_start_matches('/')
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let canonical = format!("https://{}/{}", host, path);
    validate_normalized_repo_url(&canonical)?;
    Ok(canonical)
}

fn validate_normalized_repo_url(url_str: &str) -> Result<(), String> {
    let url = Url::parse(url_str).map_err(|e| format!("Failed to parse normalized URL: {}", e))?;
    if url.scheme() != "https" {
        return Err("Normalized URL must be HTTPS".to_string());
    }
    if url.host_str().is_none() {
        return Err("Normalized URL must have a valid host".to_string());
    }
    if url.path().is_empty() || url.path() == "/" {
        return Err("Normalized URL must have a path".to_string());
    }
    Ok(())
}

fn parse_epoch_seconds_param(name: &str, value: Option<&str>) -> Result<Option<i64>, AppError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(Some(dt.timestamp()));
    }

    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let dt = date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time")
            .and_utc();
        return Ok(Some(dt.timestamp()));
    }

    Err(AppError::BadRequest(format!(
        "{} must be an RFC3339 timestamp or YYYY-MM-DD date",
        name
    )))
}

fn parse_epoch_filters(
    since: Option<&str>,
    until: Option<&str>,
) -> Result<(Option<i64>, Option<i64>), AppError> {
    Ok((
        parse_epoch_seconds_param("since", since)?,
        parse_epoch_seconds_param("until", until)?,
    ))
}

fn bounded_trend_epoch_filters(
    since: Option<&str>,
    until: Option<&str>,
    granularity: &str,
) -> Result<(i64, i64), AppError> {
    let parsed_until = parse_epoch_seconds_param("until", until)?
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let parsed_since = match parse_epoch_seconds_param("since", since)? {
        Some(value) => value,
        None => default_trend_since(parsed_until, granularity)?,
    };

    if parsed_since > parsed_until {
        return Err(AppError::BadRequest(
            "since must be earlier than or equal to until".into(),
        ));
    }

    let bucket_count = trend_bucket_count(parsed_since, parsed_until, granularity)?;
    let max_buckets = trend_bucket_limit(granularity);
    if bucket_count > max_buckets {
        return Err(AppError::BadRequest(format!(
            "Requested trend range produces {bucket_count} {granularity} buckets; maximum is {max_buckets}. Reduce since/until or use a coarser granularity."
        )));
    }

    Ok((parsed_since, parsed_until))
}

fn default_trend_since(until_ts: i64, granularity: &str) -> Result<i64, AppError> {
    let until_date = epoch_seconds_to_date(until_ts)?;
    let since_date = match granularity {
        "day" => until_date - chrono::Duration::days(TREND_DAY_BUCKET_LIMIT - 1),
        "week" => until_date - chrono::Duration::weeks(TREND_WEEK_BUCKET_LIMIT - 1),
        "month" => subtract_months(until_date, (TREND_MONTH_BUCKET_LIMIT - 1) as u32)?,
        _ => until_date - chrono::Duration::weeks(TREND_WEEK_BUCKET_LIMIT - 1),
    };
    Ok(date_start_epoch_seconds(since_date))
}

fn trend_bucket_limit(granularity: &str) -> i64 {
    match granularity {
        "day" => TREND_DAY_BUCKET_LIMIT,
        "week" => TREND_WEEK_BUCKET_LIMIT,
        "month" => TREND_MONTH_BUCKET_LIMIT,
        _ => TREND_WEEK_BUCKET_LIMIT,
    }
}

fn trend_bucket_count(since_ts: i64, until_ts: i64, granularity: &str) -> Result<i64, AppError> {
    let since_date = epoch_seconds_to_date(since_ts)?;
    let until_date = epoch_seconds_to_date(until_ts)?;
    let days = (until_date - since_date).num_days().max(0);
    let buckets = match granularity {
        "day" => days + 1,
        "week" => (days / 7) + 1,
        "month" => {
            let since_month = i64::from(since_date.year()) * 12 + i64::from(since_date.month0());
            let until_month = i64::from(until_date.year()) * 12 + i64::from(until_date.month0());
            (until_month - since_month) + 1
        }
        _ => (days / 7) + 1,
    };
    Ok(buckets)
}

fn epoch_seconds_to_date(timestamp: i64) -> Result<chrono::NaiveDate, AppError> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|dt| dt.date_naive())
        .ok_or_else(|| AppError::BadRequest("timestamp is out of range".into()))
}

fn date_start_epoch_seconds(date: chrono::NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc()
        .timestamp()
}

fn epoch_seconds_to_rfc3339(timestamp: i64) -> Result<String, AppError> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|dt| dt.to_rfc3339())
        .ok_or_else(|| AppError::BadRequest("timestamp is out of range".into()))
}

fn subtract_months(date: chrono::NaiveDate, months: u32) -> Result<chrono::NaiveDate, AppError> {
    let month_index = date.year() * 12 + date.month0() as i32 - months as i32;
    let year = month_index.div_euclid(12);
    let month0 = month_index.rem_euclid(12) as u32;
    let month = month0 + 1;
    let day = date.day().min(days_in_month(year, month));
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| AppError::BadRequest("timestamp is out of range".into()))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let first_next_month = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .expect("valid month");
    (first_next_month - chrono::Duration::days(1)).day()
}

type SummaryMetricRow = (Option<i64>, Option<i64>, Option<i64>, Option<i64>);
type TrendMetricRow = (chrono::NaiveDate, Option<i64>, Option<i64>, Option<i64>);
#[cfg(test)]
type ToolMetricRow = (
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

#[derive(Debug, Default)]
struct ToolAggregate {
    has_report: bool,
    has_metrics: bool,
    ai_additions: i64,
    mixed_additions: i64,
    ai_accepted: i64,
    total_ai_additions: i64,
    total_ai_deletions: i64,
    commits: i64,
}

impl ToolAggregate {
    fn add_report_row(
        &mut self,
        ai_additions: i64,
        mixed_additions: i64,
        ai_accepted: i64,
        total_ai_additions: i64,
        total_ai_deletions: i64,
    ) {
        self.has_report = true;
        self.add_values(
            ai_additions,
            mixed_additions,
            ai_accepted,
            total_ai_additions,
            total_ai_deletions,
        );
    }

    fn add_metrics_row(
        &mut self,
        ai_additions: i64,
        mixed_additions: i64,
        ai_accepted: i64,
        total_ai_additions: i64,
        total_ai_deletions: i64,
    ) {
        self.has_metrics = true;
        self.add_values(
            ai_additions,
            mixed_additions,
            ai_accepted,
            total_ai_additions,
            total_ai_deletions,
        );
    }

    fn add_values(
        &mut self,
        ai_additions: i64,
        mixed_additions: i64,
        ai_accepted: i64,
        total_ai_additions: i64,
        total_ai_deletions: i64,
    ) {
        self.ai_additions += ai_additions;
        self.mixed_additions += mixed_additions;
        self.ai_accepted += ai_accepted;
        self.total_ai_additions += total_ai_additions.max(ai_additions);
        self.total_ai_deletions += total_ai_deletions;
    }

    fn source(&self) -> &'static str {
        match (self.has_report, self.has_metrics) {
            (true, true) => "report+metrics",
            (true, false) => "report",
            (false, true) => "metrics",
            (false, false) => "metrics",
        }
    }
}
type AgentMetricRow = (
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

async fn fetch_metrics_summary_row(
    pool: &sqlx::PgPool,
    use_rollups: bool,
    user_filter: Option<Uuid>,
    org_filter: Option<Uuid>,
    since_ts: Option<i64>,
    until_ts: Option<i64>,
) -> Result<SummaryMetricRow, AppError> {
    if use_rollups {
        return sqlx::query_as(
            r#"SELECT
                COALESCE(SUM(commits), 0)::bigint as total_commits,
                COALESCE(SUM(total_lines), 0)::bigint as total_code_lines,
                COALESCE(SUM(ai_lines), 0)::bigint as total_ai_lines,
                COALESCE(SUM(human_lines), 0)::bigint as total_human_lines
            FROM metrics_daily_rollups
            WHERE tool_model = ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::bigint IS NULL OR day >= (to_timestamp($3) AT TIME ZONE 'UTC')::date)
              AND ($4::bigint IS NULL OR day <= (to_timestamp($4) AT TIME ZONE 'UTC')::date)"#,
        )
        .bind(user_filter)
        .bind(org_filter)
        .bind(since_ts)
        .bind(until_ts)
        .fetch_one(pool)
        .await
        .map_err(|e| AppError::Database(e));
    }

    sqlx::query_as(
        r#"SELECT
            COUNT(*) as total_commits,
            COALESCE(SUM(git_diff_added_lines), 0) as total_code_lines,
            COALESCE(SUM(ai_additions), 0) as total_ai_lines,
            COALESCE(SUM(GREATEST(COALESCE(git_diff_added_lines, 0) - COALESCE(ai_additions, 0), 0)), 0) as total_human_lines
        FROM metrics_events WHERE event_type = 1
          AND ($1::uuid IS NULL OR user_id = $1)
          AND ($2::uuid IS NULL OR org_id = $2)
          AND ($3::bigint IS NULL OR timestamp >= $3)
          AND ($4::bigint IS NULL OR timestamp <= $4)"#,
    )
    .bind(user_filter)
    .bind(org_filter)
    .bind(since_ts)
    .bind(until_ts)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Database(e))
}

async fn fetch_total_developers(
    pool: &sqlx::PgPool,
    use_rollups: bool,
    user_filter: Option<Uuid>,
    org_filter: Option<Uuid>,
    since_ts: Option<i64>,
    until_ts: Option<i64>,
    since_text: &Option<String>,
    until_text: &Option<String>,
) -> Result<i64, AppError> {
    let metrics_source = if use_rollups {
        r#"SELECT user_id
            FROM metrics_daily_rollups
            WHERE tool_model = ''
              AND user_id <> '00000000-0000-0000-0000-000000000000'::uuid
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::bigint IS NULL OR day >= (to_timestamp($3) AT TIME ZONE 'UTC')::date)
              AND ($4::bigint IS NULL OR day <= (to_timestamp($4) AT TIME ZONE 'UTC')::date)"#
    } else {
        r#"SELECT user_id
            FROM metrics_events
            WHERE event_type = 1
              AND user_id IS NOT NULL
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::bigint IS NULL OR timestamp >= $3)
              AND ($4::bigint IS NULL OR timestamp <= $4)"#
    };

    sqlx::query_scalar::<_, i64>(&format!(
        r#"SELECT COUNT(DISTINCT user_id)::bigint
        FROM (
            {metrics_source}

            UNION ALL

            SELECT p.user_id
            FROM projects p
            JOIN commit_stats cs ON cs.project_id = p.id
            WHERE p.user_id IS NOT NULL
              AND ($1::uuid IS NULL OR p.user_id = $1)
              AND ($2::uuid IS NULL OR p.org_id = $2)
              AND ($5::timestamptz IS NULL OR cs.author_time_at >= $5::timestamptz)
              AND ($6::timestamptz IS NULL OR cs.author_time_at <= $6::timestamptz)
              AND NOT EXISTS (
                  SELECT 1 FROM metrics_events m
                  WHERE m.event_type = 1
                    AND m.commit_sha = cs.sha
                    AND ($1::uuid IS NULL OR m.user_id = $1)
                    AND ($2::uuid IS NULL OR m.org_id = $2)
              )
        ) combined
        WHERE user_id IS NOT NULL"#
    ))
    .bind(user_filter)
    .bind(org_filter)
    .bind(since_ts)
    .bind(until_ts)
    .bind(since_text)
    .bind(until_text)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Database(e))
}

async fn fetch_metrics_project_urls(
    pool: &sqlx::PgPool,
    use_rollups: bool,
    user_filter: Option<Uuid>,
    org_filter: Option<Uuid>,
    since_ts: Option<i64>,
    until_ts: Option<i64>,
) -> Result<Vec<String>, AppError> {
    if use_rollups {
        return sqlx::query_scalar::<_, String>(
            r#"SELECT DISTINCT repo_url
            FROM metrics_daily_rollups
            WHERE tool_model = '' AND repo_url != ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::bigint IS NULL OR day >= (to_timestamp($3) AT TIME ZONE 'UTC')::date)
              AND ($4::bigint IS NULL OR day <= (to_timestamp($4) AT TIME ZONE 'UTC')::date)"#,
        )
        .bind(user_filter)
        .bind(org_filter)
        .bind(since_ts)
        .bind(until_ts)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::Database(e));
    }

    sqlx::query_scalar::<_, String>(
        r#"SELECT DISTINCT repo_url
        FROM metrics_events
        WHERE event_type = 1 AND repo_url IS NOT NULL AND repo_url != ''
          AND ($1::uuid IS NULL OR user_id = $1)
          AND ($2::uuid IS NULL OR org_id = $2)
          AND ($3::bigint IS NULL OR timestamp >= $3)
          AND ($4::bigint IS NULL OR timestamp <= $4)"#,
    )
    .bind(user_filter)
    .bind(org_filter)
    .bind(since_ts)
    .bind(until_ts)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e))
}

async fn fetch_trend_rows(
    pool: &sqlx::PgPool,
    use_rollups: bool,
    user_filter: Option<Uuid>,
    org_filter: Option<Uuid>,
    org_slug: &Option<String>,
    since_ts: Option<i64>,
    until_ts: Option<i64>,
    since_text: &Option<String>,
    until_text: &Option<String>,
    date_trunc: &str,
) -> Result<Vec<TrendMetricRow>, AppError> {
    let metrics_source = if use_rollups {
        format!(
            r#"SELECT
                DATE_TRUNC('{0}', day::timestamp)::date AS period,
                COALESCE(SUM(ai_lines), 0)::bigint AS ai_lines,
                COALESCE(SUM(human_lines), 0)::bigint AS human_lines,
                COALESCE(SUM(commits), 0)::bigint AS commits
            FROM metrics_daily_rollups
            WHERE tool_model = ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
              AND ($4::bigint IS NULL OR day >= (to_timestamp($4) AT TIME ZONE 'UTC')::date)
              AND ($5::bigint IS NULL OR day <= (to_timestamp($5) AT TIME ZONE 'UTC')::date)
            GROUP BY DATE_TRUNC('{0}', day::timestamp)"#,
            date_trunc
        )
    } else {
        format!(
            r#"SELECT
                DATE_TRUNC('{0}', to_timestamp(timestamp) AT TIME ZONE 'UTC')::date AS period,
                COALESCE(SUM(ai_additions), 0)::bigint AS ai_lines,
                COALESCE(SUM(GREATEST(COALESCE(git_diff_added_lines, 0) - COALESCE(ai_additions, 0), 0)), 0)::bigint AS human_lines,
                COUNT(*)::bigint AS commits
            FROM metrics_events
            WHERE event_type = 1
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
              AND ($4::bigint IS NULL OR timestamp >= $4)
              AND ($5::bigint IS NULL OR timestamp <= $5)
            GROUP BY DATE_TRUNC('{0}', to_timestamp(timestamp) AT TIME ZONE 'UTC')"#,
            date_trunc
        )
    };

    let sql = format!(
        r#"SELECT
            period,
            COALESCE(SUM(ai_lines), 0)::bigint AS ai_lines,
            COALESCE(SUM(human_lines), 0)::bigint AS human_lines,
            COALESCE(SUM(commits), 0)::bigint AS commits
        FROM (
            {metrics_source}

            UNION ALL

            SELECT
                DATE_TRUNC('{0}', cs.author_time_at AT TIME ZONE 'UTC')::date AS period,
                COALESCE(SUM(cs.ai_additions), 0)::bigint AS ai_lines,
                COALESCE(SUM(GREATEST(COALESCE(cs.git_diff_added_lines, 0) - COALESCE(cs.ai_additions, 0), 0)), 0)::bigint AS human_lines,
                COUNT(*)::bigint AS commits
            FROM projects p
            JOIN commit_stats cs ON cs.project_id = p.id
            WHERE cs.author_time_at IS NOT NULL
              AND ($1::uuid IS NULL OR p.user_id = $1)
              AND ($2::uuid IS NULL OR p.org_id = $2)
              AND ($3::text IS NULL OR p.org_id = (SELECT id FROM organizations WHERE slug = $3))
              AND ($6::timestamptz IS NULL OR cs.author_time_at >= $6::timestamptz)
              AND ($7::timestamptz IS NULL OR cs.author_time_at <= $7::timestamptz)
              AND NOT EXISTS (
                  SELECT 1 FROM metrics_events m
                  WHERE m.event_type = 1
                    AND m.commit_sha = cs.sha
                    AND ($1::uuid IS NULL OR m.user_id = $1)
                    AND ($2::uuid IS NULL OR m.org_id = $2)
              )
            GROUP BY DATE_TRUNC('{0}', cs.author_time_at AT TIME ZONE 'UTC')
        ) combined
        GROUP BY period
        ORDER BY period"#,
        date_trunc
    );

    sqlx::query_as(&sql)
        .bind(user_filter)
        .bind(org_filter)
        .bind(org_slug)
        .bind(since_ts)
        .bind(until_ts)
        .bind(since_text)
        .bind(until_text)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::Database(e))
}

#[cfg(test)]
async fn fetch_metrics_tool_rows(
    pool: &sqlx::PgPool,
    use_rollups: bool,
    user_filter: Option<Uuid>,
    org_filter: Option<Uuid>,
) -> Result<Vec<ToolMetricRow>, AppError> {
    if use_rollups {
        return sqlx::query_as(
            r#"SELECT
                tool_model,
                COALESCE(SUM(ai_lines), 0)::bigint AS ai_additions,
                COALESCE(SUM(mixed_lines), 0)::bigint AS mixed_additions,
                COALESCE(SUM(ai_accepted), 0)::bigint AS ai_accepted,
                COALESCE(SUM(ai_lines), 0)::bigint AS total_ai_additions,
                0::bigint AS total_ai_deletions
            FROM metrics_daily_rollups
            WHERE tool_model != ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
            GROUP BY tool_model
            ORDER BY SUM(ai_lines) DESC"#,
        )
        .bind(user_filter)
        .bind(org_filter)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::Database(e));
    }

    sqlx::query_as(
        r#"SELECT
            tool_model,
            COALESCE(SUM(ai_additions), 0)::bigint AS ai_additions,
            COALESCE(SUM(mixed_additions), 0)::bigint AS mixed_additions,
            COALESCE(SUM(ai_accepted), 0)::bigint AS ai_accepted,
            COALESCE(SUM(total_ai_additions), 0)::bigint AS total_ai_additions,
            COALESCE(SUM(total_ai_deletions), 0)::bigint AS total_ai_deletions
        FROM metrics_tool_model_events
        WHERE ($1::uuid IS NULL OR user_id = $1)
          AND ($2::uuid IS NULL OR org_id = $2)
        GROUP BY tool_model
        ORDER BY SUM(ai_additions) DESC"#,
    )
    .bind(user_filter)
    .bind(org_filter)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e))
}

async fn fetch_metrics_agent_rows(
    pool: &sqlx::PgPool,
    use_rollups: bool,
    user_filter: Option<Uuid>,
    org_filter: Option<Uuid>,
    org_slug: &Option<String>,
) -> Result<Vec<AgentMetricRow>, AppError> {
    if use_rollups {
        return sqlx::query_as(
            r#"SELECT
                tool_model,
                COALESCE(SUM(ai_lines), 0)::bigint AS ai_additions,
                COALESCE(SUM(mixed_lines), 0)::bigint AS mixed_additions,
                COALESCE(SUM(ai_accepted), 0)::bigint AS ai_accepted,
                COALESCE(SUM(ai_lines), 0)::bigint AS total_ai_additions,
                0::bigint AS total_ai_deletions,
                COALESCE(SUM(commits), 0)::bigint AS commits
            FROM metrics_daily_rollups
            WHERE tool_model != ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY tool_model
            ORDER BY SUM(ai_lines) DESC"#,
        )
        .bind(user_filter)
        .bind(org_filter)
        .bind(org_slug)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::Database(e));
    }

    sqlx::query_as(
        r#"SELECT
            tool_model,
            COALESCE(SUM(ai_additions), 0)::bigint AS ai_additions,
            COALESCE(SUM(mixed_additions), 0)::bigint AS mixed_additions,
            COALESCE(SUM(ai_accepted), 0)::bigint AS ai_accepted,
            COALESCE(SUM(total_ai_additions), 0)::bigint AS total_ai_additions,
            COALESCE(SUM(total_ai_deletions), 0)::bigint AS total_ai_deletions,
            COUNT(DISTINCT metric_event_id)::bigint AS commits
        FROM metrics_tool_model_events
        WHERE ($1::uuid IS NULL OR user_id = $1)
          AND ($2::uuid IS NULL OR org_id = $2)
          AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
        GROUP BY tool_model
        ORDER BY SUM(ai_additions) DESC"#,
    )
    .bind(user_filter)
    .bind(org_filter)
    .bind(org_slug)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e))
}

/// GET /api/v1/aggregate/summary — Global aggregate summary
pub async fn aggregate_summary(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<AggregateQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);
    let (since_ts, until_ts) = parse_epoch_filters(query.since.as_deref(), query.until.as_deref())?;

    let row = fetch_metrics_summary_row(
        &state.db,
        state.config.dashboard_use_rollups,
        user_filter,
        org_filter,
        since_ts,
        until_ts,
    )
    .await?;

    let report_row: (Option<i64>, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        r#"SELECT
            COUNT(cs.sha) as total_commits,
            COALESCE(SUM(cs.git_diff_added_lines), 0) as total_code_lines,
            COALESCE(SUM(cs.ai_additions), 0) as total_ai_lines,
            COALESCE(SUM(GREATEST(COALESCE(cs.git_diff_added_lines, 0) - COALESCE(cs.ai_additions, 0), 0)), 0) as total_human_lines
        FROM projects p
        JOIN commit_stats cs ON cs.project_id = p.id
        WHERE ($1::uuid IS NULL OR p.user_id = $1)
          AND ($2::uuid IS NULL OR p.org_id = $2)
          AND ($3::timestamptz IS NULL OR cs.author_time_at >= $3::timestamptz)
          AND ($4::timestamptz IS NULL OR cs.author_time_at <= $4::timestamptz)
          AND NOT EXISTS (
              SELECT 1 FROM metrics_events m
              WHERE m.event_type = 1
                AND m.commit_sha = cs.sha
                AND ($1::uuid IS NULL OR m.user_id = $1)
                AND ($2::uuid IS NULL OR m.org_id = $2)
          )"#,
    )
    .bind(user_filter)
    .bind(org_filter)
    .bind(&query.since)
    .bind(&query.until)
    .fetch_one(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let total_commits = row.0.unwrap_or(0) + report_row.0.unwrap_or(0);
    let total_code = row.1.unwrap_or(0) + report_row.1.unwrap_or(0);
    let total_ai = row.2.unwrap_or(0) + report_row.2.unwrap_or(0);
    let total_human = row.3.unwrap_or(0) + report_row.3.unwrap_or(0);
    let total_developers = fetch_total_developers(
        &state.db,
        state.config.dashboard_use_rollups,
        user_filter,
        org_filter,
        since_ts,
        until_ts,
        &query.since,
        &query.until,
    )
    .await?;
    let metrics_project_urls = fetch_metrics_project_urls(
        &state.db,
        state.config.dashboard_use_rollups,
        user_filter,
        org_filter,
        since_ts,
        until_ts,
    )
    .await?;

    let report_project_hashes: Vec<String> = sqlx::query_scalar::<_, String>(
        r#"SELECT DISTINCT p.remote_url_hash
        FROM projects p
        JOIN commit_stats cs ON cs.project_id = p.id
        WHERE p.remote_url_hash IS NOT NULL AND p.remote_url_hash != ''
          AND ($1::uuid IS NULL OR p.user_id = $1)
          AND ($2::uuid IS NULL OR p.org_id = $2)
          AND ($3::timestamptz IS NULL OR cs.author_time_at >= $3::timestamptz)
          AND ($4::timestamptz IS NULL OR cs.author_time_at <= $4::timestamptz)
          AND NOT EXISTS (
              SELECT 1 FROM metrics_events m
              WHERE m.event_type = 1
                AND m.commit_sha = cs.sha
                AND ($1::uuid IS NULL OR m.user_id = $1)
                AND ($2::uuid IS NULL OR m.org_id = $2)
          )"#,
    )
    .bind(user_filter)
    .bind(org_filter)
    .bind(&query.since)
    .bind(&query.until)
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let mut project_keys = std::collections::HashSet::new();
    for repo_url in metrics_project_urls {
        project_keys.insert(repo_project_key(&repo_url));
    }
    for remote_url_hash in report_project_hashes {
        project_keys.insert(remote_url_hash);
    }
    let total_projects = project_keys.len() as i64;
    let pct_ai = if total_code > 0 {
        (total_ai as f64 / total_code as f64) * 100.0
    } else {
        0.0
    };

    Ok(Json(json!({
        "total_commits": total_commits,
        "total_code_lines": total_code,
        "total_ai_lines": total_ai,
        "total_human_lines": total_human,
        "pct_ai_lines": pct_ai,
        "total_developers": total_developers,
        "total_projects": total_projects,
    })))
}

/// GET /api/v1/aggregate/organizations
pub async fn aggregate_organizations(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<AggregateQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);
    let limit = clamp_limit(query.limit, DEFAULT_LIMIT, DASHBOARD_MAX_LIMIT);
    let cursor: Option<OrganizationAggregateCursor> =
        decode_dashboard_cursor(query.cursor.as_deref())?;
    let cursor_name = cursor.as_ref().map(|cursor| cursor.name.clone());
    let cursor_slug = cursor.as_ref().map(|cursor| cursor.slug.clone());

    let metrics_organization_rows = if state.config.dashboard_use_rollups {
        r#"SELECT
                NULLIF(org_id, '00000000-0000-0000-0000-000000000000'::uuid) AS org_id,
                COALESCE(SUM(commits), 0)::bigint AS commits,
                COALESCE(SUM(total_lines), 0)::bigint AS total_lines,
                COALESCE(SUM(ai_lines), 0)::bigint AS ai_lines
            FROM metrics_daily_rollups
            WHERE tool_model = ''
              AND org_id <> '00000000-0000-0000-0000-000000000000'::uuid
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY org_id"#
    } else {
        r#"SELECT
                org_id,
                COUNT(*)::bigint AS commits,
                COALESCE(SUM(git_diff_added_lines), 0)::bigint AS total_lines,
                COALESCE(SUM(ai_additions), 0)::bigint AS ai_lines
            FROM metrics_events
            WHERE event_type = 1
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY org_id"#
    };

    let rows: Vec<(String, String, Option<i64>, Option<i64>, Option<i64>)> = sqlx::query_as(
        &format!(
            r#"SELECT
            o.name, o.slug,
            COALESCE(stats.commits, 0),
            COALESCE(stats.total_lines, 0),
            COALESCE(stats.ai_lines, 0)
        FROM organizations o
        LEFT JOIN (
            SELECT
                org_id,
                SUM(commits)::bigint AS commits,
                SUM(total_lines)::bigint AS total_lines,
                SUM(ai_lines)::bigint AS ai_lines
            FROM (
                {metrics_organization_rows}

                UNION ALL

                SELECT
                    p.org_id,
                    COUNT(cs.sha)::bigint AS commits,
                    COALESCE(SUM(cs.git_diff_added_lines), 0)::bigint AS total_lines,
                    COALESCE(SUM(cs.ai_additions), 0)::bigint AS ai_lines
                FROM projects p
                JOIN LATERAL (
                    SELECT cs.sha, cs.git_diff_added_lines, cs.ai_additions
                    FROM commit_stats cs
                    WHERE cs.project_id = p.id
                      AND NOT EXISTS (
                          SELECT 1 FROM metrics_events m
                          WHERE m.event_type = 1
                            AND m.commit_sha = cs.sha
                            AND ($1::uuid IS NULL OR m.user_id = $1)
                            AND ($2::uuid IS NULL OR m.org_id = $2)
                      )
                ) cs ON TRUE
                WHERE ($1::uuid IS NULL OR p.user_id = $1)
                  AND ($2::uuid IS NULL OR p.org_id = $2)
                  AND ($3::text IS NULL OR p.org_id = (SELECT id FROM organizations WHERE slug = $3))
                GROUP BY p.org_id
            ) combined
            WHERE org_id IS NOT NULL
            GROUP BY org_id
        ) stats ON stats.org_id = o.id
        WHERE ($2::uuid IS NULL OR o.id = $2)
          AND ($3::text IS NULL OR o.slug = $3)
          AND (
              $4::text IS NULL
              OR o.name > $4::text
              OR (o.name = $4::text AND o.slug > $5::text)
          )
        ORDER BY o.name ASC, o.slug ASC
        LIMIT $6"#
        ),
    )
    .bind(user_filter)
    .bind(org_filter)
    .bind(&query.org)
    .bind(cursor_name)
    .bind(cursor_slug)
    .bind(fetch_limit(limit))
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let mut rows = rows;
    let has_more = truncate_to_limit(&mut rows, limit);
    let next_cursor = if has_more {
        rows.last()
            .map(|(name, slug, _, _, _)| {
                encode_cursor(&OrganizationAggregateCursor {
                    v: CURSOR_VERSION,
                    name: name.clone(),
                    slug: slug.clone(),
                })
            })
            .transpose()?
    } else {
        None
    };

    let result: Vec<Value> = rows
        .iter()
        .map(|(name, slug, commits, total, ai)| {
            let ai = ai.unwrap_or(0);
            let total = total.unwrap_or(0);
            let human = (total - ai).max(0);
            json!({
                "organization": name,
                "org_slug": slug,
                "total_commits": commits.unwrap_or(0),
                "w_total": total,
                "w_ai": ai,
                "w_human": human,
                "pct_ai": if total > 0 { (ai as f64 / total as f64) * 100.0 } else { 0.0 },
            })
        })
        .collect();

    Ok(Json(json!({
        "organizations": result,
        "pagination": pagination_meta(limit, has_more, next_cursor),
    })))
}

/// GET /api/v1/departments — Basic department list for the dashboard.
pub async fn list_departments(
    State(state): State<AppState>,
    auth: DashboardAuth,
) -> Result<Json<Value>, AppError> {
    let (_, org_filter) = build_data_filters(&auth.0);
    let restrict_department =
        auth.0.should_filter_department_scope() && auth.0.department_id.is_some();
    let department_filter = auth.0.department_id;

    let rows: Vec<(
        Uuid,
        String,
        String,
        String,
        Option<Uuid>,
        chrono::DateTime<chrono::Utc>,
        Uuid,
        String,
        String,
        Option<String>,
        i64,
        Option<i64>,
        Option<i64>,
    )> = sqlx::query_as(
        r#"SELECT
            d.id,
            d.code,
            d.name,
            d.slug,
            d.parent_id,
            d.created_at,
            o.id AS org_id,
            o.name AS org_name,
            o.slug AS org_slug,
            parent.name AS parent_name,
            COUNT(DISTINCT om.user_id)::bigint AS member_count,
            COALESCE(stats.total_lines, 0)::bigint AS total_lines,
            COALESCE(stats.ai_lines, 0)::bigint AS ai_lines
        FROM departments d
        JOIN organizations o ON o.id = d.org_id
        LEFT JOIN departments parent ON parent.id = d.parent_id AND parent.org_id = d.org_id
        LEFT JOIN org_members om ON om.org_id = d.org_id AND om.department_id = d.id
        LEFT JOIN (
            SELECT
                om.org_id,
                om.department_id,
                COALESCE(SUM(r.total_lines), 0)::bigint AS total_lines,
                COALESCE(SUM(r.ai_lines), 0)::bigint AS ai_lines
            FROM org_members om
            JOIN metrics_daily_rollups r ON r.org_id = om.org_id
              AND r.user_id = om.user_id
              AND r.tool_model = ''
            WHERE ($1::uuid IS NULL OR r.org_id = $1)
              AND ($2::boolean = FALSE OR om.department_id = $3::uuid)
            GROUP BY om.org_id, om.department_id
        ) stats ON stats.org_id = d.org_id AND stats.department_id = d.id
        WHERE ($1::uuid IS NULL OR d.org_id = $1)
          AND ($2::boolean = FALSE OR d.id = $3::uuid)
        GROUP BY d.id, d.code, d.name, d.slug, d.parent_id, d.created_at,
                 o.id, o.name, o.slug, parent.name, stats.total_lines, stats.ai_lines
        ORDER BY o.name ASC, d.name ASC, d.id ASC"#,
    )
    .bind(org_filter)
    .bind(restrict_department)
    .bind(department_filter)
    .fetch_all(&state.db)
    .await
    .map_err(AppError::Database)?;

    let departments: Vec<Value> = rows
        .iter()
        .map(
            |(
                id,
                code,
                name,
                slug,
                parent_id,
                created_at,
                org_id,
                org_name,
                org_slug,
                parent_name,
                member_count,
                total_lines,
                ai_lines,
            )| {
                let total_lines = total_lines.unwrap_or(0);
                let ai_lines = ai_lines.unwrap_or(0);
                json!({
                    "id": id.to_string(),
                    "code": code,
                    "name": name,
                    "slug": slug,
                    "parent_id": parent_id.map(|id| id.to_string()),
                    "parent_name": parent_name,
                    "created_at": created_at,
                    "org_id": org_id.to_string(),
                    "org_name": org_name,
                    "org_slug": org_slug,
                    "member_count": member_count,
                    "total_lines": total_lines,
                    "ai_lines": ai_lines,
                    "pct_ai_lines": if total_lines > 0 {
                        (ai_lines as f64 / total_lines as f64) * 100.0
                    } else {
                        0.0
                    },
                })
            },
        )
        .collect();

    Ok(Json(json!({ "departments": departments })))
}

/// GET /api/v1/aggregate/departments
pub async fn aggregate_departments(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<AggregateQuery>,
) -> Result<Json<Value>, AppError> {
    let (scope_user_filter, org_filter) = build_data_filters(&auth.0);
    let restrict_department = auth.0.should_filter_department_scope();
    let department_filter = auth.0.department_id;
    let user_filter = if restrict_department {
        None
    } else {
        scope_user_filter
    };
    let limit = clamp_limit(query.limit, DEFAULT_LIMIT, DASHBOARD_MAX_LIMIT);
    let cursor: Option<DepartmentAggregateCursor> =
        decode_dashboard_cursor(query.cursor.as_deref())?;
    let requested_parent_id = if restrict_department {
        None
    } else {
        query.parent_id
    };
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.parent_id != requested_parent_id)
    {
        return Err(AppError::BadRequest(
            "Department cursor does not match the requested parent".into(),
        ));
    }
    let cursor_org_name = cursor.as_ref().map(|cursor| cursor.org_name.clone());
    let cursor_sort_path = cursor.as_ref().map(|cursor| cursor.sort_path.clone());
    let cursor_department_id = cursor.as_ref().map(|cursor| cursor.department_id);
    let requested_parent_exists = if let Some(parent_id) = requested_parent_id {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(\
                SELECT 1 \
                FROM departments d \
                JOIN organizations o ON o.id = d.org_id \
                WHERE d.id = $1 \
                  AND ($2::text IS NULL OR o.slug = $2) \
                  AND ($3::uuid IS NULL OR o.id = $3)\
            )",
        )
        .bind(parent_id)
        .bind(&query.org)
        .bind(org_filter)
        .fetch_one(&state.db)
        .await
        .map_err(AppError::Database)?
    } else {
        true
    };
    if !requested_parent_exists {
        return Ok(Json(json!({
            "departments": [],
            "parent_exists": false,
            "pagination": pagination_meta(limit, false, None),
        })));
    }

    let metrics_department_rows = if state.config.dashboard_use_rollups {
        r#"SELECT
                    om.org_id,
                    om.department_id,
                    COALESCE(SUM(r.commits), 0)::bigint AS commits,
                    COALESCE(SUM(r.total_lines), 0)::bigint AS total_lines,
                    COALESCE(SUM(r.ai_lines), 0)::bigint AS ai_lines
                FROM org_members om
                JOIN metrics_daily_rollups r ON r.user_id = om.user_id
                  AND r.org_id = om.org_id
                  AND r.tool_model = ''
                WHERE om.department_id IS NOT NULL
                  AND ($1::uuid IS NULL OR r.user_id = $1)
                GROUP BY om.org_id, om.department_id"#
    } else {
        r#"SELECT
                    om.org_id,
                    om.department_id,
                    COUNT(m.id)::bigint AS commits,
                    COALESCE(SUM(m.git_diff_added_lines), 0)::bigint AS total_lines,
                    COALESCE(SUM(m.ai_additions), 0)::bigint AS ai_lines
                FROM org_members om
                JOIN metrics_events m ON m.user_id = om.user_id
                  AND m.org_id = om.org_id
                  AND m.event_type = 1
                WHERE om.department_id IS NOT NULL
                  AND ($1::uuid IS NULL OR m.user_id = $1)
                GROUP BY om.org_id, om.department_id"#
    };

    let rows: Vec<(
        Uuid,
        String,
        String,
        String,
        Option<Uuid>,
        String,
        i32,
        Vec<String>,
        bool,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    )> = sqlx::query_as(&format!(
        r#"WITH RECURSIVE department_tree AS MATERIALIZED (
            SELECT
                d.id,
                d.org_id,
                d.code,
                d.name,
                d.slug,
                d.parent_id,
                o.name AS org_name,
                1 AS depth,
                ARRAY[(CASE UPPER(LEFT(d.code, 1))
                    WHEN 'F' THEN '0'
                    WHEN 'A' THEN '1'
                    WHEN 'C' THEN '2'
                    WHEN 'S' THEN '3'
                    ELSE '4'
                END) || ' ' || d.code || ' [' || d.name || '] ' || d.id::text]::text[] AS sort_path,
                ARRAY[d.id]::uuid[] AS ancestor_ids
            FROM departments d
            JOIN organizations o ON d.org_id = o.id
            WHERE ($2::text IS NULL OR o.slug = $2)
              AND ($3::uuid IS NULL OR o.id = $3)
              AND d.parent_id IS NULL

            UNION ALL

            SELECT
                child.id,
                child.org_id,
                child.code,
                child.name,
                child.slug,
                child.parent_id,
                tree.org_name,
                tree.depth + 1,
                tree.sort_path || ((CASE UPPER(LEFT(child.code, 1))
                    WHEN 'F' THEN '0'
                    WHEN 'A' THEN '1'
                    WHEN 'C' THEN '2'
                    WHEN 'S' THEN '3'
                    ELSE '4'
                END) || ' ' || child.code || ' [' || child.name || '] ' || child.id::text),
                tree.ancestor_ids || child.id
            FROM department_tree tree
            JOIN departments child
              ON child.org_id = tree.org_id
             AND child.parent_id = tree.id
        ),
        department_page AS MATERIALIZED (
            SELECT tree.*
            FROM department_tree tree
            WHERE (
                ($4::boolean = TRUE AND tree.id = $5::uuid)
                OR (
                    $4::boolean = FALSE
                    AND tree.parent_id IS NOT DISTINCT FROM $10::uuid
                )
            )
              AND (
                  $6::text IS NULL
                  OR tree.org_name > $6::text
                  OR (tree.org_name = $6::text AND tree.sort_path > $7::text[])
                  OR (
                      tree.org_name = $6::text
                      AND tree.sort_path = $7::text[]
                      AND tree.id > $8::uuid
                  )
              )
            ORDER BY tree.org_name ASC, tree.sort_path ASC, tree.id ASC
            LIMIT $9
        ),
        department_direct_stats AS MATERIALIZED (
            SELECT
                org_id,
                department_id,
                SUM(commits)::bigint AS commits,
                SUM(total_lines)::bigint AS total_lines,
                SUM(ai_lines)::bigint AS ai_lines
            FROM (
                {metrics_department_rows}

                UNION ALL

                SELECT
                    p.org_id,
                    om.department_id,
                    COUNT(cs.sha)::bigint AS commits,
                    COALESCE(SUM(cs.git_diff_added_lines), 0)::bigint AS total_lines,
                    COALESCE(SUM(cs.ai_additions), 0)::bigint AS ai_lines
                FROM org_members om
                JOIN projects p ON p.user_id = om.user_id AND p.org_id = om.org_id
                JOIN commit_stats cs ON cs.project_id = p.id
                WHERE om.department_id IS NOT NULL
                  AND ($1::uuid IS NULL OR p.user_id = $1)
                  AND NOT EXISTS (
                      SELECT 1 FROM metrics_events m
                      WHERE m.event_type = 1
                        AND m.org_id = p.org_id
                        AND m.commit_sha = cs.sha
                        AND ($1::uuid IS NULL OR m.user_id = $1)
                  )
                GROUP BY p.org_id, om.department_id
            ) combined
            WHERE org_id IS NOT NULL AND department_id IS NOT NULL
            GROUP BY org_id, department_id
        ),
        department_stats AS MATERIALIZED (
            SELECT
                direct.org_id,
                ancestor_department.department_id,
                SUM(direct.commits)::bigint AS commits,
                SUM(direct.total_lines)::bigint AS total_lines,
                SUM(direct.ai_lines)::bigint AS ai_lines
            FROM department_direct_stats direct
            JOIN department_tree node
              ON node.org_id = direct.org_id
             AND node.id = direct.department_id
            CROSS JOIN LATERAL unnest(node.ancestor_ids)
                AS ancestor_department(department_id)
            GROUP BY direct.org_id, ancestor_department.department_id
        )
        SELECT
            page.id,
            page.code,
            page.name,
            page.slug,
            page.parent_id,
            page.org_name,
            page.depth,
            page.sort_path,
            EXISTS(
                SELECT 1
                FROM departments child
                WHERE child.org_id = page.org_id
                  AND child.parent_id = page.id
            ) AS has_children,
            COALESCE(stats.commits, 0),
            COALESCE(stats.total_lines, 0),
            COALESCE(stats.ai_lines, 0)
        FROM department_page page
        LEFT JOIN department_stats stats
          ON stats.org_id = page.org_id AND stats.department_id = page.id
        ORDER BY page.org_name ASC, page.sort_path ASC, page.id ASC"#
    ))
    .bind(user_filter)
    .bind(&query.org)
    .bind(org_filter)
    .bind(restrict_department)
    .bind(department_filter)
    .bind(cursor_org_name)
    .bind(cursor_sort_path)
    .bind(cursor_department_id)
    .bind(fetch_limit(limit))
    .bind(requested_parent_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let mut rows = rows;
    let has_more = truncate_to_limit(&mut rows, limit);
    let next_cursor = if has_more {
        rows.last()
            .map(|(id, _, _, _, _, org_name, _, sort_path, _, _, _, _)| {
                encode_cursor(&DepartmentAggregateCursor {
                    v: CURSOR_VERSION,
                    parent_id: requested_parent_id,
                    org_name: org_name.clone(),
                    sort_path: sort_path.clone(),
                    department_id: *id,
                })
            })
            .transpose()?
    } else {
        None
    };

    let result: Vec<Value> = rows
        .iter()
        .map(
            |(
                id,
                code,
                name,
                slug,
                parent_id,
                org_name,
                depth,
                _,
                has_children,
                commits,
                total,
                ai,
            )| {
                let ai = ai.unwrap_or(0);
                let total = total.unwrap_or(0);
                let human = (total - ai).max(0);
                json!({
                    "id": id.to_string(),
                    "code": code,
                    "department": name,
                    "dept_slug": slug,
                    "parent_id": parent_id.map(|id| id.to_string()),
                    "depth": depth,
                    "has_children": has_children,
                    "is_leaf": !has_children,
                    "organization": org_name,
                    "total_commits": commits.unwrap_or(0),
                    "w_total": total,
                    "w_ai": ai,
                    "w_human": human,
                    "pct_ai": if total > 0 { (ai as f64 / total as f64) * 100.0 } else { 0.0 },
                })
            },
        )
        .collect();

    Ok(Json(json!({
        "departments": result,
        "parent_exists": true,
        "pagination": pagination_meta(limit, has_more, next_cursor),
    })))
}

/// GET /api/v1/aggregate/projects
pub async fn aggregate_projects(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<AggregateQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);
    let (since_ts, until_ts) = parse_epoch_filters(query.since.as_deref(), query.until.as_deref())?;
    let limit = clamp_limit(query.limit, DEFAULT_LIMIT, DASHBOARD_MAX_LIMIT);
    let cursor: Option<ProjectAggregateCursor> = decode_dashboard_cursor(query.cursor.as_deref())?;
    let cursor_project_name = cursor.as_ref().map(|cursor| cursor.project_name.clone());
    let cursor_project_key = cursor.as_ref().map(|cursor| cursor.project_key.clone());

    let metrics_project_rows = if state.config.dashboard_use_rollups {
        r#"SELECT
                CASE
                    WHEN BTRIM(COALESCE(repo_url, '')) = '' THEN
                        'unassigned:missing-repo-url'
                    WHEN BTRIM(repo_url) ~ '^[^@/]+@[^:]+:.+' THEN
                        'sha256:' || encode(sha256(convert_to(
                            regexp_replace(
                                regexp_replace(
                                    'https://' || regexp_replace(BTRIM(repo_url), '^[^@/]+@([^:]+):/?(.+)$', '\1/\2'),
                                    '/+$',
                                    ''
                                ),
                                '\.git$',
                                ''
                            ),
                            'UTF8'
                        )), 'hex')
                    WHEN BTRIM(repo_url) LIKE '%://%' THEN
                        'sha256:' || encode(sha256(convert_to(
                            regexp_replace(
                                regexp_replace(
                                    regexp_replace(BTRIM(repo_url), '^[A-Za-z][A-Za-z0-9+.-]*://([^/@]+@)?([^/]+)(/.*)?$', 'https://\2\3'),
                                    '/+$',
                                    ''
                                ),
                                '\.git$',
                                ''
                            ),
                            'UTF8'
                        )), 'hex')
                    ELSE
                        'sha256:' || encode(sha256(convert_to(BTRIM(repo_url), 'UTF8')), 'hex')
                END AS project_key,
                NULL::bigint AS project_id,
                MIN(BTRIM(COALESCE(repo_url, ''))) AS repo_url,
                MIN(
                    CASE
                        WHEN BTRIM(COALESCE(repo_url, '')) = '' THEN '未关联项目'
                        ELSE NULLIF(
                            regexp_replace(
                                regexp_replace(regexp_replace(BTRIM(repo_url), '/+$', ''), '^.*/', ''),
                                '\.git$',
                                ''
                            ),
                            ''
                        )
                    END
                ) AS project_name,
                NULL::text AS branch,
                NULL::text AS organization,
                NULL::text AS department,
                COALESCE(SUM(commits), 0)::bigint AS total_commits,
                COALESCE(SUM(total_lines), 0)::bigint AS total_code,
                COALESCE(SUM(ai_lines), 0)::bigint AS total_ai
            FROM metrics_daily_rollups
            WHERE tool_model = ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
              AND ($4::bigint IS NULL OR day >= (to_timestamp($4) AT TIME ZONE 'UTC')::date)
              AND ($5::bigint IS NULL OR day <= (to_timestamp($5) AT TIME ZONE 'UTC')::date)
            GROUP BY project_key"#
    } else {
        r#"SELECT
                project_key,
                NULL::bigint AS project_id,
                MIN(repo_url) AS repo_url,
                MIN(project_name) AS project_name,
                STRING_AGG(DISTINCT branch, ', ' ORDER BY branch)
                    FILTER (WHERE branch IS NOT NULL AND branch != '') AS branch,
                NULL::text AS organization,
                NULL::text AS department,
                COUNT(*)::bigint AS total_commits,
                COALESCE(SUM(git_diff_added_lines), 0)::bigint AS total_code,
                COALESCE(SUM(ai_additions), 0)::bigint AS total_ai
            FROM (
                SELECT
                    CASE
                        WHEN BTRIM(COALESCE(repo_url, '')) = '' THEN
                            'unassigned:missing-repo-url'
                        WHEN BTRIM(repo_url) ~ '^[^@/]+@[^:]+:.+' THEN
                            'sha256:' || encode(sha256(convert_to(
                                regexp_replace(
                                    regexp_replace(
                                        'https://' || regexp_replace(BTRIM(repo_url), '^[^@/]+@([^:]+):/?(.+)$', '\1/\2'),
                                        '/+$',
                                        ''
                                    ),
                                    '\.git$',
                                    ''
                                ),
                                'UTF8'
                            )), 'hex')
                        WHEN BTRIM(repo_url) LIKE '%://%' THEN
                            'sha256:' || encode(sha256(convert_to(
                                regexp_replace(
                                    regexp_replace(
                                        regexp_replace(BTRIM(repo_url), '^[A-Za-z][A-Za-z0-9+.-]*://([^/@]+@)?([^/]+)(/.*)?$', 'https://\2\3'),
                                        '/+$',
                                        ''
                                    ),
                                    '\.git$',
                                    ''
                                ),
                                'UTF8'
                            )), 'hex')
                        ELSE
                            'sha256:' || encode(sha256(convert_to(BTRIM(repo_url), 'UTF8')), 'hex')
                    END AS project_key,
                    BTRIM(COALESCE(repo_url, '')) AS repo_url,
                    CASE
                        WHEN BTRIM(COALESCE(repo_url, '')) = '' THEN '未关联项目'
                        ELSE NULLIF(
                            regexp_replace(
                                regexp_replace(regexp_replace(BTRIM(repo_url), '/+$', ''), '^.*/', ''),
                                '\.git$',
                                ''
                            ),
                            ''
                        )
                    END AS project_name,
                    COALESCE(
                        NULLIF(BTRIM(raw_attrs->>'branch'), ''),
                        NULLIF(BTRIM(raw_attrs->>'5'), '')
                    ) AS branch,
                    git_diff_added_lines,
                    ai_additions
                FROM metrics_events
                WHERE event_type = 1
                  AND ($1::uuid IS NULL OR user_id = $1)
                  AND ($2::uuid IS NULL OR org_id = $2)
                  AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
                  AND ($4::bigint IS NULL OR timestamp >= $4)
                  AND ($5::bigint IS NULL OR timestamp <= $5)
            ) metrics_events_source
            GROUP BY project_key"#
    };

    let rows: Vec<(
        String,
        Option<i64>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    )> = sqlx::query_as(&format!(
        r#"WITH metric_project_rows AS (
            {metrics_project_rows}
        ),
        report_project_rows AS (
            SELECT
                p.remote_url_hash AS project_key,
                MIN(p.id)::bigint AS project_id,
                p.remote_url_hash AS repo_url,
                NULLIF(
                    regexp_replace(
                        regexp_replace(regexp_replace(p.remote_url_hash, '/+$', ''), '^.*/', ''),
                        '\.git$',
                        ''
                    ),
                    ''
                ) AS project_name,
                STRING_AGG(DISTINCT p.branch, ', ' ORDER BY p.branch)
                    FILTER (WHERE p.branch IS NOT NULL AND p.branch != '') AS branch,
                STRING_AGG(DISTINCT p.organization, ', ' ORDER BY p.organization)
                    FILTER (WHERE p.organization IS NOT NULL AND p.organization != '') AS organization,
                STRING_AGG(DISTINCT p.department, ', ' ORDER BY p.department)
                    FILTER (WHERE p.department IS NOT NULL AND p.department != '') AS department,
                COUNT(cs.sha)::bigint AS total_commits,
                COALESCE(SUM(cs.git_diff_added_lines), 0)::bigint AS total_code,
                COALESCE(SUM(cs.ai_additions), 0)::bigint AS total_ai
            FROM projects p
            JOIN LATERAL (
                SELECT cs.sha, cs.git_diff_added_lines, cs.ai_additions, cs.author_time_at
                FROM commit_stats cs
                WHERE cs.project_id = p.id
                  AND ($6::timestamptz IS NULL OR cs.author_time_at >= $6::timestamptz)
                  AND ($7::timestamptz IS NULL OR cs.author_time_at <= $7::timestamptz)
                  AND NOT EXISTS (
                      SELECT 1 FROM metrics_events m
                      WHERE m.event_type = 1
                        AND m.commit_sha = cs.sha
                        AND ($1::uuid IS NULL OR m.user_id = $1)
                        AND ($2::uuid IS NULL OR m.org_id = $2)
                  )
            ) cs ON TRUE
            WHERE ($1::uuid IS NULL OR p.user_id = $1)
              AND ($2::uuid IS NULL OR p.org_id = $2)
              AND ($3::text IS NULL OR p.org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY p.remote_url_hash
        ),
        project_sources AS (
            SELECT 'metrics'::text AS source, * FROM metric_project_rows
            UNION ALL
            SELECT 'report'::text AS source, * FROM report_project_rows
        ),
        project_rows AS (
            SELECT
                project_key,
                MIN(project_id) FILTER (WHERE project_id IS NOT NULL) AS project_id,
                COALESCE(
                    MIN(repo_url) FILTER (WHERE source = 'metrics'),
                    MIN(repo_url) FILTER (WHERE source = 'report'),
                    project_key
                ) AS repo_url,
                COALESCE(
                    MIN(project_name) FILTER (WHERE source = 'metrics'),
                    MIN(project_name) FILTER (WHERE source = 'report'),
                    project_key
                ) AS project_name,
                STRING_AGG(DISTINCT branch, ', ' ORDER BY branch)
                    FILTER (WHERE branch IS NOT NULL AND branch != '') AS branch,
                STRING_AGG(DISTINCT organization, ', ' ORDER BY organization)
                    FILTER (WHERE organization IS NOT NULL AND organization != '') AS organization,
                STRING_AGG(DISTINCT department, ', ' ORDER BY department)
                    FILTER (WHERE department IS NOT NULL AND department != '') AS department,
                SUM(total_commits)::bigint AS total_commits,
                SUM(total_code)::bigint AS total_code,
                SUM(total_ai)::bigint AS total_ai
            FROM project_sources
            GROUP BY project_key
        )
        SELECT
            project_key,
            project_id,
            repo_url,
            project_name,
            branch,
            organization,
            department,
            total_commits,
            total_code,
            total_ai
        FROM project_rows
        WHERE (
            $8::text IS NULL
            OR project_name > $8::text
            OR (project_name = $8::text AND project_key > $9::text)
        )
        ORDER BY project_name ASC, project_key ASC
        LIMIT $10"#
    ))
    .bind(user_filter)
    .bind(org_filter)
    .bind(&query.org)
    .bind(since_ts)
    .bind(until_ts)
    .bind(&query.since)
    .bind(&query.until)
    .bind(cursor_project_name)
    .bind(cursor_project_key)
    .bind(fetch_limit(limit))
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let mut rows = rows;
    let has_more = truncate_to_limit(&mut rows, limit);
    let next_cursor = if has_more {
        rows.last()
            .map(|(project_key, _, _, project_name, _, _, _, _, _, _)| {
                encode_cursor(&ProjectAggregateCursor {
                    v: CURSOR_VERSION,
                    project_name: project_name.clone(),
                    project_key: project_key.clone(),
                })
            })
            .transpose()?
    } else {
        None
    };

    let result: Vec<Value> = rows
        .iter()
        .map(
            |(
                _project_key,
                project_id,
                repo_url,
                project_name,
                branch,
                organization,
                department,
                commits,
                total,
                ai,
            )| {
                ProjectAggregate {
                    project_id: *project_id,
                    repo_url: repo_url.clone(),
                    project_name: project_name.clone(),
                    branch: branch.clone(),
                    organization: organization.clone(),
                    department: department.clone(),
                    total_commits: commits.unwrap_or(0),
                    total_code: total.unwrap_or(0),
                    total_ai: ai.unwrap_or(0),
                }
                .to_json()
            },
        )
        .collect();

    Ok(Json(json!({
        "projects": result,
        "pagination": pagination_meta(limit, has_more, next_cursor),
    })))
}

/// GET /api/v1/aggregate/developers
pub async fn aggregate_developers(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<AggregateQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);
    let (since_ts, until_ts) = parse_epoch_filters(query.since.as_deref(), query.until.as_deref())?;
    let limit = clamp_limit(query.limit, DEFAULT_LIMIT, DASHBOARD_MAX_LIMIT);
    let sort_by = query.sort_by.as_deref().unwrap_or("ai_lines");
    let sort_order = query.sort_order.as_deref().unwrap_or("desc");
    let sort_expression = match sort_by {
        "ai_lines" => "ai_added_lines::numeric",
        "ai_ratio" => {
            "CASE WHEN total_added_lines > 0 \
             THEN ai_added_lines::numeric / total_added_lines::numeric ELSE 0::numeric END"
        }
        _ => {
            return Err(AppError::BadRequest(
                "sort_by must be either ai_lines or ai_ratio".into(),
            ));
        }
    };
    let cursor_sort_expression = match sort_by {
        "ai_lines" => "$7::numeric",
        "ai_ratio" => {
            "CASE WHEN $8::bigint > 0 \
             THEN $7::numeric / $8::numeric ELSE 0::numeric END"
        }
        _ => unreachable!("sort_by was validated above"),
    };
    let (sort_comparison, sort_direction) = match sort_order {
        "desc" => ("<", "DESC"),
        "asc" => (">", "ASC"),
        _ => {
            return Err(AppError::BadRequest(
                "sort_order must be either asc or desc".into(),
            ));
        }
    };
    let cursor: Option<DeveloperAggregateCursor> =
        decode_dashboard_cursor(query.cursor.as_deref())?;
    if let Some(cursor) = &cursor {
        if cursor.sort_by != sort_by || cursor.sort_order != sort_order {
            return Err(AppError::BadRequest(
                "Developer pagination cursor does not match the selected sorting".into(),
            ));
        }
    }
    let cursor_ai_added_lines = cursor.as_ref().map(|cursor| cursor.ai_added_lines);
    let cursor_total_added_lines = cursor.as_ref().map(|cursor| cursor.total_added_lines);
    let cursor_total_commits = cursor.as_ref().map(|cursor| cursor.total_commits);
    let cursor_name = cursor.as_ref().map(|cursor| cursor.name.clone());
    let cursor_user_id = cursor.as_ref().map(|cursor| cursor.user_id);

    let metrics_developer_rows = if state.config.dashboard_use_rollups {
        r#"SELECT
                    NULLIF(user_id, '00000000-0000-0000-0000-000000000000'::uuid) AS user_id,
                    NULLIF(org_id, '00000000-0000-0000-0000-000000000000'::uuid) AS org_id,
                    COALESCE(SUM(commits), 0)::bigint AS commits,
                    COALESCE(SUM(total_lines), 0)::bigint AS added,
                    COALESCE(SUM(ai_lines), 0)::bigint AS ai,
                    COALESCE(SUM(human_lines), 0)::bigint AS human
                FROM metrics_daily_rollups
                WHERE tool_model = ''
                  AND user_id <> '00000000-0000-0000-0000-000000000000'::uuid
                  AND ($1::uuid IS NULL OR user_id = $1)
                  AND ($2::uuid IS NULL OR org_id = $2)
                  AND ($3::bigint IS NULL OR day >= (to_timestamp($3) AT TIME ZONE 'UTC')::date)
                  AND ($4::bigint IS NULL OR day <= (to_timestamp($4) AT TIME ZONE 'UTC')::date)
                GROUP BY user_id, org_id"#
    } else {
        r#"SELECT
                    user_id,
                    org_id,
                    COUNT(*) AS commits,
                    COALESCE(SUM(git_diff_added_lines), 0) AS added,
                    COALESCE(SUM(ai_additions), 0) AS ai,
                    COALESCE(SUM(GREATEST(COALESCE(git_diff_added_lines, 0) - COALESCE(ai_additions, 0), 0)), 0) AS human
                FROM metrics_events
                WHERE event_type = 1
                  AND user_id IS NOT NULL
                  AND ($1::uuid IS NULL OR user_id = $1)
                  AND ($2::uuid IS NULL OR org_id = $2)
                  AND ($3::bigint IS NULL OR timestamp >= $3)
                  AND ($4::bigint IS NULL OR timestamp <= $4)
                GROUP BY user_id, org_id"#
    };

    let rows: Vec<(
        Uuid,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Value,
    )> = sqlx::query_as(&format!(
        r#"WITH developer_stats AS (
            SELECT
                user_id,
                org_id,
                SUM(commits)::bigint AS total_commits,
                SUM(added)::bigint AS total_added_lines,
                SUM(ai)::bigint AS ai_added_lines,
                SUM(human)::bigint AS human_added_lines
            FROM (
                {metrics_developer_rows}

                UNION ALL

                SELECT
                    p.user_id,
                    p.org_id,
                    COUNT(*) AS commits,
                    COALESCE(SUM(cs.git_diff_added_lines), 0) AS added,
                    COALESCE(SUM(cs.ai_additions), 0) AS ai,
                    COALESCE(SUM(GREATEST(COALESCE(cs.git_diff_added_lines, 0) - COALESCE(cs.ai_additions, 0), 0)), 0) AS human
                FROM projects p
                JOIN LATERAL (
                    SELECT cs.sha, cs.git_diff_added_lines, cs.ai_additions
                    FROM commit_stats cs
                    WHERE cs.project_id = p.id
                      AND ($5::timestamptz IS NULL OR cs.author_time_at >= $5::timestamptz)
                      AND ($6::timestamptz IS NULL OR cs.author_time_at <= $6::timestamptz)
                      AND NOT EXISTS (
                          SELECT 1 FROM metrics_events m
                          WHERE m.event_type = 1
                            AND m.commit_sha = cs.sha
                            AND ($1::uuid IS NULL OR m.user_id = $1)
                            AND ($2::uuid IS NULL OR m.org_id = $2)
                      )
                ) cs ON TRUE
                WHERE p.user_id IS NOT NULL
                  AND ($1::uuid IS NULL OR p.user_id = $1)
                  AND ($2::uuid IS NULL OR p.org_id = $2)
                GROUP BY p.user_id, p.org_id
            ) combined
            GROUP BY user_id, org_id
        ),
        developer_rows AS (
            SELECT
                u.id,
                u.name,
                u.email,
                d.name AS department_name,
                COALESCE(ds.total_commits, 0)::bigint AS total_commits,
                COALESCE(ds.total_added_lines, 0)::bigint AS total_added_lines,
                COALESCE(ds.ai_added_lines, 0)::bigint AS ai_added_lines,
                COALESCE(ds.human_added_lines, 0)::bigint AS human_added_lines,
                ds.org_id
            FROM developer_stats ds
            JOIN users u ON u.id = ds.user_id
            LEFT JOIN org_members om ON om.user_id = u.id
              AND om.org_id = COALESCE(ds.org_id, u.default_org_id)
            LEFT JOIN departments d ON d.id = om.department_id AND d.org_id = om.org_id
        ),
        ranked_developers AS (
            SELECT *
            FROM developer_rows
            WHERE (
                $7::bigint IS NULL
                OR {sort_expression} {sort_comparison} {cursor_sort_expression}
                OR ({sort_expression} = {cursor_sort_expression} AND total_commits < $9::bigint)
                OR ({sort_expression} = {cursor_sort_expression} AND total_commits = $9::bigint AND name > $10::text)
                OR (
                    {sort_expression} = {cursor_sort_expression}
                    AND total_commits = $9::bigint
                    AND name = $10::text
                    AND id > $11::uuid
                )
            )
            ORDER BY {sort_expression} {sort_direction}, total_commits DESC, name ASC, id ASC
            LIMIT $12
        ),
        git_identity_candidates AS (
                SELECT
                    rd.id AS user_id,
                    rd.org_id,
                    TRIM(CASE
                        WHEN identity.author_email ~ '<[^>]+>' THEN split_part(identity.author_email, '<', 1)
                        WHEN identity.author_email LIKE '%@%' THEN ''
                        ELSE identity.author_email
                    END) AS git_name,
                    TRIM(CASE
                        WHEN identity.author_email ~ '<[^>]+>' THEN substring(identity.author_email from '<([^>]+)>')
                        WHEN identity.author_email LIKE '%@%' THEN identity.author_email
                        ELSE ''
                    END) AS git_email
                FROM ranked_developers rd
                JOIN LATERAL (
                    SELECT DISTINCT m.author_email
                    FROM metrics_events m
                    WHERE rd.org_id IS NOT NULL
                      AND m.event_type = 1
                      AND m.user_id = rd.id
                      AND m.org_id = rd.org_id
                      AND m.author_email IS NOT NULL
                      AND m.author_email != ''
                      AND ($3::bigint IS NULL OR m.timestamp >= $3)
                      AND ($4::bigint IS NULL OR m.timestamp <= $4)
                    ORDER BY m.author_email ASC
                    LIMIT 25
                ) identity ON TRUE

                UNION

                SELECT
                    rd.id AS user_id,
                    rd.org_id,
                    TRIM(CASE
                        WHEN identity.author_email ~ '<[^>]+>' THEN split_part(identity.author_email, '<', 1)
                        WHEN identity.author_email LIKE '%@%' THEN ''
                        ELSE identity.author_email
                    END) AS git_name,
                    TRIM(CASE
                        WHEN identity.author_email ~ '<[^>]+>' THEN substring(identity.author_email from '<([^>]+)>')
                        WHEN identity.author_email LIKE '%@%' THEN identity.author_email
                        ELSE ''
                    END) AS git_email
                FROM ranked_developers rd
                JOIN LATERAL (
                    SELECT DISTINCT m.author_email
                    FROM metrics_events m
                    WHERE rd.org_id IS NULL
                      AND m.event_type = 1
                      AND m.user_id = rd.id
                      AND m.org_id IS NULL
                      AND m.author_email IS NOT NULL
                      AND m.author_email != ''
                      AND ($3::bigint IS NULL OR m.timestamp >= $3)
                      AND ($4::bigint IS NULL OR m.timestamp <= $4)
                    ORDER BY m.author_email ASC
                    LIMIT 25
                ) identity ON TRUE

                UNION

                SELECT
                    rd.id AS user_id,
                    rd.org_id,
                    TRIM(CASE
                        WHEN identity.author ~ '<[^>]+>' THEN split_part(identity.author, '<', 1)
                        WHEN identity.author LIKE '%@%' THEN ''
                        ELSE identity.author
                    END) AS git_name,
                    TRIM(CASE
                        WHEN identity.author ~ '<[^>]+>' THEN substring(identity.author from '<([^>]+)>')
                        WHEN identity.author LIKE '%@%' THEN identity.author
                        ELSE ''
                END) AS git_email
                FROM ranked_developers rd
                JOIN LATERAL (
                    SELECT DISTINCT project_identity.author
                    FROM projects p
                    JOIN LATERAL (
                        SELECT DISTINCT cs.author
                        FROM commit_stats cs
                        WHERE cs.project_id = p.id
                          AND cs.author IS NOT NULL
                          AND cs.author != ''
                          AND ($5::timestamptz IS NULL OR cs.author_time_at >= $5::timestamptz)
                          AND ($6::timestamptz IS NULL OR cs.author_time_at <= $6::timestamptz)
                        ORDER BY cs.author ASC
                        LIMIT 25
                    ) project_identity ON TRUE
                    WHERE p.user_id = rd.id
                      AND (
                          (rd.org_id IS NULL AND p.org_id IS NULL)
                          OR p.org_id = rd.org_id
                      )
                    ORDER BY project_identity.author ASC
                    LIMIT 25
                ) identity ON TRUE
        ),
        git_identities AS (
            SELECT
                user_id,
                org_id,
                jsonb_agg(DISTINCT jsonb_build_object('name', git_name, 'email', git_email))
                    FILTER (WHERE git_name != '' OR git_email != '') AS identities
            FROM git_identity_candidates
            GROUP BY user_id, org_id
        )
        SELECT
            rd.id,
            rd.name,
            rd.email,
            rd.department_name,
            rd.total_commits,
            rd.total_added_lines,
            rd.ai_added_lines,
            rd.human_added_lines,
            COALESCE(gi.identities, '[]'::jsonb) AS git_identities
        FROM ranked_developers rd
        LEFT JOIN git_identities gi ON gi.user_id = rd.id
          AND gi.org_id IS NOT DISTINCT FROM rd.org_id
        ORDER BY {sort_expression} {sort_direction}, rd.total_commits DESC, rd.name ASC, rd.id ASC"#
    ))
    .bind(user_filter)
    .bind(org_filter)
    .bind(since_ts)
    .bind(until_ts)
    .bind(&query.since)
    .bind(&query.until)
    .bind(cursor_ai_added_lines)
    .bind(cursor_total_added_lines)
    .bind(cursor_total_commits)
    .bind(cursor_name)
    .bind(cursor_user_id)
    .bind(fetch_limit(limit))
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let mut rows = rows;
    let has_more = truncate_to_limit(&mut rows, limit);
    let next_cursor = if has_more {
        rows.last()
            .map(|(user_id, name, _, _, commits, total, ai, _, _)| {
                encode_cursor(&DeveloperAggregateCursor {
                    v: CURSOR_VERSION,
                    sort_by: sort_by.to_string(),
                    sort_order: sort_order.to_string(),
                    ai_added_lines: ai.unwrap_or(0),
                    total_added_lines: total.unwrap_or(0),
                    total_commits: commits.unwrap_or(0),
                    name: name.clone(),
                    user_id: *user_id,
                })
            })
            .transpose()?
    } else {
        None
    };

    let result: Vec<Value> = rows
        .iter()
        .map(
            |(user_id, name, email, department, commits, added, ai, human, git_identities)| {
                let ai = ai.unwrap_or(0);
                let total = added.unwrap_or(0);
                json!({
                    "id": user_id.to_string(),
                    "email": email,
                    "name": name,
                    "department": department,
                    "total_commits": commits.unwrap_or(0),
                    "total_added_lines": total,
                    "ai_added_lines": ai,
                    "human_added_lines": human.unwrap_or(0),
                    "pct_ai": if total > 0 { (ai as f64 / total as f64) * 100.0 } else { 0.0 },
                    "git_identities": git_identities,
                })
            },
        )
        .collect();

    Ok(Json(json!({
        "developers": result,
        "pagination": pagination_meta(limit, has_more, next_cursor),
    })))
}

/// GET /api/v1/aggregate/tools — Tool/Model breakdown statistics
pub async fn aggregate_tools(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<AggregateQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);
    let limit = clamp_limit(query.limit, DEFAULT_LIMIT, DASHBOARD_MAX_LIMIT);
    let cursor: Option<ToolAggregateCursor> = decode_dashboard_cursor(query.cursor.as_deref())?;
    let cursor_ai_additions = cursor.as_ref().map(|cursor| cursor.ai_additions);
    let cursor_tool_model = cursor.as_ref().map(|cursor| cursor.tool_model.clone());

    let metrics_source = if state.config.dashboard_use_rollups {
        r#"SELECT
                tool_model,
                COALESCE(SUM(ai_lines), 0)::bigint AS ai_additions,
                COALESCE(SUM(mixed_lines), 0)::bigint AS mixed_additions,
                COALESCE(SUM(ai_accepted), 0)::bigint AS ai_accepted,
                COALESCE(SUM(ai_lines), 0)::bigint AS total_ai_additions,
                0::bigint AS total_ai_deletions,
                FALSE AS has_report,
                TRUE AS has_metrics
            FROM metrics_daily_rollups
            WHERE tool_model != ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY tool_model"#
    } else {
        r#"SELECT
                tool_model,
                COALESCE(SUM(ai_additions), 0)::bigint AS ai_additions,
                COALESCE(SUM(mixed_additions), 0)::bigint AS mixed_additions,
                COALESCE(SUM(ai_accepted), 0)::bigint AS ai_accepted,
                COALESCE(SUM(total_ai_additions), 0)::bigint AS total_ai_additions,
                COALESCE(SUM(total_ai_deletions), 0)::bigint AS total_ai_deletions,
                FALSE AS has_report,
                TRUE AS has_metrics
            FROM metrics_tool_model_events
            WHERE ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY tool_model"#
    };

    let sql = format!(
        r#"WITH tool_sources AS (
            SELECT
                tms.tool_model,
                COALESCE(SUM(tms.ai_additions), 0)::bigint AS ai_additions,
                COALESCE(SUM(tms.mixed_additions), 0)::bigint AS mixed_additions,
                COALESCE(SUM(tms.ai_accepted), 0)::bigint AS ai_accepted,
                COALESCE(SUM(tms.total_ai_additions), 0)::bigint AS total_ai_additions,
                COALESCE(SUM(tms.total_ai_deletions), 0)::bigint AS total_ai_deletions,
                TRUE AS has_report,
                FALSE AS has_metrics
            FROM tool_model_stats tms
            JOIN projects p ON tms.project_id = p.id
            WHERE ($1::uuid IS NULL OR p.user_id = $1)
              AND ($2::uuid IS NULL OR p.org_id = $2)
              AND ($3::text IS NULL OR p.org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY tms.tool_model

            UNION ALL

            {metrics_source}

            UNION ALL

            SELECT
                CASE
                    WHEN COALESCE(model, '') = '' THEN COALESCE(tool, 'unknown')
                    ELSE CONCAT(COALESCE(tool, 'unknown'), '::', model)
                END AS tool_model,
                COALESCE(SUM(ai_additions), 0)::bigint AS ai_additions,
                0::bigint AS mixed_additions,
                0::bigint AS ai_accepted,
                COALESCE(SUM(ai_additions), 0)::bigint AS total_ai_additions,
                0::bigint AS total_ai_deletions,
                FALSE AS has_report,
                TRUE AS has_metrics
            FROM metrics_events
            WHERE event_type IN (2, 4) AND tool IS NOT NULL AND tool != ''
              AND ($1::uuid IS NULL OR user_id = $1)
              AND ($2::uuid IS NULL OR org_id = $2)
              AND ($3::text IS NULL OR org_id = (SELECT id FROM organizations WHERE slug = $3))
            GROUP BY tool, model
        ),
        tool_rows AS (
            SELECT
                tool_model,
                SUM(ai_additions)::bigint AS ai_additions,
                SUM(mixed_additions)::bigint AS mixed_additions,
                SUM(ai_accepted)::bigint AS ai_accepted,
                SUM(total_ai_additions)::bigint AS total_ai_additions,
                SUM(total_ai_deletions)::bigint AS total_ai_deletions,
                BOOL_OR(has_report) AS has_report,
                BOOL_OR(has_metrics) AS has_metrics
            FROM tool_sources
            GROUP BY tool_model
        )
        SELECT
            tool_model,
            CASE
                WHEN has_report AND has_metrics THEN 'report+metrics'
                WHEN has_report THEN 'report'
                ELSE 'metrics'
            END AS source,
            ai_additions,
            mixed_additions,
            ai_accepted,
            total_ai_additions,
            total_ai_deletions
        FROM tool_rows
        WHERE (
            $4::bigint IS NULL
            OR ai_additions < $4::bigint
            OR (ai_additions = $4::bigint AND tool_model > $5::text)
        )
        ORDER BY ai_additions DESC, tool_model ASC
        LIMIT $6"#
    );

    let rows: Vec<(String, String, i64, i64, i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(user_filter)
        .bind(org_filter)
        .bind(&query.org)
        .bind(cursor_ai_additions)
        .bind(cursor_tool_model)
        .bind(fetch_limit(limit))
        .fetch_all(&state.db)
        .await
        .map_err(|e| AppError::Database(e))?;

    let mut rows = rows;
    let has_more = truncate_to_limit(&mut rows, limit);
    let next_cursor = if has_more {
        rows.last()
            .map(|(tool_model, _, ai_additions, _, _, _, _)| {
                encode_cursor(&ToolAggregateCursor {
                    v: CURSOR_VERSION,
                    ai_additions: *ai_additions,
                    tool_model: tool_model.clone(),
                })
            })
            .transpose()?
    } else {
        None
    };

    let tools: Vec<Value> = rows
        .iter()
        .map(
            |(
                tool_model,
                source,
                ai_additions,
                mixed_additions,
                ai_accepted,
                total_ai_additions,
                total_ai_deletions,
            )| {
                json!({
                    "tool_model": tool_model,
                    "source": source,
                    "ai_additions": ai_additions,
                    "mixed_additions": mixed_additions,
                    "ai_accepted": ai_accepted,
                    "total_ai_additions": total_ai_additions,
                    "total_ai_deletions": total_ai_deletions,
                })
            },
        )
        .collect();

    Ok(Json(json!({
        "tools": tools,
        "pagination": pagination_meta(limit, has_more, next_cursor),
    })))
}

// ================================================================
// Phase 6: Advanced Dashboard Enhancement APIs
// ================================================================

#[derive(Debug, Deserialize)]
pub struct TrendsQuery {
    pub metric: Option<String>, // "ai_ratio", "ai_lines", "human_lines", "commits"
    pub granularity: Option<String>, // "day", "week", "month"
    pub org: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

/// GET /api/v1/aggregate/trends — AI code attribution trends over time
pub async fn aggregate_trends(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<TrendsQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);

    let metric = query.metric.as_deref().unwrap_or("ai_ratio");
    let granularity = query.granularity.as_deref().unwrap_or("week");

    let valid_metrics = ["ai_ratio", "ai_lines", "human_lines", "commits"];
    if !valid_metrics.contains(&metric) {
        return Err(AppError::BadRequest(format!(
            "metric must be one of: {}",
            valid_metrics.join(", ")
        )));
    }

    let valid_granularities = ["day", "week", "month"];
    if !valid_granularities.contains(&granularity) {
        return Err(AppError::BadRequest(format!(
            "granularity must be one of: {}",
            valid_granularities.join(", ")
        )));
    }

    let date_trunc = match granularity {
        "day" => "day",
        "week" => "week",
        "month" => "month",
        _ => "week",
    };

    let (since_ts, until_ts) =
        bounded_trend_epoch_filters(query.since.as_deref(), query.until.as_deref(), granularity)?;
    let effective_since = Some(epoch_seconds_to_rfc3339(since_ts)?);
    let effective_until = Some(epoch_seconds_to_rfc3339(until_ts)?);

    let rows = fetch_trend_rows(
        &state.db,
        state.config.dashboard_use_rollups,
        user_filter,
        org_filter,
        &query.org,
        Some(since_ts),
        Some(until_ts),
        &effective_since,
        &effective_until,
        date_trunc,
    )
    .await?;

    let data: Vec<Value> = rows
        .iter()
        .map(|(period, ai, human, commits)| {
            let ai = ai.unwrap_or(0);
            let human = human.unwrap_or(0);
            let total = ai + human;
            let ai_ratio = if total > 0 {
                (ai as f64 / total as f64) * 100.0
            } else {
                0.0
            };

            let value = match metric {
                "ai_ratio" => ai_ratio,
                "ai_lines" => ai as f64,
                "human_lines" => human as f64,
                "commits" => commits.unwrap_or(0) as f64,
                _ => 0.0,
            };

            json!({
                "period": period.to_string(),
                "granularity": granularity,
                "value": (value * 100.0).round() / 100.0,
                "ai_lines": ai,
                "human_lines": human,
                "commits": commits.unwrap_or(0),
                "ai_ratio": (ai_ratio * 100.0).round() / 100.0,
            })
        })
        .collect();

    Ok(Json(json!({
        "metric": metric,
        "granularity": granularity,
        "period": {
            "since": effective_since,
            "until": effective_until,
        },
        "data": data,
    })))
}

#[derive(Debug, Deserialize)]
pub struct AgentComparisonQuery {
    pub org: Option<String>,
}

/// GET /api/v1/aggregate/agent-comparison — Compare AI tools/models
pub async fn aggregate_agent_comparison(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<AgentComparisonQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);
    // From report data
    let report_rows: Vec<(
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    )> = sqlx::query_as(
        r#"SELECT
            tms.tool_model,
            COALESCE(SUM(tms.ai_additions), 0),
            COALESCE(SUM(tms.mixed_additions), 0),
            COALESCE(SUM(tms.ai_accepted), 0),
            COALESCE(SUM(tms.total_ai_additions), 0),
            COALESCE(SUM(tms.total_ai_deletions), 0)
        FROM tool_model_stats tms
        JOIN projects p ON tms.project_id = p.id
        WHERE ($1::uuid IS NULL OR p.user_id = $1)
          AND ($2::uuid IS NULL OR p.org_id = $2)
        GROUP BY tms.tool_model
        ORDER BY SUM(tms.ai_additions) DESC"#,
    )
    .bind(user_filter)
    .bind(org_filter)
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    // From committed metrics events or daily rollups.
    let metrics_rows = fetch_metrics_agent_rows(
        &state.db,
        state.config.dashboard_use_rollups,
        user_filter,
        org_filter,
        &query.org,
    )
    .await?;

    let mut comparisons_by_model: std::collections::BTreeMap<String, ToolAggregate> =
        std::collections::BTreeMap::new();

    // Report-based
    for (tool_model, ai_add, mixed_add, ai_accept, total_ai_add, total_ai_del) in &report_rows {
        comparisons_by_model
            .entry(tool_model.clone())
            .or_default()
            .add_report_row(
                ai_add.unwrap_or(0),
                mixed_add.unwrap_or(0),
                ai_accept.unwrap_or(0),
                total_ai_add.unwrap_or(0),
                total_ai_del.unwrap_or(0),
            );
    }

    // Metrics-based (supplementary)
    for (tool_model, ai_add, mixed_add, ai_accept, total_ai_add, total_ai_del, commits) in
        &metrics_rows
    {
        let aggregate = comparisons_by_model.entry(tool_model.clone()).or_default();
        aggregate.add_metrics_row(
            ai_add.unwrap_or(0),
            mixed_add.unwrap_or(0),
            ai_accept.unwrap_or(0),
            total_ai_add.unwrap_or(0),
            total_ai_del.unwrap_or(0),
        );
        aggregate.commits += commits.unwrap_or(0);
    }

    let mut comparisons: Vec<Value> = comparisons_by_model
        .into_iter()
        .map(|(tool_model, stats)| {
            let acceptance_rate = if stats.ai_additions > 0 {
                (stats.ai_accepted as f64 / stats.ai_additions as f64) * 100.0
            } else {
                0.0
            };
            json!({
                "tool_model": tool_model,
                "source": stats.source(),
                "ai_additions": stats.ai_additions,
                "mixed_additions": stats.mixed_additions,
                "ai_accepted": stats.ai_accepted,
                "total_ai_additions": stats.total_ai_additions,
                "total_ai_deletions": stats.total_ai_deletions,
                "net_ai_lines": stats.total_ai_additions - stats.total_ai_deletions,
                "commits": stats.commits,
                "acceptance_rate": (acceptance_rate * 100.0).round() / 100.0,
            })
        })
        .collect();

    // Sort by ai_additions descending
    comparisons.sort_by(|a, b| {
        let a_val = a.get("ai_additions").and_then(|v| v.as_i64()).unwrap_or(0);
        let b_val = b.get("ai_additions").and_then(|v| v.as_i64()).unwrap_or(0);
        b_val.cmp(&a_val)
    });

    Ok(Json(json!({ "comparisons": comparisons })))
}

#[derive(Debug, Deserialize)]
pub struct TeamComparisonQuery {
    pub org: Option<String>,
}

/// GET /api/v1/aggregate/team-comparison — Compare AI adoption across teams/departments
pub async fn aggregate_team_comparison(
    State(state): State<AppState>,
    auth: DashboardAuth,
    Query(query): Query<TeamComparisonQuery>,
) -> Result<Json<Value>, AppError> {
    let (user_filter, org_filter) = build_data_filters(&auth.0);

    let rows: Vec<(String, String, String, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> = sqlx::query_as(
        r#"SELECT
            d.name AS dept_name,
            d.slug AS dept_slug,
            o.name AS org_name,
            COUNT(m.id) AS total_commits,
            COALESCE(SUM(m.git_diff_added_lines), 0) AS total_lines,
            COALESCE(SUM(m.ai_additions), 0) AS ai_lines,
            COALESCE(SUM(GREATEST(COALESCE(m.git_diff_added_lines, 0) - COALESCE(m.ai_additions, 0), 0)), 0) AS human_lines
        FROM departments d
        JOIN organizations o ON d.org_id = o.id
        LEFT JOIN org_members om ON om.department_id = d.id AND om.org_id = d.org_id
        LEFT JOIN metrics_events m ON m.user_id = om.user_id AND m.org_id = om.org_id AND m.event_type = 1
          AND ($1::uuid IS NULL OR m.user_id = $1)
        WHERE ($2::text IS NULL OR o.slug = $2)
          AND ($3::uuid IS NULL OR o.id = $3)
        GROUP BY d.id, d.name, d.slug, o.name
        ORDER BY o.name, d.name"#
    )
    .bind(user_filter)
    .bind(&query.org)
    .bind(org_filter)
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e))?;

    let teams: Vec<Value> = rows.iter().map(|(dept_name, dept_slug, org_name, commits, total, ai, human)| {
        let ai = ai.unwrap_or(0);
        let human = human.unwrap_or(0);
        let total = total.unwrap_or(0);
        let pct_ai = if total > 0 { (ai as f64 / total as f64) * 100.0 } else { 0.0 };

        json!({
            "department": dept_name,
            "dept_slug": dept_slug,
            "organization": org_name,
            "total_commits": commits.unwrap_or(0),
            "total_lines": total,
            "ai_lines": ai,
            "human_lines": human,
            "pct_ai": (pct_ai * 100.0).round() / 100.0,
            "adoption_level": if pct_ai >= 60.0 { "high" } else if pct_ai >= 30.0 { "medium" } else { "low" },
        })
    }).collect();

    Ok(Json(json!({ "teams": teams })))
}

/// Build data filter parameters based on the user's role.
/// Returns (user_id_filter, org_id_filter):
/// - Admin users: (None, Some(org_id)) — sees all data within their organization
/// - Non-admin users: (Some(user_id), Some(org_id)) — sees only their own data within their organization
/// - If org_id is not available, falls back to no org filter (should not happen in practice)
pub fn build_data_filters(
    auth: &crate::models::user::AuthIdentity,
) -> (Option<uuid::Uuid>, Option<uuid::Uuid>) {
    if auth.is_admin() {
        // Admin sees all data within their organization (no user filter, but org filter applies)
        (None, auth.org_id)
    } else {
        // Non-admin sees only their own data within their organization
        (Some(auth.user_id), auth.org_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, MetricsRollupWriteMode};
    use crate::models::user::{AuthIdentity, AuthMethod};
    use crate::routes::response_has_compressible_content_type;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use tower_http::compression::predicate::{Predicate, SizeAbove};
    use tower_http::compression::{CompressionLayer, CompressionLevel};
    use uuid::Uuid;

    #[tokio::test]
    async fn dashboard_root_redirects_to_me() {
        let response = dashboard_root().await.into_response();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/me");
    }

    #[tokio::test]
    async fn dashboard_static_assets_revalidate_version_and_compress() {
        let app = Router::new()
            .route("/static/{*path}", get(dashboard_static_asset))
            .layer(
                CompressionLayer::new()
                    .gzip(true)
                    .br(true)
                    .quality(CompressionLevel::Precise(5))
                    .compress_when(SizeAbove::new(256).and(response_has_compressible_content_type)),
            );

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/dashboard.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers().get(header::CACHE_CONTROL).unwrap(),
            DASHBOARD_ASSET_REVALIDATE_CACHE_CONTROL
        );
        let etag = first.headers().get(header::ETAG).unwrap().clone();
        assert!(etag.to_str().unwrap().starts_with(r#"W/"sha256-"#));
        let raw_body = to_bytes(first.into_body(), usize::MAX).await.unwrap();
        let version = dashboard_asset_version(&raw_body);

        let not_modified = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/dashboard.js")
                    .header(header::IF_NONE_MATCH, etag.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers().get(header::ETAG).unwrap(), &etag);
        assert_eq!(
            not_modified.headers().get(header::VARY).unwrap(),
            "Accept-Encoding"
        );
        assert!(to_bytes(not_modified.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty());

        let versioned = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/static/dashboard.js?v={version}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            versioned.headers().get(header::CACHE_CONTROL).unwrap(),
            DASHBOARD_ASSET_IMMUTABLE_CACHE_CONTROL
        );

        let stale_version = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/dashboard.js?v=stale")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            stale_version.headers().get(header::CACHE_CONTROL).unwrap(),
            DASHBOARD_ASSET_REVALIDATE_CACHE_CONTROL
        );

        for asset_path in [
            "dashboard.css",
            "dashboard.js",
            "dashboard/api.js",
            "dashboard/pagination.js",
            "dashboard/refresh.js",
            "dashboard/render.js",
            "dashboard/router.js",
            "dashboard/state.js",
            "dashboard/ui/toast.js",
            "assets/vendor/chart.js/chart.umd.js",
        ] {
            let identity = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/static/{asset_path}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let identity_body = to_bytes(identity.into_body(), usize::MAX).await.unwrap();
            let mut compressed_sizes = Vec::new();
            for encoding in ["gzip", "br"] {
                let compressed = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(format!("/static/{asset_path}"))
                            .header(header::ACCEPT_ENCODING, encoding)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    compressed.headers().get(header::CONTENT_ENCODING).unwrap(),
                    encoding
                );
                assert!(compressed
                    .headers()
                    .get_all(header::VARY)
                    .iter()
                    .any(|value| value.as_bytes().eq_ignore_ascii_case(b"accept-encoding")));
                let body = to_bytes(compressed.into_body(), usize::MAX).await.unwrap();
                assert!(body.len() < identity_body.len());
                compressed_sizes.push(body.len());
            }
            println!(
                "{asset_path} transfer bytes: identity={}, gzip={}, br={}",
                identity_body.len(),
                compressed_sizes[0],
                compressed_sizes[1]
            );
        }

        let binary = app
            .oneshot(
                Request::builder()
                    .uri("/static/assets/vendor/chart.js/LICENSE.md")
                    .header(header::ACCEPT_ENCODING, "br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(binary.headers().get(header::CONTENT_ENCODING).is_none());
    }

    #[test]
    fn dashboard_cache_helpers_handle_html_and_weak_etags() {
        let html_path = std::path::Path::new("dashboard.html");
        assert_eq!(
            dashboard_asset_cache_control(html_path, Some("same"), "same"),
            DASHBOARD_HTML_CACHE_CONTROL
        );
        let response = with_dashboard_html_cache_control(StatusCode::OK.into_response());
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            DASHBOARD_HTML_CACHE_CONTROL
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static(r#""other", W/"sha256-current""#),
        );
        assert!(if_none_match_matches(&headers, r#"W/"sha256-current""#));
    }

    #[test]
    fn dashboard_template_uses_trusted_public_url_and_escapes_dynamic_text() {
        let mut auth = global_admin_auth(Uuid::new_v4()).0;
        auth.name = r#"<Admin "A" & 'B'>"#.into();
        auth.email = r#"admin+<test>&"@example.com"#.into();

        let html = render_dashboard_template(&auth).unwrap();
        let help = render_dashboard_help_template("https://git-ai.example.com/").unwrap();
        let static_dir = dashboard_static_dir().unwrap();
        let chart_version =
            dashboard_asset_file_version(&static_dir, "assets/vendor/chart.js/chart.umd.js")
                .unwrap();
        let css_version = dashboard_asset_file_version(&static_dir, "dashboard.css").unwrap();
        let js_version = dashboard_asset_file_version(&static_dir, "dashboard.js").unwrap();

        assert!(!html.contains("git-ai login --server https://git-ai.example.com"));
        assert!(html.contains(r#"id="help-content" aria-busy="true""#));
        assert!(help.contains("git-ai login --server https://git-ai.example.com"));
        assert!(help.contains(r#"href="/auth/register""#));
        assert!(html.contains(&format!(
            r#"src="/static/assets/vendor/chart.js/chart.umd.js?v={chart_version}""#
        )));
        assert!(html.contains(&format!(r#"href="/static/dashboard.css?v={css_version}""#)));
        assert!(html.contains(&format!(
            r#"type="module" src="/static/dashboard.js?v={js_version}""#
        )));
        assert!(html.contains(
            r#"<script type="application/json" id="dashboard-bootstrap">{"isAdmin":true}</script>"#
        ));
        assert!(html.contains(r#"<body class="dashboard-role-admin">"#));
        assert!(!html.contains("const isAdmin"));
        assert!(html.contains("&lt;Admin &quot;A&quot; &amp; &#39;B&#39;&gt;"));
        assert!(!html.contains("117.147.213.234"));
        assert!(!html.contains("cdn.jsdelivr.net"));
        assert!(!html.contains("__GITAI_"));
        assert!(!help.contains("__GITAI_"));
    }

    #[test]
    fn dashboard_bootstrap_json_cannot_terminate_its_script_element() {
        let hostile = json!({
            "value": "</script><script>alert('xss')</script>&\u{2028}\u{2029}"
        });

        let serialized = serialize_script_safe_json(&hostile).unwrap();

        assert!(!serialized.contains('<'));
        assert!(!serialized.contains('>'));
        assert!(!serialized.contains('&'));
        assert!(!serialized.contains('\u{2028}'));
        assert!(!serialized.contains('\u{2029}'));
        assert_eq!(serde_json::from_str::<Value>(&serialized).unwrap(), hostile);
    }

    #[test]
    fn dashboard_member_role_is_hidden_before_javascript_runs() {
        let auth = department_member_auth(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "engineering",
            Uuid::new_v4(),
        )
        .0;

        let html = render_dashboard_template(&auth).unwrap();

        assert!(html.contains(
            r#"<script type="application/json" id="dashboard-bootstrap">{"isAdmin":false}</script>"#
        ));
        assert!(html.contains(r#"<body class="dashboard-role-member">"#));
        assert!(html.contains(r#"class="nav-item admin-only" id="org-nav-item""#));
        assert!(!html.contains("const isAdmin"));
    }

    #[test]
    fn dashboard_template_renders_one_line_install_commands_for_http_url() {
        let html = render_dashboard_help_template("http://127.0.0.1:8080").unwrap();

        assert!(html.contains("一键安装"));
        assert!(html.contains(
            "curl -fsSL http://127.0.0.1:8080/worker/releases/latest/download/install.sh | bash"
        ));
        assert!(html.contains(
            "irm http://127.0.0.1:8080/worker/releases/latest/download/install.ps1 | iex"
        ));
    }

    #[test]
    fn parse_epoch_seconds_param_accepts_rfc3339() {
        let parsed = parse_epoch_seconds_param("since", Some("2024-01-01T00:00:00Z")).unwrap();

        assert_eq!(parsed, Some(1_704_067_200));
    }

    #[test]
    fn parse_epoch_seconds_param_accepts_date() {
        let parsed = parse_epoch_seconds_param("until", Some("2024-01-02")).unwrap();

        assert_eq!(parsed, Some(1_704_153_600));
    }

    #[test]
    fn parse_epoch_seconds_param_ignores_empty_values() {
        assert_eq!(parse_epoch_seconds_param("since", None).unwrap(), None);
        assert_eq!(
            parse_epoch_seconds_param("since", Some("  ")).unwrap(),
            None
        );
    }

    #[test]
    fn parse_epoch_seconds_param_rejects_invalid_values() {
        assert!(matches!(
            parse_epoch_seconds_param("since", Some("not-a-date")),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn bounded_trend_epoch_filters_defaults_missing_since_to_max_window() {
        let until = "2026-07-09T00:00:00Z";

        let (since_ts, until_ts) = bounded_trend_epoch_filters(None, Some(until), "day").unwrap();

        assert_eq!(
            epoch_seconds_to_date(since_ts).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2025, 7, 9).unwrap()
        );
        assert_eq!(
            epoch_seconds_to_date(until_ts).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()
        );
        assert_eq!(
            trend_bucket_count(since_ts, until_ts, "day").unwrap(),
            TREND_DAY_BUCKET_LIMIT
        );
    }

    #[test]
    fn bounded_trend_epoch_filters_rejects_too_many_day_buckets() {
        let error =
            bounded_trend_epoch_filters(Some("2025-01-01"), Some("2026-01-02"), "day").unwrap_err();

        assert!(matches!(error, AppError::BadRequest(_)));
    }

    #[test]
    fn tool_aggregate_merges_sources_and_fills_missing_totals() {
        let mut aggregate = ToolAggregate::default();

        aggregate.add_report_row(10, 2, 8, 12, 1);
        aggregate.add_metrics_row(5, 1, 4, 0, 0);

        assert_eq!(aggregate.source(), "report+metrics");
        assert_eq!(aggregate.ai_additions, 15);
        assert_eq!(aggregate.mixed_additions, 3);
        assert_eq!(aggregate.ai_accepted, 12);
        assert_eq!(aggregate.total_ai_additions, 17);
        assert_eq!(aggregate.total_ai_deletions, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn summary_rollup_matches_metrics_detail_path() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let (user_id, org_id) = insert_test_identity(&db.pool).await?;
        insert_dashboard_metrics_fixture(&db.pool, user_id, org_id).await?;

        let detail =
            fetch_metrics_summary_row(&db.pool, false, Some(user_id), Some(org_id), None, None)
                .await?;
        let rollup =
            fetch_metrics_summary_row(&db.pool, true, Some(user_id), Some(org_id), None, None)
                .await?;
        assert_eq!(detail, (Some(2), Some(70), Some(30), Some(40)));
        assert_eq!(rollup, detail);

        let detail_developers = fetch_total_developers(
            &db.pool,
            false,
            Some(user_id),
            Some(org_id),
            None,
            None,
            &None,
            &None,
        )
        .await?;
        let rollup_developers = fetch_total_developers(
            &db.pool,
            true,
            Some(user_id),
            Some(org_id),
            None,
            None,
            &None,
            &None,
        )
        .await?;
        assert_eq!(detail_developers, 1);
        assert_eq!(rollup_developers, detail_developers);

        let mut detail_projects =
            fetch_metrics_project_urls(&db.pool, false, Some(user_id), Some(org_id), None, None)
                .await?;
        let mut rollup_projects =
            fetch_metrics_project_urls(&db.pool, true, Some(user_id), Some(org_id), None, None)
                .await?;
        detail_projects.sort();
        rollup_projects.sort();
        assert_eq!(rollup_projects, detail_projects);

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_aggregates_include_unassigned_metrics() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let state = db.state()?;
        let (user_id, org_id) = insert_test_identity(&db.pool).await?;

        insert_dashboard_metric_row_with_tool(
            &db.pool,
            user_id,
            org_id,
            1_700_000_000,
            "https://example.com/git-ai.git",
            "assigned-commit",
            56,
            53,
            "codex::gpt-5",
            53,
            0,
            0,
        )
        .await?;
        insert_dashboard_metric_row_with_tool(
            &db.pool,
            user_id,
            org_id,
            1_700_000_100,
            "",
            "unassigned-commit",
            13,
            11,
            "codebuddy::unknown",
            11,
            0,
            0,
        )
        .await?;
        sqlx::query(
            "UPDATE metrics_events SET repo_url = NULL \
             WHERE user_id = $1 AND commit_sha = 'unassigned-commit'",
        )
        .bind(user_id)
        .execute(&db.pool)
        .await?;

        for use_rollups in [false, true] {
            let mut aggregate_state = state.clone();
            aggregate_state.config.dashboard_use_rollups = use_rollups;
            let auth = global_admin_auth(user_id);

            let Json(summary) = aggregate_summary(
                State(aggregate_state.clone()),
                auth.clone(),
                Query(aggregate_query(None, None, None, None)),
            )
            .await?;
            assert_eq!(summary["total_projects"].as_i64(), Some(1));
            assert_eq!(summary["total_commits"].as_i64(), Some(2));
            assert_eq!(summary["total_code_lines"].as_i64(), Some(69));
            assert_eq!(summary["total_ai_lines"].as_i64(), Some(64));
            assert_eq!(summary["total_human_lines"].as_i64(), Some(5));

            let Json(project_page) = aggregate_projects(
                State(aggregate_state),
                auth,
                Query(aggregate_query(None, None, Some(10), None)),
            )
            .await?;
            let projects = project_page["projects"]
                .as_array()
                .expect("projects should be an array");
            assert_eq!(projects.len(), 2);
            assert_eq!(
                projects
                    .iter()
                    .map(|project| project["total_code"].as_i64().unwrap_or_default())
                    .sum::<i64>(),
                69
            );
            assert_eq!(
                projects
                    .iter()
                    .map(|project| project["total_ai"].as_i64().unwrap_or_default())
                    .sum::<i64>(),
                64
            );
            assert_eq!(
                projects
                    .iter()
                    .map(|project| project["total_human"].as_i64().unwrap_or_default())
                    .sum::<i64>(),
                5
            );

            let unassigned = projects
                .iter()
                .find(|project| project["is_unassigned"] == true)
                .expect("unassigned project aggregate should be present");
            assert_eq!(unassigned["project_name"].as_str(), Some("未关联项目"));
            assert_eq!(unassigned["repo_url"].as_str(), Some(""));
            assert_eq!(unassigned["total_commits"].as_i64(), Some(1));
            assert_eq!(unassigned["total_code"].as_i64(), Some(13));
            assert_eq!(unassigned["total_ai"].as_i64(), Some(11));
            assert_eq!(unassigned["total_human"].as_i64(), Some(2));
        }

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trend_and_tool_rollups_match_metrics_detail_path() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let (user_id, org_id) = insert_test_identity(&db.pool).await?;
        insert_dashboard_metrics_fixture(&db.pool, user_id, org_id).await?;

        let detail_trends = fetch_trend_rows(
            &db.pool,
            false,
            Some(user_id),
            Some(org_id),
            &None,
            None,
            None,
            &None,
            &None,
            "day",
        )
        .await?;
        let rollup_trends = fetch_trend_rows(
            &db.pool,
            true,
            Some(user_id),
            Some(org_id),
            &None,
            None,
            None,
            &None,
            &None,
            "day",
        )
        .await?;
        assert_eq!(rollup_trends, detail_trends);

        let detail_tools =
            fetch_metrics_tool_rows(&db.pool, false, Some(user_id), Some(org_id)).await?;
        let rollup_tools =
            fetch_metrics_tool_rows(&db.pool, true, Some(user_id), Some(org_id)).await?;
        assert_eq!(detail_tools, rollup_tools);
        assert_eq!(rollup_tools[0].0, "codex::gpt-5");
        assert_eq!(rollup_tools[0].1, Some(9));
        assert_eq!(rollup_tools[0].2, Some(3));
        assert_eq!(rollup_tools[0].3, Some(5));

        let detail_agents =
            fetch_metrics_agent_rows(&db.pool, false, Some(user_id), Some(org_id), &None).await?;
        let rollup_agents =
            fetch_metrics_agent_rows(&db.pool, true, Some(user_id), Some(org_id), &None).await?;
        assert_eq!(rollup_agents, detail_agents);
        assert_eq!(rollup_agents[0].6, Some(2));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn organization_and_department_aggregates_cursor_paginate() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let state = db.state()?;
        let auth = global_admin_auth(Uuid::new_v4());
        let alpha_org_name = "000 Alpha Org";
        let beta_org_name = "000 Beta Org";
        let gamma_org_name = "000 Gamma Org";
        let alpha_dept_name = "000 Alpha Dept";
        let beta_dept_name = "000 Beta Dept";
        let gamma_dept_name = "000 Gamma Dept";
        let (alpha_org_id, _) = insert_organization(&db.pool, alpha_org_name).await?;
        let (beta_org_id, _) = insert_organization(&db.pool, beta_org_name).await?;
        insert_organization(&db.pool, gamma_org_name).await?;
        insert_department(&db.pool, alpha_org_id, alpha_dept_name).await?;
        insert_department(&db.pool, alpha_org_id, beta_dept_name).await?;
        insert_department(&db.pool, beta_org_id, gamma_dept_name).await?;

        let Json(first_org_page) = aggregate_organizations(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(None, None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            string_values(&first_org_page, "organizations", "organization"),
            vec![alpha_org_name, beta_org_name]
        );
        assert_eq!(
            first_org_page["pagination"]["has_more"].as_bool(),
            Some(true)
        );

        let Json(second_org_page) = aggregate_organizations(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(
                None,
                None,
                Some(2),
                Some(required_next_cursor(&first_org_page)),
            )),
        )
        .await?;
        let second_org_names = string_values(&second_org_page, "organizations", "organization");
        assert_eq!(
            second_org_names.first().map(String::as_str),
            Some(gamma_org_name)
        );
        assert!(!second_org_names.contains(&alpha_org_name.to_string()));
        assert!(!second_org_names.contains(&beta_org_name.to_string()));

        let Json(first_department_page) = aggregate_departments(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(None, None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            pair_values(
                &first_department_page,
                "departments",
                "organization",
                "department"
            ),
            vec![
                (alpha_org_name.to_string(), alpha_dept_name.to_string()),
                (alpha_org_name.to_string(), beta_dept_name.to_string())
            ]
        );

        let Json(second_department_page) = aggregate_departments(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(
                None,
                None,
                Some(2),
                Some(required_next_cursor(&first_department_page)),
            )),
        )
        .await?;
        let second_department_pairs = pair_values(
            &second_department_page,
            "departments",
            "organization",
            "department",
        );
        assert_eq!(
            second_department_pairs
                .first()
                .map(|(organization, department)| (organization.as_str(), department.as_str())),
            Some((beta_org_name, gamma_dept_name))
        );
        assert!(!second_department_pairs
            .contains(&(alpha_org_name.to_string(), alpha_dept_name.to_string())));
        assert!(!second_department_pairs
            .contains(&(alpha_org_name.to_string(), beta_dept_name.to_string())));

        let mut rollup_state = state;
        rollup_state.config.dashboard_use_rollups = true;
        let Json(first_rollup_department_page) = aggregate_departments(
            State(rollup_state.clone()),
            auth.clone(),
            Query(aggregate_query(None, None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            pair_values(
                &first_rollup_department_page,
                "departments",
                "organization",
                "department"
            ),
            vec![
                (alpha_org_name.to_string(), alpha_dept_name.to_string()),
                (alpha_org_name.to_string(), beta_dept_name.to_string())
            ]
        );
        let Json(second_rollup_department_page) = aggregate_departments(
            State(rollup_state),
            auth,
            Query(aggregate_query(
                None,
                None,
                Some(2),
                Some(required_next_cursor(&first_rollup_department_page)),
            )),
        )
        .await?;
        assert_eq!(
            pair_values(
                &second_rollup_department_page,
                "departments",
                "organization",
                "department"
            )
            .first()
            .map(|(organization, department)| (organization.as_str(), department.as_str())),
            Some((beta_org_name, gamma_dept_name))
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn department_aggregates_include_all_descendants() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let state = db.state()?;
        let (org_id, org_slug) = insert_organization(&db.pool, "Hierarchy Stats Org").await?;
        let root_id = insert_department(&db.pool, org_id, "Root").await?;
        let child_id =
            insert_department_with_parent(&db.pool, org_id, "Child", Some(root_id)).await?;
        let leaf_id =
            insert_department_with_parent(&db.pool, org_id, "Leaf", Some(child_id)).await?;
        let sibling_id =
            insert_department_with_parent(&db.pool, org_id, "Sibling", Some(root_id)).await?;
        sqlx::query(
            "UPDATE departments \
             SET code = CASE WHEN id = $1 THEN 'F00020' ELSE 'F00010' END \
             WHERE id IN ($1, $2)",
        )
        .bind(child_id)
        .bind(sibling_id)
        .execute(&db.pool)
        .await?;

        let admin_id = insert_user(&db.pool, org_id, "Admin", "admin", None).await?;
        let root_user = insert_user(&db.pool, org_id, "Root User", "member", Some(root_id)).await?;
        let child_user =
            insert_user(&db.pool, org_id, "Child User", "member", Some(child_id)).await?;
        let child_peer =
            insert_user(&db.pool, org_id, "Child Peer", "member", Some(child_id)).await?;
        let leaf_user = insert_user(&db.pool, org_id, "Leaf User", "member", Some(leaf_id)).await?;
        let sibling_user =
            insert_user(&db.pool, org_id, "Sibling User", "member", Some(sibling_id)).await?;

        for (index, user_id, total, ai) in [
            (1, root_user, 10, 2),
            (2, child_user, 20, 5),
            (3, leaf_user, 30, 10),
            (4, sibling_user, 40, 20),
            (5, child_peer, 12, 6),
        ] {
            insert_dashboard_metric_row_with_tool(
                &db.pool,
                user_id,
                org_id,
                1_700_000_000 + index,
                &format!("https://example.com/hierarchy-{index}.git"),
                &format!("hierarchy-{index}"),
                total,
                ai,
                "codex::gpt-5",
                ai,
                0,
                0,
            )
            .await?;
        }

        let auth = org_admin_auth(admin_id, org_id, &org_slug);
        for use_rollups in [false, true] {
            let mut aggregate_state = state.clone();
            aggregate_state.config.dashboard_use_rollups = use_rollups;
            let Json(root_page) = aggregate_departments(
                State(aggregate_state.clone()),
                auth.clone(),
                Query(aggregate_query(Some(org_slug.clone()), None, Some(1), None)),
            )
            .await?;
            let root = &root_page["departments"][0];
            assert_eq!(root["department"].as_str(), Some("Root"));
            assert_eq!(root["total_commits"].as_i64(), Some(5));
            assert_eq!(root["w_total"].as_i64(), Some(112));
            assert_eq!(root["w_ai"].as_i64(), Some(43));
            assert_eq!(root["pct_ai"].as_f64(), Some(43.0 / 112.0 * 100.0));
            assert_eq!(root_page["pagination"]["has_more"].as_bool(), Some(false));

            let mut children_query = aggregate_query(Some(org_slug.clone()), None, Some(10), None);
            children_query.parent_id = Some(root_id);
            let Json(children_page) = aggregate_departments(
                State(aggregate_state.clone()),
                auth.clone(),
                Query(children_query),
            )
            .await?;
            let children = children_page["departments"]
                .as_array()
                .expect("departments should be an array");
            assert_eq!(
                children
                    .iter()
                    .map(|department| department["department"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["Sibling", "Child"]
            );
            assert_eq!(
                children
                    .iter()
                    .map(|department| department["depth"].as_i64().unwrap())
                    .collect::<Vec<_>>(),
                vec![2, 2]
            );

            let mut leaf_query = aggregate_query(Some(org_slug.clone()), None, Some(10), None);
            leaf_query.parent_id = Some(child_id);
            let Json(leaf_page) = aggregate_departments(
                State(aggregate_state.clone()),
                auth.clone(),
                Query(leaf_query),
            )
            .await?;
            assert_eq!(
                string_values(&leaf_page, "departments", "department"),
                vec!["Leaf"]
            );

            let departments = std::iter::once(root.clone())
                .chain(children.iter().cloned())
                .chain(
                    leaf_page["departments"]
                        .as_array()
                        .expect("leaf departments should be an array")
                        .iter()
                        .cloned(),
                )
                .collect::<Vec<_>>();
            let expected = [
                ("Root", 5, 112, 43, 43.0 / 112.0 * 100.0, false),
                ("Child", 3, 62, 21, 21.0 / 62.0 * 100.0, false),
                ("Leaf", 1, 30, 10, 100.0 / 3.0, true),
                ("Sibling", 1, 40, 20, 50.0, true),
            ];
            for (name, commits, total, ai, pct_ai, is_leaf) in expected {
                let department = departments
                    .iter()
                    .find(|department| department["department"] == name)
                    .expect("expected department should exist");
                assert_eq!(department["total_commits"].as_i64(), Some(commits));
                assert_eq!(department["w_total"].as_i64(), Some(total));
                assert_eq!(department["w_ai"].as_i64(), Some(ai));
                let actual_pct_ai = department["pct_ai"]
                    .as_f64()
                    .expect("department AI percentage should be numeric");
                assert!((actual_pct_ai - pct_ai).abs() < 1e-12);
                assert_eq!(department["is_leaf"].as_bool(), Some(is_leaf));
            }

            let mut first_child_query =
                aggregate_query(Some(org_slug.clone()), None, Some(1), None);
            first_child_query.parent_id = Some(root_id);
            let Json(first_child_page) = aggregate_departments(
                State(aggregate_state.clone()),
                auth.clone(),
                Query(first_child_query),
            )
            .await?;
            let mut mismatched_cursor_query = aggregate_query(
                Some(org_slug.clone()),
                None,
                Some(1),
                Some(required_next_cursor(&first_child_page)),
            );
            mismatched_cursor_query.parent_id = Some(child_id);
            let cursor_error = aggregate_departments(
                State(aggregate_state.clone()),
                auth.clone(),
                Query(mismatched_cursor_query),
            )
            .await
            .expect_err("a department cursor must not be reused for another parent");
            assert!(matches!(cursor_error, AppError::BadRequest(_)));

            let mut missing_parent_query =
                aggregate_query(Some(org_slug.clone()), None, Some(10), None);
            missing_parent_query.parent_id = Some(Uuid::new_v4());
            let Json(missing_parent_page) = aggregate_departments(
                State(aggregate_state.clone()),
                auth.clone(),
                Query(missing_parent_query),
            )
            .await?;
            assert_eq!(missing_parent_page["parent_exists"].as_bool(), Some(false));
            assert!(missing_parent_page["departments"]
                .as_array()
                .expect("missing parent departments should be an array")
                .is_empty());

            let mut developer_query = aggregate_query(Some(org_slug.clone()), None, Some(10), None);
            developer_query.parent_id = Some(sibling_id);
            let Json(developer_page) = aggregate_departments(
                State(aggregate_state),
                department_member_auth(child_user, org_id, &org_slug, child_id),
                Query(developer_query),
            )
            .await?;
            let developer_departments = developer_page["departments"]
                .as_array()
                .expect("developer departments should be an array");
            assert_eq!(developer_departments.len(), 1);
            let own_department = &developer_departments[0];
            assert_eq!(
                own_department["id"].as_str(),
                Some(child_id.to_string().as_str())
            );
            assert_eq!(own_department["department"].as_str(), Some("Child"));
            assert_eq!(own_department["total_commits"].as_i64(), Some(3));
            assert_eq!(own_department["w_total"].as_i64(), Some(62));
            assert_eq!(own_department["w_ai"].as_i64(), Some(21));
        }

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn department_aggregates_follow_code_prefix_order() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let state = db.state()?;
        let (org_id, org_slug) = insert_organization(&db.pool, "Code Order Org").await?;

        for (name, code) in [
            ("Other Department", "Z00001"),
            ("S Department", "S00001"),
            ("C Department", "C00001"),
            ("A Department", "A00001"),
            ("F Department", "F00001"),
        ] {
            let department_id = insert_department(&db.pool, org_id, name).await?;
            sqlx::query("UPDATE departments SET code = $1 WHERE id = $2")
                .bind(code)
                .bind(department_id)
                .execute(&db.pool)
                .await?;
        }

        let admin_id = insert_user(&db.pool, org_id, "Admin", "admin", None).await?;
        let Json(page) = aggregate_departments(
            State(state),
            org_admin_auth(admin_id, org_id, &org_slug),
            Query(aggregate_query(Some(org_slug), None, Some(10), None)),
        )
        .await?;

        assert_eq!(
            string_values(&page, "departments", "department"),
            vec![
                "F Department",
                "A Department",
                "C Department",
                "S Department",
                "Other Department",
            ]
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "manual isolated database scale benchmark"]
    async fn department_aggregate_scale_benchmark() -> anyhow::Result<()> {
        const PAGE_SIZE: i64 = 25;
        const WARM_RUNS: usize = 7;

        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let mut state = db.state()?;
        state.config.dashboard_use_rollups = true;

        for department_count in [100_i32, 1_000, 10_000] {
            let (org_id, org_slug) = insert_organization(
                &db.pool,
                &format!("Department Benchmark {department_count}"),
            )
            .await?;
            insert_department_benchmark_fixture(
                &db.pool,
                org_id,
                &format!("benchmark-{}", org_id.simple()),
                department_count,
            )
            .await?;
            let admin_id = insert_user(&db.pool, org_id, "Admin", "admin", None).await?;
            let auth = org_admin_auth(admin_id, org_id, &org_slug);
            sqlx::query("ANALYZE departments").execute(&db.pool).await?;

            let first_started = std::time::Instant::now();
            let Json(first_page) = aggregate_departments(
                State(state.clone()),
                auth.clone(),
                Query(aggregate_query(
                    Some(org_slug.clone()),
                    None,
                    Some(PAGE_SIZE),
                    None,
                )),
            )
            .await?;
            let first_ms = first_started.elapsed().as_secs_f64() * 1_000.0;
            let returned = first_page["departments"]
                .as_array()
                .expect("departments should be an array")
                .len();
            let has_more = first_page["pagination"]["has_more"]
                .as_bool()
                .expect("has_more should be a boolean");
            assert_eq!(returned, PAGE_SIZE as usize);
            assert!(has_more);

            let mut warm_durations_ms = Vec::with_capacity(WARM_RUNS);
            for _ in 0..WARM_RUNS {
                let started = std::time::Instant::now();
                let Json(page) = aggregate_departments(
                    State(state.clone()),
                    auth.clone(),
                    Query(aggregate_query(
                        Some(org_slug.clone()),
                        None,
                        Some(PAGE_SIZE),
                        None,
                    )),
                )
                .await?;
                warm_durations_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
                assert_eq!(
                    page["departments"]
                        .as_array()
                        .expect("departments should be an array")
                        .len(),
                    PAGE_SIZE as usize
                );
            }
            warm_durations_ms.sort_by(f64::total_cmp);
            let median_ms = warm_durations_ms[WARM_RUNS / 2];
            let p95_ms = *warm_durations_ms
                .last()
                .expect("the benchmark should contain warm runs");

            eprintln!(
                "DEPARTMENT_BENCHMARK departments={department_count} page_size={PAGE_SIZE} \
                 initial_api_requests=1 returned={returned} has_more={has_more} \
                 first_ms={first_ms:.2} median_ms={median_ms:.2} p95_ms={p95_ms:.2}"
            );
        }

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dashboard_stat_aggregates_cursor_paginate() -> anyhow::Result<()> {
        let Some(db) = TestDatabase::new().await? else {
            return Ok(());
        };
        let state = db.state()?;
        let (org_id, org_slug) = insert_organization(&db.pool, "Stats Org").await?;
        let admin_id = insert_user(&db.pool, org_id, "Admin User", "admin", None).await?;
        let alice_id = insert_user(&db.pool, org_id, "Alice", "member", None).await?;
        let bob_id = insert_user(&db.pool, org_id, "Bob", "member", None).await?;
        let carol_id = insert_user(&db.pool, org_id, "Carol", "member", None).await?;
        let auth = org_admin_auth(admin_id, org_id, &org_slug);

        insert_dashboard_metric_row_with_tool(
            &db.pool,
            alice_id,
            org_id,
            1_700_000_000,
            "https://example.com/alpha.git",
            "alpha-1",
            80,
            60,
            "codex::gpt-5",
            60,
            0,
            0,
        )
        .await?;
        insert_dashboard_metric_row_with_tool(
            &db.pool,
            bob_id,
            org_id,
            1_700_000_100,
            "https://example.com/beta.git",
            "beta-1",
            50,
            25,
            "qoder::unknown",
            25,
            0,
            0,
        )
        .await?;
        insert_dashboard_metric_row_with_tool(
            &db.pool,
            bob_id,
            org_id,
            1_700_000_200,
            "https://example.com/beta.git",
            "beta-2",
            50,
            25,
            "qoder::unknown",
            25,
            0,
            0,
        )
        .await?;
        insert_dashboard_metric_row_with_tool(
            &db.pool,
            carol_id,
            org_id,
            1_700_000_300,
            "https://example.com/gamma.git",
            "gamma-1",
            70,
            40,
            "trae::unknown",
            40,
            0,
            0,
        )
        .await?;

        let Json(first_developer_page) = aggregate_developers(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(Some(org_slug.clone()), None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            string_values(&first_developer_page, "developers", "name"),
            vec!["Alice", "Bob"]
        );
        let Json(second_developer_page) = aggregate_developers(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(
                Some(org_slug.clone()),
                None,
                Some(2),
                Some(required_next_cursor(&first_developer_page)),
            )),
        )
        .await?;
        assert_eq!(
            string_values(&second_developer_page, "developers", "name"),
            vec!["Carol"]
        );

        let mut ratio_desc_query = aggregate_query(Some(org_slug.clone()), None, Some(2), None);
        ratio_desc_query.sort_by = Some("ai_ratio".into());
        ratio_desc_query.sort_order = Some("desc".into());
        let Json(first_ratio_desc_page) =
            aggregate_developers(State(state.clone()), auth.clone(), Query(ratio_desc_query))
                .await?;
        assert_eq!(
            string_values(&first_ratio_desc_page, "developers", "name"),
            vec!["Alice", "Carol"]
        );
        let mut second_ratio_desc_query = aggregate_query(
            Some(org_slug.clone()),
            None,
            Some(2),
            Some(required_next_cursor(&first_ratio_desc_page)),
        );
        second_ratio_desc_query.sort_by = Some("ai_ratio".into());
        second_ratio_desc_query.sort_order = Some("desc".into());
        let Json(second_ratio_desc_page) = aggregate_developers(
            State(state.clone()),
            auth.clone(),
            Query(second_ratio_desc_query),
        )
        .await?;
        assert_eq!(
            string_values(&second_ratio_desc_page, "developers", "name"),
            vec!["Bob"]
        );

        let mut ai_lines_asc_query = aggregate_query(Some(org_slug.clone()), None, Some(3), None);
        ai_lines_asc_query.sort_by = Some("ai_lines".into());
        ai_lines_asc_query.sort_order = Some("asc".into());
        let Json(ai_lines_asc_page) = aggregate_developers(
            State(state.clone()),
            auth.clone(),
            Query(ai_lines_asc_query),
        )
        .await?;
        assert_eq!(
            string_values(&ai_lines_asc_page, "developers", "name"),
            vec!["Carol", "Bob", "Alice"]
        );

        let mut ratio_asc_query = aggregate_query(Some(org_slug.clone()), None, Some(3), None);
        ratio_asc_query.sort_by = Some("ai_ratio".into());
        ratio_asc_query.sort_order = Some("asc".into());
        let Json(ratio_asc_page) =
            aggregate_developers(State(state.clone()), auth.clone(), Query(ratio_asc_query))
                .await?;
        assert_eq!(
            string_values(&ratio_asc_page, "developers", "name"),
            vec!["Bob", "Carol", "Alice"]
        );

        let Json(first_project_page) = aggregate_projects(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(Some(org_slug.clone()), None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            string_values(&first_project_page, "projects", "project_name"),
            vec!["alpha", "beta"]
        );
        let Json(second_project_page) = aggregate_projects(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(
                Some(org_slug.clone()),
                None,
                Some(2),
                Some(required_next_cursor(&first_project_page)),
            )),
        )
        .await?;
        assert_eq!(
            string_values(&second_project_page, "projects", "project_name"),
            vec!["gamma"]
        );

        let mut rollup_state = state.clone();
        rollup_state.config.dashboard_use_rollups = true;
        let Json(first_rollup_developer_page) = aggregate_developers(
            State(rollup_state.clone()),
            auth.clone(),
            Query(aggregate_query(Some(org_slug.clone()), None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            string_values(&first_rollup_developer_page, "developers", "name"),
            vec!["Alice", "Bob"]
        );
        let Json(second_rollup_developer_page) = aggregate_developers(
            State(rollup_state.clone()),
            auth.clone(),
            Query(aggregate_query(
                Some(org_slug.clone()),
                None,
                Some(2),
                Some(required_next_cursor(&first_rollup_developer_page)),
            )),
        )
        .await?;
        assert_eq!(
            string_values(&second_rollup_developer_page, "developers", "name"),
            vec!["Carol"]
        );

        let Json(first_rollup_project_page) = aggregate_projects(
            State(rollup_state.clone()),
            auth.clone(),
            Query(aggregate_query(Some(org_slug.clone()), None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            string_values(&first_rollup_project_page, "projects", "project_name"),
            vec!["alpha", "beta"]
        );
        let Json(second_rollup_project_page) = aggregate_projects(
            State(rollup_state),
            auth.clone(),
            Query(aggregate_query(
                Some(org_slug.clone()),
                None,
                Some(2),
                Some(required_next_cursor(&first_rollup_project_page)),
            )),
        )
        .await?;
        assert_eq!(
            string_values(&second_rollup_project_page, "projects", "project_name"),
            vec!["gamma"]
        );

        let Json(first_tool_page) = aggregate_tools(
            State(state.clone()),
            auth.clone(),
            Query(aggregate_query(Some(org_slug.clone()), None, Some(2), None)),
        )
        .await?;
        assert_eq!(
            string_values(&first_tool_page, "tools", "tool_model"),
            vec!["codex::gpt-5", "qoder::unknown"]
        );
        let Json(second_tool_page) = aggregate_tools(
            State(state),
            auth,
            Query(aggregate_query(
                Some(org_slug),
                None,
                Some(2),
                Some(required_next_cursor(&first_tool_page)),
            )),
        )
        .await?;
        assert_eq!(
            string_values(&second_tool_page, "tools", "tool_model"),
            vec!["trae::unknown"]
        );

        db.cleanup().await?;
        Ok(())
    }

    struct TestDatabase {
        pool: PgPool,
        admin_pool: PgPool,
        db_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn new() -> anyhow::Result<Option<Self>> {
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
                        "skipping dashboard database test: could not connect to admin database: {error}"
                    );
                    return Ok(None);
                }
            };

            if let Err(error) = create_database(&admin_pool, &db_name).await {
                eprintln!(
                    "skipping dashboard database test: could not create isolated database {db_name}: {error}"
                );
                admin_pool.close().await;
                return Ok(None);
            }

            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect(&test_url)
                .await?;
            crate::db::run_migrations(&pool).await?;

            Ok(Some(Self {
                pool,
                admin_pool,
                db_name,
                database_url: test_url,
            }))
        }

        fn state(&self) -> anyhow::Result<AppState> {
            let config = test_config(&self.database_url);
            let redis = redis::Client::open(config.redis_url.clone())?;
            let auth_password_limiter = crate::routes::auth_password_limiter(&config);
            let cas_store = crate::services::cas::CasStore::new(&config)?;

            Ok(AppState {
                db: self.pool.clone(),
                redis,
                config,
                cas_store,
                rate_limiter: crate::services::rate_limit::RateLimiter::new(),
                auth_password_limiter,
            })
        }

        async fn cleanup(self) -> anyhow::Result<()> {
            self.pool.close().await;
            drop_database(&self.admin_pool, &self.db_name).await?;
            self.admin_pool.close().await;
            Ok(())
        }
    }

    async fn insert_test_identity(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
        let user_id = Uuid::new_v4();
        let org_id = Uuid::new_v4();

        sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
            .bind(org_id)
            .bind("Dashboard Test Org")
            .bind(format!("dashboard-test-{}", org_id.simple()))
            .execute(pool)
            .await?;
        sqlx::query("INSERT INTO users (id, email, name, default_org_id) VALUES ($1, $2, $3, $4)")
            .bind(user_id)
            .bind(format!("{user_id}@example.com"))
            .bind("Dashboard Test User")
            .bind(org_id)
            .execute(pool)
            .await?;
        sqlx::query("INSERT INTO org_members (user_id, org_id, role) VALUES ($1, $2, $3)")
            .bind(user_id)
            .bind(org_id)
            .bind("member")
            .execute(pool)
            .await?;

        Ok((user_id, org_id))
    }

    async fn insert_organization(pool: &PgPool, name: &str) -> anyhow::Result<(Uuid, String)> {
        let org_id = Uuid::new_v4();
        let slug = format!(
            "{}-{}",
            name.to_ascii_lowercase().replace(' ', "-"),
            org_id.simple()
        );

        sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
            .bind(org_id)
            .bind(name)
            .bind(&slug)
            .execute(pool)
            .await?;

        Ok((org_id, slug))
    }

    async fn insert_department(pool: &PgPool, org_id: Uuid, name: &str) -> anyhow::Result<Uuid> {
        insert_department_with_parent(pool, org_id, name, None).await
    }

    async fn insert_department_benchmark_fixture(
        pool: &PgPool,
        org_id: Uuid,
        slug_prefix: &str,
        count: i32,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO departments (org_id, code, name, slug) \
             SELECT $1, \
                    'F' || LPAD(series::text, 6, '0'), \
                    'Department ' || LPAD(series::text, 6, '0'), \
                    $2 || '-' || series::text \
             FROM generate_series(1, $3::integer) AS series",
        )
        .bind(org_id)
        .bind(slug_prefix)
        .bind(count)
        .execute(pool)
        .await?;

        Ok(())
    }

    async fn insert_department_with_parent(
        pool: &PgPool,
        org_id: Uuid,
        name: &str,
        parent_id: Option<Uuid>,
    ) -> anyhow::Result<Uuid> {
        let department_id = Uuid::new_v4();
        let slug = format!(
            "{}-{}",
            name.to_ascii_lowercase().replace(' ', "-"),
            department_id.simple()
        );
        let code = format!(
            "TEST-{}-{}",
            name.to_ascii_uppercase().replace(' ', "-"),
            department_id.simple()
        );

        sqlx::query(
            "INSERT INTO departments (id, org_id, code, name, slug, parent_id) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(department_id)
        .bind(org_id)
        .bind(code)
        .bind(name)
        .bind(slug)
        .bind(parent_id)
        .execute(pool)
        .await?;

        Ok(department_id)
    }

    async fn insert_user(
        pool: &PgPool,
        org_id: Uuid,
        name: &str,
        role: &str,
        department_id: Option<Uuid>,
    ) -> anyhow::Result<Uuid> {
        let user_id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, email, name, default_org_id) VALUES ($1, $2, $3, $4)")
            .bind(user_id)
            .bind(format!("{user_id}@example.com"))
            .bind(name)
            .bind(org_id)
            .execute(pool)
            .await?;
        sqlx::query(
            "INSERT INTO org_members (user_id, org_id, department_id, role) VALUES ($1, $2, $3, $4)",
        )
        .bind(user_id)
        .bind(org_id)
        .bind(department_id)
        .bind(role)
        .execute(pool)
        .await?;

        Ok(user_id)
    }

    async fn insert_dashboard_metrics_fixture(
        pool: &PgPool,
        user_id: Uuid,
        org_id: Uuid,
    ) -> anyhow::Result<()> {
        insert_dashboard_metric_row(
            pool,
            user_id,
            org_id,
            1_700_000_000,
            "repo-a",
            "abc1",
            30,
            20,
            5,
            2,
            3,
        )
        .await?;
        insert_dashboard_metric_row(
            pool,
            user_id,
            org_id,
            1_700_086_400,
            "repo-b",
            "abc2",
            40,
            10,
            4,
            1,
            2,
        )
        .await?;
        Ok(())
    }

    async fn insert_dashboard_metric_row(
        pool: &PgPool,
        user_id: Uuid,
        org_id: Uuid,
        timestamp: i64,
        repo_url: &str,
        commit_sha: &str,
        total_lines: i32,
        total_ai_lines: i32,
        tool_ai_lines: i32,
        tool_mixed_lines: i32,
        tool_accepted: i32,
    ) -> anyhow::Result<()> {
        insert_dashboard_metric_row_with_tool(
            pool,
            user_id,
            org_id,
            timestamp,
            repo_url,
            commit_sha,
            total_lines,
            total_ai_lines,
            "codex::gpt-5",
            tool_ai_lines,
            tool_mixed_lines,
            tool_accepted,
        )
        .await?;

        sqlx::query(
            r#"UPDATE metrics_tool_model_events
            SET mixed_additions = $1, ai_accepted = $2
            WHERE org_id = $3 AND user_id = $4 AND timestamp = $5 AND tool_model = 'codex::gpt-5'"#,
        )
        .bind(i64::from(tool_mixed_lines))
        .bind(i64::from(tool_accepted))
        .bind(org_id)
        .bind(user_id)
        .bind(timestamp)
        .execute(pool)
        .await?;

        sqlx::query(
            r#"UPDATE metrics_daily_rollups
            SET mixed_lines = $1, ai_accepted = $2
            WHERE org_id = $3 AND user_id = $4 AND repo_url = $5 AND tool_model = 'codex::gpt-5'"#,
        )
        .bind(i64::from(tool_mixed_lines))
        .bind(i64::from(tool_accepted))
        .bind(org_id)
        .bind(user_id)
        .bind(repo_url)
        .execute(pool)
        .await?;

        Ok(())
    }

    async fn insert_dashboard_metric_row_with_tool(
        pool: &PgPool,
        user_id: Uuid,
        org_id: Uuid,
        timestamp: i64,
        repo_url: &str,
        commit_sha: &str,
        total_lines: i32,
        total_ai_lines: i32,
        tool_model: &str,
        tool_ai_lines: i32,
        tool_mixed_lines: i32,
        tool_accepted: i32,
    ) -> anyhow::Result<()> {
        let raw_values = serde_json::json!({
            "3": ["all", tool_model],
            "4": [0, 0],
            "5": [total_ai_lines, tool_ai_lines],
            "6": [0, 0],
            "7": [total_ai_lines, tool_ai_lines],
            "8": [0, 0],
        });
        let tool_model_pairs = serde_json::json!(["all", tool_model]);

        let metric_event_id: i64 = sqlx::query_scalar(
            r#"INSERT INTO metrics_events (
                event_type, timestamp, user_id, org_id, repo_url, commit_sha,
                human_additions, ai_additions, mixed_additions, ai_accepted,
                git_diff_added_lines, tool_model_pairs, raw_values
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            RETURNING id"#,
        )
        .bind(1_i16)
        .bind(timestamp)
        .bind(user_id)
        .bind(org_id)
        .bind(repo_url)
        .bind(commit_sha)
        .bind(total_lines - total_ai_lines)
        .bind(total_ai_lines)
        .bind(tool_mixed_lines)
        .bind(tool_accepted)
        .bind(total_lines)
        .bind(&tool_model_pairs)
        .bind(&raw_values)
        .fetch_one(pool)
        .await?;

        sqlx::query(
            r#"INSERT INTO metrics_tool_model_events (
                metric_event_id, org_id, user_id, timestamp, tool_model,
                ai_additions, mixed_additions, ai_accepted,
                total_ai_additions, total_ai_deletions
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $6, 0)"#,
        )
        .bind(metric_event_id)
        .bind(org_id)
        .bind(user_id)
        .bind(timestamp)
        .bind(tool_model)
        .bind(i64::from(tool_ai_lines))
        .bind(i64::from(tool_mixed_lines))
        .bind(i64::from(tool_accepted))
        .execute(pool)
        .await?;

        sqlx::query(
            r#"INSERT INTO metrics_daily_rollups (
                day, org_id, user_id, repo_url, tool_model,
                commits, total_lines, ai_lines, human_lines, mixed_lines, ai_accepted
            ) VALUES
            ((to_timestamp($1) AT TIME ZONE 'UTC')::date, $2, $3, $4, '', 1, $5, $6, $7, $8, $9),
            ((to_timestamp($1) AT TIME ZONE 'UTC')::date, $2, $3, $4, $10, 1, 0, $11, 0, $12, $13)
            ON CONFLICT (day, org_id, user_id, repo_url, tool_model) DO UPDATE SET
                commits = metrics_daily_rollups.commits + EXCLUDED.commits,
                total_lines = metrics_daily_rollups.total_lines + EXCLUDED.total_lines,
                ai_lines = metrics_daily_rollups.ai_lines + EXCLUDED.ai_lines,
                human_lines = metrics_daily_rollups.human_lines + EXCLUDED.human_lines,
                mixed_lines = metrics_daily_rollups.mixed_lines + EXCLUDED.mixed_lines,
                ai_accepted = metrics_daily_rollups.ai_accepted + EXCLUDED.ai_accepted,
                updated_at = now()"#,
        )
        .bind(timestamp)
        .bind(org_id)
        .bind(user_id)
        .bind(repo_url)
        .bind(i64::from(total_lines))
        .bind(i64::from(total_ai_lines))
        .bind(i64::from(total_lines - total_ai_lines))
        .bind(0_i64)
        .bind(0_i64)
        .bind(tool_model)
        .bind(i64::from(tool_ai_lines))
        .bind(i64::from(tool_mixed_lines))
        .bind(i64::from(tool_accepted))
        .execute(pool)
        .await?;

        Ok(())
    }

    fn aggregate_query(
        org: Option<String>,
        since: Option<String>,
        limit: Option<i64>,
        cursor: Option<String>,
    ) -> AggregateQuery {
        AggregateQuery {
            org,
            parent_id: None,
            since,
            until: None,
            sort_by: None,
            sort_order: None,
            limit,
            cursor,
        }
    }

    fn global_admin_auth(user_id: Uuid) -> DashboardAuth {
        DashboardAuth(AuthIdentity {
            user_id,
            email: format!("{user_id}@example.com"),
            name: "Global Admin".into(),
            org_id: None,
            org_slug: None,
            department_id: None,
            role: Some("owner".into()),
            scopes: vec![],
            auth_method: AuthMethod::BearerToken,
        })
    }

    fn org_admin_auth(user_id: Uuid, org_id: Uuid, org_slug: &str) -> DashboardAuth {
        DashboardAuth(AuthIdentity {
            user_id,
            email: format!("{user_id}@example.com"),
            name: "Org Admin".into(),
            org_id: Some(org_id),
            org_slug: Some(org_slug.to_string()),
            department_id: None,
            role: Some("admin".into()),
            scopes: vec![],
            auth_method: AuthMethod::BearerToken,
        })
    }

    fn department_member_auth(
        user_id: Uuid,
        org_id: Uuid,
        org_slug: &str,
        department_id: Uuid,
    ) -> DashboardAuth {
        DashboardAuth(AuthIdentity {
            user_id,
            email: format!("{user_id}@example.com"),
            name: "Department Member".into(),
            org_id: Some(org_id),
            org_slug: Some(org_slug.to_string()),
            department_id: Some(department_id),
            role: Some("member".into()),
            scopes: vec![],
            auth_method: AuthMethod::BearerToken,
        })
    }

    fn string_values(page: &Value, list_key: &str, field: &str) -> Vec<String> {
        page[list_key]
            .as_array()
            .expect("response field should be an array")
            .iter()
            .map(|entry| {
                entry[field]
                    .as_str()
                    .expect("field should be a string")
                    .to_string()
            })
            .collect()
    }

    fn pair_values(
        page: &Value,
        list_key: &str,
        first_field: &str,
        second_field: &str,
    ) -> Vec<(String, String)> {
        page[list_key]
            .as_array()
            .expect("response field should be an array")
            .iter()
            .map(|entry| {
                (
                    entry[first_field]
                        .as_str()
                        .expect("first field should be a string")
                        .to_string(),
                    entry[second_field]
                        .as_str()
                        .expect("second field should be a string")
                        .to_string(),
                )
            })
            .collect()
    }

    fn required_next_cursor(page: &Value) -> String {
        page["pagination"]["next_cursor"]
            .as_str()
            .expect("page should include next_cursor")
            .to_string()
    }

    fn test_config(database_url: &str) -> AppConfig {
        AppConfig {
            database_url: database_url.to_string(),
            database_max_connections: 20,
            database_min_connections: 1,
            database_acquire_timeout_seconds: 5,
            redis_url: "redis://127.0.0.1:6379".to_string(),
            jwt_secret: "dashboard-test-secret".to_string(),
            s3_endpoint: "http://localhost:9000".to_string(),
            s3_bucket: "git-ai-cas".to_string(),
            s3_access_key: "minioadmin".to_string(),
            s3_secret_key: "minioadmin".to_string(),
            s3_region: "us-east-1".to_string(),
            cas_upload_concurrency: 8,
            auth_password_concurrency: 8,
            metrics_rollup_write_mode: MetricsRollupWriteMode::Sync,
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
            rate_limit_admin_max_requests: 300,
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
        format!("git_ai_dashboard_test_{}", Uuid::new_v4().simple())
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
