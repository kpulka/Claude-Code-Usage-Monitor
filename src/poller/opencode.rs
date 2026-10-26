use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{build_agent, parse_iso8601, PollError};
use crate::diagnose;
use crate::models::{UsageData, UsageSection};

const GO_STATUS_URL: &str = "https://opencode.ai/console/api/go/status";
/// API-key route. OpenCode Go and Zen share one key, so this needs no browser
/// session and never expires; the console route below stays as a fallback for
/// accounts that only have a signed-in browser.
const GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
const DASHBOARD_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const USAGE_USER_AGENT: &str = concat!("claude-code-usage-monitor/", env!("CARGO_PKG_VERSION"));
const WORKSPACE_ID_ENV: &str = "OPENCODE_GO_WORKSPACE_ID";
const AUTH_COOKIE_ENV: &str = "OPENCODE_GO_AUTH_COOKIE";
const API_KEY_ENV: &str = "OPENCODE_GO_API_KEY";
const CONFIG_FILE_ENV: &str = "OPENCODE_GO_CONFIG_FILE";
/// Point at a different `auth.json` (used by tests and unusual installs).
const AUTH_FILE_ENV: &str = "OPENCODE_AUTH_FILE";

#[derive(Deserialize)]
struct DashboardConfig {
    #[serde(alias = "workspaceId", alias = "workspaceID")]
    workspace_id: String,
    #[serde(alias = "authCookie", alias = "cookie")]
    auth_cookie: String,
}

struct DashboardCredentials {
    workspace_id: String,
    auth_cookie: String,
    source: String,
}

/// Plain API key, as stored by the OpenCode CLI or supplied by the user.
struct ApiKeyCredentials {
    api_key: String,
    source: String,
}

/// One entry of OpenCode's `auth.json`. Only API-key providers carry `key`;
/// OAuth providers carry `access`/`refresh` instead and are ignored here.
#[derive(Deserialize)]
struct AuthFileEntry {
    #[serde(default)]
    key: String,
}

/// Optional `{"apiKey": "..."}` in the same config file the console route uses.
#[derive(Deserialize)]
struct ApiKeyConfig {
    #[serde(alias = "apiKey", alias = "key")]
    api_key: String,
}

// Wire shape of `GET https://opencode.ai/zen/go/v1/usage`:
// {"usage":{"rolling":{"status":"ok","percent":9,"resetsAt":"..."},...}}
#[derive(Deserialize)]
struct GoUsage {
    usage: Option<GoUsageWindows>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoUsageWindows {
    rolling: Option<GoUsageWindowPayload>,
    weekly: Option<GoUsageWindowPayload>,
    monthly: Option<GoUsageWindowPayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoUsageWindowPayload {
    percent: Option<f64>,
    resets_at: Option<String>,
    status: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct UsageWindow {
    usage_percent: f64,
    resets_at: Option<SystemTime>,
}

#[derive(Debug, Default, PartialEq)]
struct DashboardUsage {
    rolling: Option<UsageWindow>,
    weekly: Option<UsageWindow>,
    monthly: Option<UsageWindow>,
}

// The console JSON API serializes microcent amounts as strings (JavaScript BigInts).
#[derive(Deserialize)]
struct GoStatus {
    access: Option<GoAccess>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoAccess {
    ends_at: String,
    meters: GoMeters,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoMeters {
    five_hour: GoTimedMeter,
    week: GoTimedMeter,
    month: GoMeter,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoTimedMeter {
    #[serde(flatten)]
    meter: GoMeter,
    // An unused rolling window has no reset time until the first request.
    resets_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoMeter {
    limit_micro_cents: String,
    used_micro_cents: String,
}

pub(super) fn poll_opencode() -> Result<UsageData, PollError> {
    poll_sources(
        read_api_key_credentials().as_ref(),
        read_dashboard_credentials().as_ref(),
        GO_USAGE_URL,
        GO_STATUS_URL,
    )
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    vec![credential_watch_signature()]
}

/// Prefer the API key: the OpenCode CLI already stores it, it never expires and
/// it needs no browser session. The console cookie stays as a fallback so
/// accounts that only ever signed in through the browser keep working.
fn poll_sources(
    api_key: Option<&ApiKeyCredentials>,
    dashboard: Option<&DashboardCredentials>,
    usage_url: &str,
    status_url: &str,
) -> Result<UsageData, PollError> {
    if api_key.is_none() && dashboard.is_none() {
        diagnose::log("OpenCode usage poll failed: no API key and no dashboard credentials found");
        return Err(PollError::NoCredentials);
    }

    let mut last_error = None;
    if let Some(credentials) = api_key {
        match fetch_api_key_usage(credentials, usage_url)
            .and_then(|usage| usage_data_from_windows(usage, &credentials.source))
        {
            Ok(usage) => return Ok(usage),
            Err(error) => {
                diagnose::log(format!(
                    "OpenCode API key poll failed via {}: {error:?}",
                    credentials.source
                ));
                last_error = Some(error);
            }
        }
    }

    if let Some(credentials) = dashboard {
        match fetch_dashboard_usage(credentials, status_url)
            .and_then(|usage| usage_data_from_windows(usage, &credentials.source))
        {
            Ok(usage) => return Ok(usage),
            Err(error) => {
                diagnose::log(format!(
                    "OpenCode dashboard poll failed via {}: {error:?}",
                    credentials.source
                ));
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or(PollError::NoCredentials))
}

fn usage_data_from_windows(usage: DashboardUsage, source: &str) -> Result<UsageData, PollError> {
    if usage.rolling.is_none() && usage.weekly.is_none() && usage.monthly.is_none() {
        diagnose::log(format!("OpenCode returned no usage windows from {source}"));
        return Err(PollError::RequestFailed);
    }

    let session = usage
        .rolling
        .as_ref()
        .map(section_from_window)
        .unwrap_or_default();
    let (weekly, weekly_label) = select_long_window(&usage);

    Ok(UsageData {
        limits: Vec::new(),
        session,
        weekly,
        weekly_label,
        // The monthly window is kept available to themes alongside the
        // auto-selected `weekly` slot (which prefers the more constrained
        // of the two windows, as before).
        monthly: usage.monthly.as_ref().map(section_from_window),
        credits: None,
        stale: false,
        ..Default::default()
    })
}

fn select_long_window(usage: &DashboardUsage) -> (UsageSection, Option<String>) {
    match (&usage.weekly, &usage.monthly) {
        (Some(weekly), Some(monthly)) if monthly.usage_percent > weekly.usage_percent => {
            (section_from_window(monthly), Some("30d".to_string()))
        }
        (Some(weekly), _) => (section_from_window(weekly), Some("7d".to_string())),
        (None, Some(monthly)) => (section_from_window(monthly), Some("30d".to_string())),
        (None, None) => (UsageSection::default(), None),
    }
}

fn section_from_window(window: &UsageWindow) -> UsageSection {
    UsageSection {
        available: true,
        percentage: window.usage_percent.clamp(0.0, 100.0),
        resets_at: window.resets_at,
    }
}

fn read_dashboard_credentials() -> Option<DashboardCredentials> {
    if let (Some(workspace_id), Some(auth_cookie)) = (
        non_empty_environment(WORKSPACE_ID_ENV),
        non_empty_environment(AUTH_COOKIE_ENV),
    ) {
        if valid_workspace_id(&workspace_id) && valid_cookie(&auth_cookie) {
            return Some(DashboardCredentials {
                workspace_id,
                auth_cookie,
                source: "environment".to_string(),
            });
        }
    }

    dashboard_config_paths()
        .into_iter()
        .find_map(|path| read_dashboard_config(&path))
}

fn read_dashboard_config(path: &Path) -> Option<DashboardCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: DashboardConfig = serde_json::from_str(&content).ok()?;
    let workspace_id = config.workspace_id.trim().to_string();
    let auth_cookie = config.auth_cookie.trim().to_string();
    if !valid_workspace_id(&workspace_id) || !valid_cookie(&auth_cookie) {
        return None;
    }
    Some(DashboardCredentials {
        workspace_id,
        auth_cookie,
        source: path.display().to_string(),
    })
}

fn fetch_dashboard_usage(
    credentials: &DashboardCredentials,
    url: &str,
) -> Result<DashboardUsage, PollError> {
    fetch_go_status(credentials, url)
}

fn fetch_api_key_usage(
    credentials: &ApiKeyCredentials,
    url: &str,
) -> Result<DashboardUsage, PollError> {
    let mut response = match build_agent()?
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", credentials.api_key))
        .header("User-Agent", USAGE_USER_AGENT)
        .call()
        .and_then(super::check_http_status)
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401 | 403)) => return Err(PollError::AuthRequired),
        Err(ureq::Error::StatusCode(429)) => {
            diagnose::log("OpenCode usage request was rate limited");
            return Err(PollError::RequestFailed);
        }
        Err(error) => {
            diagnose::log_error("OpenCode usage request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let usage = response
        .body_mut()
        .read_json::<Option<GoUsage>>()
        .map_err(|error| {
            diagnose::log_error("OpenCode usage response is not valid JSON", error);
            PollError::RequestFailed
        })?;
    usage_from_api_response(usage)
}

fn usage_from_api_response(response: Option<GoUsage>) -> Result<DashboardUsage, PollError> {
    let windows = response
        .and_then(|response| response.usage)
        .ok_or_else(|| {
            diagnose::log("OpenCode usage response carried no usage object");
            PollError::RequestFailed
        })?;
    let usage = DashboardUsage {
        rolling: window_from_api_payload(windows.rolling.as_ref())?,
        weekly: window_from_api_payload(windows.weekly.as_ref())?,
        monthly: window_from_api_payload(windows.monthly.as_ref())?,
    };
    if usage.rolling.is_none() && usage.weekly.is_none() && usage.monthly.is_none() {
        diagnose::log("OpenCode usage response had no usable percentage");
        return Err(PollError::RequestFailed);
    }
    Ok(usage)
}

fn window_from_api_payload(
    payload: Option<&GoUsageWindowPayload>,
) -> Result<Option<UsageWindow>, PollError> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    if let Some(status) = payload.status.as_deref() {
        if !status.eq_ignore_ascii_case("ok") {
            diagnose::log(format!("OpenCode usage window reported status {status}"));
        }
    }
    let Some(percent) = payload.percent else {
        // A window the account has not touched yet may omit its percentage.
        diagnose::log("OpenCode usage window omitted its percentage");
        return Ok(None);
    };
    if !percent.is_finite() {
        diagnose::log("OpenCode usage window reported a non-finite percentage");
        return Err(PollError::RequestFailed);
    }
    let resets_at = payload
        .resets_at
        .as_deref()
        .map(|value| {
            parse_iso8601(Some(value)).ok_or_else(|| {
                diagnose::log("OpenCode usage window contains an invalid reset time");
                PollError::RequestFailed
            })
        })
        .transpose()?;
    Ok(Some(UsageWindow {
        usage_percent: percent,
        resets_at,
    }))
}

fn fetch_go_status(
    credentials: &DashboardCredentials,
    url: &str,
) -> Result<DashboardUsage, PollError> {
    let cookie = if credentials.auth_cookie.split(';').any(|part| {
        let part = part.trim_start();
        part.starts_with("auth=") || part.starts_with("__Host-console_session=")
    }) {
        credentials.auth_cookie.clone()
    } else {
        // Bare legacy auth values may contain '=' padding; that alone does
        // not identify a complete Cookie header.
        format!("auth={}", credentials.auth_cookie)
    };

    let mut response = match build_agent()?
        .get(url)
        .header("Accept", "application/json")
        .header("x-org-id", &credentials.workspace_id)
        .header("Cookie", &cookie)
        .header("User-Agent", DASHBOARD_USER_AGENT)
        .call()
        .and_then(super::check_http_status)
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401 | 403)) => return Err(PollError::AuthRequired),
        Err(error) => {
            diagnose::log_error("OpenCode Go status request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let status = response
        .body_mut()
        .read_json::<Option<GoStatus>>()
        .map_err(|error| {
            diagnose::log_error("OpenCode Go status response is not valid JSON", error);
            PollError::RequestFailed
        })?;
    usage_from_status(status)
}

fn usage_from_status(status: Option<GoStatus>) -> Result<DashboardUsage, PollError> {
    let access = status.and_then(|status| status.access).ok_or_else(|| {
        diagnose::log("OpenCode Go status returned no active subscription access");
        PollError::RequestFailed
    })?;
    Ok(DashboardUsage {
        rolling: Some(window_from_meter(
            &access.meters.five_hour.meter,
            access.meters.five_hour.resets_at.as_deref(),
        )?),
        weekly: Some(window_from_meter(
            &access.meters.week.meter,
            access.meters.week.resets_at.as_deref(),
        )?),
        monthly: Some(window_from_meter(
            &access.meters.month,
            Some(&access.ends_at),
        )?),
    })
}

fn window_from_meter(meter: &GoMeter, resets_at: Option<&str>) -> Result<UsageWindow, PollError> {
    let limit = meter.limit_micro_cents.parse::<u128>().map_err(|_| {
        diagnose::log("OpenCode Go status contains an invalid usage limit");
        PollError::RequestFailed
    })?;
    let used = meter.used_micro_cents.parse::<u128>().map_err(|_| {
        diagnose::log("OpenCode Go status contains an invalid usage amount");
        PollError::RequestFailed
    })?;
    let resets_at = resets_at
        .map(|value| {
            parse_iso8601(Some(value)).ok_or_else(|| {
                diagnose::log("OpenCode Go status contains an invalid reset time");
                PollError::RequestFailed
            })
        })
        .transpose()?;
    Ok(UsageWindow {
        usage_percent: if limit == 0 {
            0.0
        } else {
            used as f64 / limit as f64 * 100.0
        },
        resets_at,
    })
}

fn read_api_key_credentials() -> Option<ApiKeyCredentials> {
    if let Some(api_key) = non_empty_environment(API_KEY_ENV).filter(|value| valid_api_key(value)) {
        return Some(ApiKeyCredentials {
            api_key,
            source: "environment".to_string(),
        });
    }

    // The OpenCode CLI stores `{"<provider>": {"type": "api", "key": "..."}}`.
    opencode_auth_paths()
        .into_iter()
        .find_map(|path| read_api_key_from_auth_file(&path))
        .or_else(|| {
            dashboard_config_paths()
                .into_iter()
                .find_map(|path| read_api_key_from_config(&path))
        })
}

fn read_api_key_from_auth_file(path: &Path) -> Option<ApiKeyCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    let entries: BTreeMap<String, AuthFileEntry> = serde_json::from_str(&content).ok()?;
    // Go and Zen share one key, so either provider id is usable.
    let api_key = ["opencode-go", "opencode-go-zen", "opencode"]
        .iter()
        .filter_map(|id| entries.get(*id))
        .map(|entry| entry.key.trim())
        .find(|key| valid_api_key(key))?
        .to_string();
    Some(ApiKeyCredentials {
        api_key,
        source: path.display().to_string(),
    })
}

fn read_api_key_from_config(path: &Path) -> Option<ApiKeyCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: ApiKeyConfig = serde_json::from_str(&content).ok()?;
    let api_key = config.api_key.trim().to_string();
    valid_api_key(&api_key).then(|| ApiKeyCredentials {
        api_key,
        source: path.display().to_string(),
    })
}

fn opencode_auth_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = non_empty_environment(AUTH_FILE_ENV).map(PathBuf::from) {
        paths.push(path);
    }
    if let Some(data_home) = non_empty_environment("XDG_DATA_HOME").map(PathBuf::from) {
        paths.push(data_home.join("opencode").join("auth.json"));
    }
    if let Some(home) = crate::known_folders::home_dir() {
        paths.push(
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("auth.json"),
        );
    }
    paths
}

fn dashboard_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = non_empty_environment(CONFIG_FILE_ENV).map(PathBuf::from) {
        paths.push(path);
    }
    if let Some(app_data) = non_empty_environment("APPDATA").map(PathBuf::from) {
        paths.push(app_data.join("opencode-go").join("config.json"));
    }
    if let Some(config_home) = non_empty_environment("XDG_CONFIG_HOME").map(PathBuf::from) {
        paths.push(config_home.join("opencode-bar").join("opencode-go.json"));
        paths.push(config_home.join("opencode-quota").join("opencode-go.json"));
    }
    if let Some(home) = crate::known_folders::home_dir() {
        paths.push(
            home.join(".config")
                .join("opencode-bar")
                .join("opencode-go.json"),
        );
        paths.push(
            home.join(".config")
                .join("opencode-quota")
                .join("opencode-go.json"),
        );
    }
    paths
}

fn non_empty_environment(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn valid_workspace_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_cookie(value: &str) -> bool {
    !value.is_empty() && !value.bytes().any(|byte| byte == b'\r' || byte == b'\n')
}

/// API keys are short opaque tokens; anything with whitespace or quotes would
/// break the request header, so reject it rather than send a mangled request.
fn valid_api_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\')
}

fn credential_watch_signature() -> String {
    let mut parts = Vec::new();
    match read_api_key_credentials() {
        Some(credentials) => {
            let mut hasher = DefaultHasher::new();
            credentials.api_key.hash(&mut hasher);
            parts.push(format!(
                "api_key|present|{}|{:x}|{}",
                credentials.api_key.len(),
                hasher.finish(),
                credentials.source
            ));
        }
        None => parts.push("api_key|missing".to_string()),
    }
    match read_dashboard_credentials() {
        Some(credentials) => {
            let mut hasher = DefaultHasher::new();
            credentials.workspace_id.hash(&mut hasher);
            credentials.auth_cookie.hash(&mut hasher);
            parts.push(format!(
                "dashboard|present|{}|{}|{:x}|{}",
                credentials.workspace_id.len(),
                credentials.auth_cookie.len(),
                hasher.finish(),
                credentials.source
            ));
        }
        None => parts.push("dashboard|missing".to_string()),
    }
    for path in dashboard_config_paths() {
        parts.push(path_signature("config", &path));
    }
    for path in opencode_auth_paths() {
        parts.push(path_signature("auth", &path));
    }
    parts.join(";;")
}

fn path_signature(kind: &str, path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!(
                "{kind}:{}|present|{}|{modified}",
                path.display(),
                metadata.len()
            )
        }
        Err(_) => format!("{kind}:{}|missing", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Wire shape verified against the console's Go status schema and usage UI.
    const GO_STATUS_JSON: &str = r#"{
        "subscriberUserID": "usr_example",
        "useBalance": false,
        "access": {
            "startsAt": "2026-09-01T00:00:00.000Z",
            "endsAt": "2026-10-01T00:00:00.000Z",
            "meters": {
                "fiveHour": {
                    "limitMicroCents": "2000000000",
                    "usedMicroCents": "250000000",
                    "resetsAt": "2026-09-17T05:00:00.000Z"
                },
                "week": {
                    "limitMicroCents": "10000000000",
                    "usedMicroCents": "4500000000",
                    "resetsAt": "2026-09-21T00:00:00.000Z"
                },
                "month": {
                    "limitMicroCents": "20000000000",
                    "usedMicroCents": "12000000000"
                }
            }
        }
    }"#;

    #[test]
    fn console_status_maps_meter_percentages_and_absolute_resets() {
        let usage = usage_from_status(serde_json::from_str(GO_STATUS_JSON).unwrap()).unwrap();
        let rolling = usage.rolling.as_ref().unwrap();
        assert_eq!(rolling.usage_percent, 12.5);
        assert_eq!(
            rolling.resets_at,
            parse_iso8601(Some("2026-09-17T05:00:00Z"))
        );
        let weekly = usage.weekly.as_ref().unwrap();
        assert_eq!(weekly.usage_percent, 45.0);
        assert_eq!(
            weekly.resets_at,
            parse_iso8601(Some("2026-09-21T00:00:00Z"))
        );
        let (long, label) = select_long_window(&usage);
        assert_eq!(long.percentage, 60.0);
        assert_eq!(label.as_deref(), Some("30d"));
        assert_eq!(long.resets_at, parse_iso8601(Some("2026-10-01T00:00:00Z")));
    }

    #[test]
    fn console_status_preserves_idle_rolling_window_without_reset() {
        let json = GO_STATUS_JSON
            .replace("\"250000000\"", "\"0\"")
            .replace("\"2026-09-17T05:00:00.000Z\"", "null");
        let usage = usage_from_status(serde_json::from_str(&json).unwrap()).unwrap();
        let section = section_from_window(usage.rolling.as_ref().unwrap());
        assert!(section.available);
        assert_eq!(section.percentage, 0.0);
        assert_eq!(section.resets_at, None);
    }

    #[test]
    fn console_status_rejects_missing_access_and_invalid_meter_values() {
        for json in ["null", r#"{"access":null}"#] {
            assert_eq!(
                usage_from_status(serde_json::from_str(json).unwrap()),
                Err(PollError::RequestFailed)
            );
        }
        for json in [
            GO_STATUS_JSON.replace("250000000", "-1"),
            GO_STATUS_JSON.replace("250000000", "NaN"),
            GO_STATUS_JSON.replace("250000000", "1.5"),
            GO_STATUS_JSON.replace("2026-09-17T05:00:00.000Z", "invalid-date"),
        ] {
            assert_eq!(
                usage_from_status(serde_json::from_str(&json).unwrap()),
                Err(PollError::RequestFailed)
            );
        }
    }

    #[test]
    fn console_meter_handles_zero_limits_large_amounts_and_overages() {
        let meter = |used: &str, limit: &str| GoMeter {
            used_micro_cents: used.into(),
            limit_micro_cents: limit.into(),
        };
        assert_eq!(
            window_from_meter(&meter("0", "0"), None)
                .unwrap()
                .usage_percent,
            0.0
        );
        assert_eq!(
            window_from_meter(&meter("10000000000000000000", "20000000000000000000"), None)
                .unwrap()
                .usage_percent,
            50.0
        );
        let window = window_from_meter(&meter("120", "100"), None).unwrap();
        assert_eq!(section_from_window(&window).percentage, 100.0);
    }

    fn mock_status_request(
        status: u16,
        body: &str,
        cookie: &str,
    ) -> (Result<DashboardUsage, PollError>, String) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/console/api/go/status",
            listener.local_addr().unwrap()
        );
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        let result = fetch_go_status(
            &DashboardCredentials {
                workspace_id: "wrk_example".into(),
                auth_cookie: cookie.into(),
                source: "test".into(),
            },
            &url,
        );
        (result, server.join().unwrap())
    }

    #[test]
    fn console_request_sends_workspace_context_and_normalizes_auth_cookie() {
        for (cookie, expected) in [
            ("test-token", "auth=test-token"),
            ("test-token==", "auth=test-token=="),
            ("auth=test-token; theme=dark", "auth=test-token; theme=dark"),
            (
                "__Host-console_session=SessionToken==",
                "__Host-console_session=SessionToken==",
            ),
            (
                "__Host-console_session=SessionToken; __stripe_mid=StripeValue",
                "__Host-console_session=SessionToken; __stripe_mid=StripeValue",
            ),
            (
                "theme=dark; __Host-console_session=SessionToken",
                "theme=dark; __Host-console_session=SessionToken",
            ),
            (
                "auth=; __Host-console_session=SessionToken",
                "auth=; __Host-console_session=SessionToken",
            ),
        ] {
            let (result, request) = mock_status_request(200, GO_STATUS_JSON, cookie);
            assert_eq!(result.unwrap().rolling.unwrap().usage_percent, 12.5);
            let sent_cookie = request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("cookie")
                        .then_some(value.trim_start())
                })
                .unwrap();
            assert_eq!(sent_cookie, expected);
            let request = request.to_ascii_lowercase();
            assert!(request.starts_with("get /console/api/go/status http/1.1\r\n"));
            assert!(request.contains("\r\nx-org-id: wrk_example\r\n"));
            assert!(request.contains("\r\naccept: application/json\r\n"));
        }
    }

    #[test]
    fn console_request_distinguishes_auth_errors_from_bad_responses() {
        for status in [401, 403] {
            assert_eq!(
                mock_status_request(status, "{}", "test-token").0,
                Err(PollError::AuthRequired)
            );
        }
        for (status, body) in [
            (400, "{}"),
            (500, "{}"),
            (200, "<html><div id=app></div></html>"),
            (200, "{invalid"),
            (200, "{}"),
            (200, "null"),
            (200, r#"{"access":{"meters":{}}}"#),
        ] {
            assert_eq!(
                mock_status_request(status, body, "test-token").0,
                Err(PollError::RequestFailed)
            );
        }
    }

    #[test]
    fn most_constrained_long_window_is_selected() {
        let usage = DashboardUsage {
            weekly: Some(UsageWindow {
                usage_percent: 40.0,
                resets_at: Some(UNIX_EPOCH),
            }),
            monthly: Some(UsageWindow {
                usage_percent: 70.0,
                resets_at: Some(UNIX_EPOCH),
            }),
            ..Default::default()
        };
        let (section, label) = select_long_window(&usage);
        assert_eq!(section.percentage, 70.0);
        assert_eq!(label.as_deref(), Some("30d"));
    }

    #[test]
    fn idle_opencode_windows_are_available_and_missing_windows_are_not() {
        let window = UsageWindow {
            usage_percent: 0.0,
            resets_at: None,
        };
        let section = section_from_window(&window);
        assert!(section.available);
        assert_eq!(section.percentage, 0.0);
        let usage = DashboardUsage {
            monthly: Some(window),
            ..Default::default()
        };
        let (section, label) = select_long_window(&usage);
        assert!(section.available);
        assert_eq!(label.as_deref(), Some("30d"));
        assert!(!select_long_window(&DashboardUsage::default()).0.available);
    }

    #[test]
    fn dashboard_identifiers_and_cookie_headers_reject_request_injection() {
        assert!(valid_workspace_id("wrk_01-test"));
        assert!(!valid_workspace_id("../other"));
        assert!(valid_cookie("auth=abc; theme=dark"));
        assert!(!valid_cookie("auth=abc\r\nX-Test: injected"));
    }
    // Canonical `GET /zen/go/v1/usage` payload: percentages plus reset times.
    const GO_USAGE_JSON: &str = r#"{
        "usage": {
            "rolling": {"status": "ok", "percent": 9, "resetsAt": "2026-10-26T02:50:45.000Z"},
            "weekly": {"status": "ok", "percent": 0, "resetsAt": "2026-11-02T00:00:00.000Z"},
            "monthly": {"status": "ok", "percent": 69, "resetsAt": "2026-11-04T11:33:14.000Z"}
        }
    }"#;

    /// Serves canned responses on 127.0.0.1 and records the requests it saw.
    fn mock_server(
        routes: Vec<(String, u16, String)>,
        expected_requests: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                let text = String::from_utf8(request).unwrap();
                let path = text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let (_, status, body) = routes
                    .iter()
                    .find(|(route, _, _)| path.starts_with(route.as_str()))
                    .expect("unexpected request path");
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                seen.push(text);
            }
            seen
        });
        (base, handle)
    }

    fn api_key_credentials() -> ApiKeyCredentials {
        ApiKeyCredentials {
            api_key: "sk-test-key".into(),
            source: "test".into(),
        }
    }

    fn dashboard_credentials() -> DashboardCredentials {
        DashboardCredentials {
            workspace_id: "wrk_example".into(),
            auth_cookie: "auth=test-token".into(),
            source: "test".into(),
        }
    }

    #[test]
    fn api_key_usage_maps_percentages_and_resets() {
        let usage = usage_from_api_response(serde_json::from_str(GO_USAGE_JSON).unwrap()).unwrap();
        let rolling = usage.rolling.as_ref().unwrap();
        assert_eq!(rolling.usage_percent, 9.0);
        assert_eq!(
            rolling.resets_at,
            parse_iso8601(Some("2026-10-26T02:50:45Z"))
        );
        assert_eq!(usage.weekly.as_ref().unwrap().usage_percent, 0.0);
        assert_eq!(usage.monthly.as_ref().unwrap().usage_percent, 69.0);
        // Monthly is the most constrained long window, so it owns the long slot.
        let (long, label) = select_long_window(&usage);
        assert_eq!(long.percentage, 69.0);
        assert_eq!(label.as_deref(), Some("30d"));
    }

    #[test]
    fn api_key_usage_tolerates_missing_windows_and_rejects_empty_payloads() {
        let partial = r#"{"usage":{"rolling":{"percent":12.5}}}"#;
        let usage = usage_from_api_response(serde_json::from_str(partial).unwrap()).unwrap();
        assert_eq!(usage.rolling.unwrap().usage_percent, 12.5);
        assert!(usage.weekly.is_none());

        for json in [
            "null",
            r#"{}"#,
            r#"{"usage":null}"#,
            r#"{"usage":{}}"#,
            r#"{"usage":{"rolling":{}}}"#,
            r#"{"usage":{"rolling":{"percent":9,"resetsAt":"not-a-date"}}}"#,
        ] {
            assert_eq!(
                usage_from_api_response(serde_json::from_str(json).unwrap()),
                Err(PollError::RequestFailed),
                "{json}"
            );
        }
    }

    #[test]
    fn api_key_request_sends_bearer_token_without_cookie() {
        let (base, requests) = mock_server(
            vec![("/zen/go/v1/usage".into(), 200, GO_USAGE_JSON.into())],
            1,
        );
        let usage = fetch_api_key_usage(&api_key_credentials(), &format!("{base}/zen/go/v1/usage"))
            .unwrap();
        assert_eq!(usage.rolling.unwrap().usage_percent, 9.0);

        let request = requests.join().unwrap().remove(0).to_ascii_lowercase();
        assert!(request.starts_with("get /zen/go/v1/usage http/1.1"));
        assert!(request.contains("\r\nauthorization: bearer sk-test-key\r\n"));
        assert!(request.contains("\r\naccept: application/json\r\n"));
        assert!(!request.contains("\r\ncookie:"));
        assert!(!request.contains("x-org-id"));
    }

    #[test]
    fn api_key_rejections_and_broken_bodies_are_reported() {
        for status in [401u16, 403] {
            let (base, requests) =
                mock_server(vec![("/zen/go/v1/usage".into(), status, "{}".into())], 1);
            assert_eq!(
                fetch_api_key_usage(&api_key_credentials(), &format!("{base}/zen/go/v1/usage"))
                    .unwrap_err(),
                PollError::AuthRequired
            );
            requests.join().unwrap();
        }
        for body in [
            "{invalid",
            "{}",
            "null",
            r#"{"usage":{}}"#,
            r#"{"usage":{"rolling":{"percent":"high"}}}"#,
        ] {
            let (base, requests) =
                mock_server(vec![("/zen/go/v1/usage".into(), 200, body.into())], 1);
            assert_eq!(
                fetch_api_key_usage(&api_key_credentials(), &format!("{base}/zen/go/v1/usage"))
                    .unwrap_err(),
                PollError::RequestFailed,
                "{body}"
            );
            requests.join().unwrap();
        }
    }

    #[test]
    fn rejected_api_key_falls_back_to_console_cookie() {
        let (base, requests) = mock_server(
            vec![
                ("/zen/go/v1/usage".into(), 401, "{}".into()),
                ("/console/api/go/status".into(), 200, GO_STATUS_JSON.into()),
            ],
            2,
        );
        let usage = poll_sources(
            Some(&api_key_credentials()),
            Some(&dashboard_credentials()),
            &format!("{base}/zen/go/v1/usage"),
            &format!("{base}/console/api/go/status"),
        )
        .unwrap();
        assert_eq!(usage.session.percentage, 12.5);
        requests.join().unwrap();
    }

    #[test]
    fn api_key_alone_is_enough_to_poll() {
        let (base, requests) = mock_server(
            vec![("/zen/go/v1/usage".into(), 200, GO_USAGE_JSON.into())],
            1,
        );
        let usage = poll_sources(
            Some(&api_key_credentials()),
            None,
            &format!("{base}/zen/go/v1/usage"),
            &format!("{base}/console/api/go/status"),
        )
        .unwrap();
        assert_eq!(usage.session.percentage, 9.0);
        assert_eq!(usage.weekly.percentage, 69.0);
        requests.join().unwrap();
    }

    #[test]
    fn missing_opencode_credentials_are_reported_as_missing_login() {
        assert_eq!(
            poll_sources(
                None,
                None,
                "http://127.0.0.1:9/zen/go/v1/usage",
                "http://127.0.0.1:9/console/api/go/status"
            )
            .unwrap_err(),
            PollError::NoCredentials
        );
    }

    #[test]
    fn opencode_auth_file_supplies_the_go_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"google":{"type":"oauth","access":"x","refresh":"y"},"opencode-go":{"type":"api","key":"sk-auth-file-key"}}"#,
        )
        .unwrap();
        let credentials = read_api_key_from_auth_file(&path).unwrap();
        assert_eq!(credentials.api_key, "sk-auth-file-key");
        assert_eq!(credentials.source, path.display().to_string());

        std::fs::write(&path, r#"{"google":{"type":"oauth","access":"x"}}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, r#"{"opencode-go":{"type":"api","key":"   "}}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, "not json").unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());
    }

    #[test]
    fn config_api_key_is_accepted_and_header_injection_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"apiKey":"sk-config-key"}"#).unwrap();
        assert_eq!(
            read_api_key_from_config(&path).unwrap().api_key,
            "sk-config-key"
        );

        std::fs::write(&path, r#"{"apiKey":"sk-bad\r\nX-Test: injected"}"#).unwrap();
        assert!(read_api_key_from_config(&path).is_none());
        assert!(!valid_api_key("sk key with spaces"));
        assert!(!valid_api_key(""));
        assert!(valid_api_key("sk-abc_123-XYZ"));
    }
    /// Live check against the real endpoint and the real on-disk key.
    /// Run with `cargo test live_opencode_api_key -- --ignored --nocapture`.
    #[test]
    #[ignore = "hits the live OpenCode API"]
    fn live_opencode_api_key_returns_real_numbers() {
        let credentials =
            read_api_key_credentials().expect("no OpenCode API key found on this machine");
        let usage =
            fetch_api_key_usage(&credentials, GO_USAGE_URL).expect("live usage request failed");
        println!("credential source: {}", credentials.source);
        for (name, window) in [
            ("rolling", usage.rolling.as_ref()),
            ("weekly", usage.weekly.as_ref()),
            ("monthly", usage.monthly.as_ref()),
        ] {
            match window {
                Some(window) => println!(
                    "{name}: {:.2}% used, resets at {:?}",
                    window.usage_percent, window.resets_at
                ),
                None => println!("{name}: not reported"),
            }
        }
        assert!(
            usage.rolling.is_some() || usage.weekly.is_some() || usage.monthly.is_some(),
            "live response carried no windows"
        );
    }
}
