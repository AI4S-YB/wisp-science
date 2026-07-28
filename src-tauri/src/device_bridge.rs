//! Experimental StickS3 LAN bridge.
//!
//! The listener is opt-in and binds one concrete IPv4 address. `/health` is
//! intentionally minimal; every other route requires a keyring-backed
//! pre-shared token. The action surface is a closed list and never enters the
//! Agent/tool execution path.

use crate::{desktop_lifecycle, device_hub::DeviceHub, AppState};
use async_trait::async_trait;
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use ring::{hmac, rand as ring_rand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock},
};
use tauri::{AppHandle, Manager, State as TauriState};
use tokio::{
    net::TcpListener,
    sync::{oneshot, Mutex},
    task::JoinHandle,
};
use wisp_store::{secrets::Secret, Store};

pub const DEFAULT_DEVICE_BRIDGE_PORT: u16 = 18_766;
const DEVICE_TOKEN_SECRET: &str = "sticks3_device_bridge_token";
const SETTING_ENABLED: &str = "device_bridge_enabled";
const SETTING_MODE: &str = "device_bridge_mode";
const SETTING_BIND_IPV4: &str = "device_bridge_bind_ipv4";
const SETTING_PORT: &str = "device_bridge_port";
const DEVICE_TOKEN_HEADER: &str = "x-wisp-device-token";
const ACTION_HISTORY_LIMIT: usize = 50;
const MAX_ACTION_ID_BYTES: usize = 160;

/// The bridge transport is explicit from v1 so adding the out-of-LAN relay
/// later does not overload LAN bind settings or silently change exposure.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceBridgeMode {
    #[default]
    Lan,
    Relay,
}

impl DeviceBridgeMode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "" | "lan" => Ok(Self::Lan),
            "relay" => Ok(Self::Relay),
            _ => Err("Unknown Device Bridge transport mode.".into()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Relay => "relay",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceBridgeConfig {
    pub bind_ipv4: Ipv4Addr,
    pub port: u16,
}

impl DeviceBridgeConfig {
    pub fn parse(bind_ipv4: &str, port: u32) -> Result<Self, String> {
        let bind_ipv4 = bind_ipv4
            .trim()
            .parse::<Ipv4Addr>()
            .map_err(|_| "Bind address must be a concrete IPv4 address.".to_string())?;
        if bind_ipv4.is_unspecified() {
            return Err("Device Bridge cannot bind 0.0.0.0; choose one specific IPv4 address.".into());
        }
        let port = u16::try_from(port)
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| "Device Bridge port must be between 1 and 65535.".to_string())?;
        Ok(Self { bind_ipv4, port })
    }

    fn socket_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(self.bind_ipv4, self.port))
    }

    fn url(&self) -> String {
        format!("http://{}:{}", self.bind_ipv4, self.port)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceBridgeServiceState {
    Stopped,
    Listening,
    Error,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceBridgeRuntimeStatus {
    pub state: DeviceBridgeServiceState,
    pub bind_ipv4: String,
    pub port: u16,
    pub url: Option<String>,
    pub detail: String,
}

impl Default for DeviceBridgeRuntimeStatus {
    fn default() -> Self {
        Self {
            state: DeviceBridgeServiceState::Stopped,
            bind_ipv4: String::new(),
            port: DEFAULT_DEVICE_BRIDGE_PORT,
            url: None,
            detail: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceBridgeSettingsStatus {
    pub enabled: bool,
    pub mode: DeviceBridgeMode,
    pub has_token: bool,
    #[serde(flatten)]
    pub runtime: DeviceBridgeRuntimeStatus,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceActionRecord {
    id: String,
    action: String,
    session_id: Option<String>,
    ok: bool,
    timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceActionRequest {
    id: String,
    action: String,
    #[serde(default)]
    session_id: Option<String>,
}

#[async_trait]
pub(crate) trait SessionFocus: Send + Sync {
    async fn focus_session(&self, session_id: &str) -> Result<(), String>;
}

struct TauriSessionFocus {
    app: AppHandle,
    store: Store,
}

#[async_trait]
impl SessionFocus for TauriSessionFocus {
    async fn focus_session(&self, session_id: &str) -> Result<(), String> {
        let project_id = self
            .store
            .frame_project_id(session_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Session does not exist.".to_string())?;
        if self
            .store
            .get_project(&project_id)
            .await
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err("Session project does not exist.".into());
        }
        let project_window = crate::project_commands::project_window_label(&project_id);
        let preferred = if self.app.get_webview_window(&project_window).is_some() {
            project_window.as_str()
        } else {
            "main"
        };
        desktop_lifecycle::activate_workspace_window(
            &self.app,
            preferred,
            Some(json!({
                "projectId": project_id,
                "sessionId": session_id,
            })),
        );
        Ok(())
    }
}

#[derive(Clone)]
struct HttpState {
    hub: Arc<DeviceHub>,
    token: Arc<StdRwLock<Option<Vec<u8>>>>,
    actions: Arc<StdMutex<VecDeque<DeviceActionRecord>>>,
    focus: Arc<dyn SessionFocus>,
}

struct RunningBridge {
    config: DeviceBridgeConfig,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

pub struct DeviceBridge {
    hub: Arc<DeviceHub>,
    token: Arc<StdRwLock<Option<Vec<u8>>>>,
    actions: Arc<StdMutex<VecDeque<DeviceActionRecord>>>,
    runtime: Mutex<Option<RunningBridge>>,
    status: Arc<StdMutex<DeviceBridgeRuntimeStatus>>,
}

impl DeviceBridge {
    pub fn new(hub: Arc<DeviceHub>) -> Self {
        Self {
            hub,
            token: Arc::new(StdRwLock::new(None)),
            actions: Arc::new(StdMutex::new(VecDeque::with_capacity(
                ACTION_HISTORY_LIMIT,
            ))),
            runtime: Mutex::new(None),
            status: Arc::new(StdMutex::new(DeviceBridgeRuntimeStatus::default())),
        }
    }

    pub fn status(&self) -> DeviceBridgeRuntimeStatus {
        self.status.lock().unwrap().clone()
    }

    pub async fn start(
        &self,
        config: DeviceBridgeConfig,
        token: String,
        focus: Arc<dyn SessionFocus>,
    ) -> Result<(), String> {
        if token.is_empty() {
            self.set_error(&config, "Device token is missing.");
            return Err("Device token is missing.".into());
        }
        let mut runtime = self.runtime.lock().await;
        if runtime
            .as_ref()
            .is_some_and(|running| running.config == config && !running.task.is_finished())
        {
            *self.token.write().unwrap() = Some(token.into_bytes());
            self.set_listening(&config);
            return Ok(());
        }
        self.stop_locked(&mut runtime).await;

        let listener = match TcpListener::bind(config.socket_addr()).await {
            Ok(listener) => listener,
            Err(error) => {
                let message = format!(
                    "Cannot listen on {}:{}: {error}",
                    config.bind_ipv4, config.port
                );
                self.set_error(&config, &message);
                return Err(message);
            }
        };
        *self.token.write().unwrap() = Some(token.into_bytes());
        let router = router(HttpState {
            hub: self.hub.clone(),
            token: self.token.clone(),
            actions: self.actions.clone(),
            focus,
        });
        let (shutdown, shutdown_rx) = oneshot::channel();
        let status = self.status.clone();
        let failed_config = config.clone();
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(error) = result {
                let mut current = status.lock().unwrap();
                current.state = DeviceBridgeServiceState::Error;
                current.bind_ipv4 = failed_config.bind_ipv4.to_string();
                current.port = failed_config.port;
                current.url = None;
                current.detail = format!("Device Bridge stopped unexpectedly: {error}");
                tracing::warn!(target: "wisp", error = %error, "device bridge server failed");
            }
        });
        *runtime = Some(RunningBridge {
            config: config.clone(),
            shutdown,
            task,
        });
        self.set_listening(&config);
        Ok(())
    }

    pub async fn stop(&self) {
        // Authentication is revoked before graceful shutdown starts, so an
        // already-connected client cannot race one final authorized request.
        *self.token.write().unwrap() = None;
        let mut runtime = self.runtime.lock().await;
        self.stop_locked(&mut runtime).await;
        *self.status.lock().unwrap() = DeviceBridgeRuntimeStatus::default();
    }

    pub fn update_token(&self, token: String) {
        *self.token.write().unwrap() = Some(token.into_bytes());
    }

    pub fn revoke_token(&self) {
        *self.token.write().unwrap() = None;
    }

    fn set_listening(&self, config: &DeviceBridgeConfig) {
        *self.status.lock().unwrap() = DeviceBridgeRuntimeStatus {
            state: DeviceBridgeServiceState::Listening,
            bind_ipv4: config.bind_ipv4.to_string(),
            port: config.port,
            url: Some(config.url()),
            detail: String::new(),
        };
    }

    fn set_error(&self, config: &DeviceBridgeConfig, detail: &str) {
        *self.status.lock().unwrap() = DeviceBridgeRuntimeStatus {
            state: DeviceBridgeServiceState::Error,
            bind_ipv4: config.bind_ipv4.to_string(),
            port: config.port,
            url: None,
            detail: detail.to_string(),
        };
    }

    fn set_startup_error(&self, detail: &str) {
        *self.status.lock().unwrap() = DeviceBridgeRuntimeStatus {
            state: DeviceBridgeServiceState::Error,
            detail: detail.to_string(),
            ..DeviceBridgeRuntimeStatus::default()
        };
    }

    async fn stop_locked(&self, runtime: &mut Option<RunningBridge>) {
        if let Some(running) = runtime.take() {
            let _ = running.shutdown.send(());
            let mut task = running.task;
            if tokio::time::timeout(std::time::Duration::from_secs(3), &mut task)
                .await
                .is_err()
            {
                tracing::warn!(target: "wisp", "device bridge did not stop within three seconds");
                task.abort();
                let _ = task.await;
            }
        }
    }
}

fn router(state: HttpState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/state", get(get_state))
        .route("/action", post(post_action))
        .route("/actions", get(get_actions))
        .layer(DefaultBodyLimit::max(4 * 1024))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "wisp-device-bridge",
        "protocol": 1,
    }))
}

async fn get_state(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    Json(state.hub.snapshot()).into_response()
}

async fn get_actions(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    let actions = state.actions.lock().unwrap().clone();
    Json(json!({ "actions": actions })).into_response()
}

async fn post_action(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DeviceActionRequest>,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    if request.id.trim().is_empty() || request.id.len() > MAX_ACTION_ID_BYTES {
        return action_error(
            &state,
            &request,
            StatusCode::BAD_REQUEST,
            "Action id is required and must be short.",
        );
    }

    let result = match request.action.as_str() {
        "ping" => {
            tracing::debug!(target: "wisp", action_id = %request.id, "StickS3 ping");
            Ok(json!({
                "ok": true,
                "id": request.id,
                "action": "ping",
            }))
        }
        "focus_session" => match request.session_id.as_deref().filter(|id| !id.trim().is_empty()) {
            Some(session_id) => state
                .focus
                .focus_session(session_id)
                .await
                .map(|_| {
                    json!({
                        "ok": true,
                        "id": request.id,
                        "action": "focus_session",
                        "sessionId": session_id,
                    })
                }),
            None => Err("focus_session requires sessionId.".into()),
        },
        "acknowledge" => {
            let acknowledged = state.hub.acknowledge(request.session_id.as_deref());
            Ok(json!({
                "ok": true,
                "id": request.id,
                "action": "acknowledge",
                "acknowledged": acknowledged,
            }))
        }
        _ => {
            return action_error(
                &state,
                &request,
                StatusCode::BAD_REQUEST,
                "Unknown device action.",
            )
        }
    };

    match result {
        Ok(value) => {
            record_action(&state, &request, true, None);
            Json(value).into_response()
        }
        Err(error) => action_error(&state, &request, StatusCode::BAD_REQUEST, &error),
    }
}

fn authorized(state: &HttpState, headers: &HeaderMap) -> bool {
    let supplied = headers
        .get(DEVICE_TOKEN_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    let expected = state.token.read().unwrap();
    expected.as_deref().is_some_and(|expected| {
        // Compare fixed-size MACs rather than the tokens themselves. ring's
        // verifier performs the tag comparison in constant time.
        const CHALLENGE: &[u8] = b"wisp-device-bridge-token-check-v1";
        let expected_key = hmac::Key::new(hmac::HMAC_SHA256, expected);
        let supplied_key = hmac::Key::new(hmac::HMAC_SHA256, supplied);
        let expected_tag = hmac::sign(&expected_key, CHALLENGE);
        hmac::verify(&supplied_key, CHALLENGE, expected_tag.as_ref()).is_ok()
    })
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "ok": false, "error": "Unauthorized" })),
    )
        .into_response()
}

fn action_error(
    state: &HttpState,
    request: &DeviceActionRequest,
    status: StatusCode,
    error: &str,
) -> Response {
    record_action(state, request, false, Some(error));
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

fn record_action(
    state: &HttpState,
    request: &DeviceActionRequest,
    ok: bool,
    error: Option<&str>,
) {
    let mut actions = state.actions.lock().unwrap();
    if actions.len() == ACTION_HISTORY_LIMIT {
        actions.pop_front();
    }
    actions.push_back(DeviceActionRecord {
        id: request.id.clone(),
        action: request.action.clone(),
        session_id: request.session_id.clone(),
        ok,
        timestamp: chrono::Utc::now().timestamp(),
        error: error.map(str::to_string),
    });
}

pub fn generate_device_token() -> Result<String, String> {
    use ring_rand::SecureRandom;
    let random = ring_rand::SystemRandom::new();
    let mut bytes = [0_u8; 32];
    random
        .fill(&mut bytes)
        .map_err(|_| "Could not generate a secure device token.".to_string())?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

async fn get_setting(store: &Store, key: &str) -> String {
    store
        .get_setting(key)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

async fn load_token() -> Option<String> {
    tokio::task::spawn_blocking(|| Secret::get(DEVICE_TOKEN_SECRET).ok())
        .await
        .ok()
        .flatten()
}

async fn store_token(token: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || Secret::set(DEVICE_TOKEN_SECRET, &token))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

async fn delete_token() {
    let _ = tokio::task::spawn_blocking(|| Secret::delete(DEVICE_TOKEN_SECRET)).await;
}

async fn load_config(store: &Store) -> Result<DeviceBridgeConfig, String> {
    let bind_ipv4 = get_setting(store, SETTING_BIND_IPV4).await;
    let port = get_setting(store, SETTING_PORT)
        .await
        .parse::<u32>()
        .unwrap_or(u32::from(DEFAULT_DEVICE_BRIDGE_PORT));
    DeviceBridgeConfig::parse(&bind_ipv4, port)
}

fn production_focus(app: &AppHandle, store: &Store) -> Arc<dyn SessionFocus> {
    Arc::new(TauriSessionFocus {
        app: app.clone(),
        store: store.clone(),
    })
}

pub async fn autostart(app: AppHandle) {
    let state = app.state::<AppState>();
    if get_setting(&state.store, SETTING_ENABLED).await != "true" {
        return;
    }
    let mode = match DeviceBridgeMode::parse(&get_setting(&state.store, SETTING_MODE).await) {
        Ok(mode) => mode,
        Err(error) => {
            state.device_bridge.set_startup_error(&error);
            tracing::warn!(target: "wisp", %error, "device bridge transport mode is invalid");
            return;
        }
    };
    if mode == DeviceBridgeMode::Relay {
        let error = "Device Bridge relay transport is not available in this release.";
        state.device_bridge.set_startup_error(error);
        tracing::warn!(target: "wisp", "{error}");
        return;
    }
    let config = match load_config(&state.store).await {
        Ok(config) => config,
        Err(error) => {
            state.device_bridge.set_startup_error(&error);
            tracing::warn!(target: "wisp", %error, "device bridge configuration is invalid");
            return;
        }
    };
    let token = match load_token().await {
        Some(token) => token,
        None => match generate_device_token() {
            Ok(token) => {
                if let Err(error) = store_token(token.clone()).await {
                    state.device_bridge.set_startup_error(&error);
                    tracing::warn!(target: "wisp", %error, "could not store device bridge token");
                    return;
                }
                token
            }
            Err(error) => {
                state.device_bridge.set_startup_error(&error);
                tracing::warn!(target: "wisp", %error, "could not generate device bridge token");
                return;
            }
        },
    };
    let focus = production_focus(&app, &state.store);
    if let Err(error) = state.device_bridge.start(config, token, focus).await {
        // Opt-in network failures are visible in Settings but never fail app
        // startup or affect the loopback-only Browser Bridge.
        tracing::warn!(target: "wisp", %error, "device bridge did not start");
    }
}

pub async fn settings_status(state: &AppState) -> DeviceBridgeSettingsStatus {
    let enabled = get_setting(&state.store, SETTING_ENABLED).await == "true";
    let mode = DeviceBridgeMode::parse(&get_setting(&state.store, SETTING_MODE).await)
        .unwrap_or_default();
    let has_token = load_token().await.is_some();
    let mut runtime = state.device_bridge.status();
    if runtime.bind_ipv4.is_empty() {
        runtime.bind_ipv4 = get_setting(&state.store, SETTING_BIND_IPV4).await;
        runtime.port = get_setting(&state.store, SETTING_PORT)
            .await
            .parse()
            .unwrap_or(DEFAULT_DEVICE_BRIDGE_PORT);
    }
    DeviceBridgeSettingsStatus {
        enabled,
        mode,
        has_token,
        runtime,
    }
}

#[tauri::command]
pub(crate) async fn set_device_bridge(
    app: AppHandle,
    state: TauriState<'_, AppState>,
    enabled: bool,
    mode: String,
    bind_ipv4: String,
    port: u32,
) -> Result<DeviceBridgeSettingsStatus, String> {
    let mode = DeviceBridgeMode::parse(&mode)?;
    if mode == DeviceBridgeMode::Relay {
        return Err(
            "Relay mode is reserved for a later phase and is not available yet.".into(),
        );
    }
    let config = DeviceBridgeConfig::parse(&bind_ipv4, port)?;
    state
        .store
        .set_setting(SETTING_MODE, mode.as_str())
        .await
        .map_err(|error| error.to_string())?;
    state
        .store
        .set_setting(SETTING_BIND_IPV4, &config.bind_ipv4.to_string())
        .await
        .map_err(|error| error.to_string())?;
    state
        .store
        .set_setting(SETTING_PORT, &config.port.to_string())
        .await
        .map_err(|error| error.to_string())?;
    state
        .store
        .set_setting(SETTING_ENABLED, if enabled { "true" } else { "false" })
        .await
        .map_err(|error| error.to_string())?;

    if !enabled {
        state.device_bridge.stop().await;
        delete_token().await;
        return Ok(settings_status(&state).await);
    }

    let token = match load_token().await {
        Some(token) => token,
        None => {
            let token = generate_device_token()?;
            store_token(token.clone()).await?;
            token
        }
    };
    state
        .device_bridge
        .start(config, token, production_focus(&app, &state.store))
        .await?;
    Ok(settings_status(&state).await)
}

#[tauri::command]
pub(crate) async fn get_device_bridge_token() -> Result<String, String> {
    load_token()
        .await
        .ok_or_else(|| "No Device Bridge token exists. Generate one first.".into())
}

#[tauri::command]
pub(crate) async fn rotate_device_bridge_token(
    state: TauriState<'_, AppState>,
) -> Result<String, String> {
    let token = generate_device_token()?;
    store_token(token.clone()).await?;
    state.device_bridge.update_token(token.clone());
    Ok(token)
}

#[tauri::command]
pub(crate) async fn revoke_device_bridge_token(
    state: TauriState<'_, AppState>,
) -> Result<(), String> {
    state.device_bridge.revoke_token();
    delete_token().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeFocus {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl SessionFocus for FakeFocus {
        async fn focus_session(&self, session_id: &str) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match session_id {
                "valid" => Ok(()),
                "orphan" => Err("Session project does not exist.".into()),
                _ => Err("Session does not exist.".into()),
            }
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn test_config(port: u16) -> DeviceBridgeConfig {
        DeviceBridgeConfig::parse("127.0.0.1", u32::from(port)).unwrap()
    }

    async fn start_test_bridge(
    ) -> (Arc<DeviceBridge>, Arc<FakeFocus>, String, String, u16) {
        let hub = Arc::new(DeviceHub::default());
        hub.mark_working("frame-a", Some("project-a"));
        let bridge = Arc::new(DeviceBridge::new(hub));
        let focus = Arc::new(FakeFocus::default());
        let token = "test-token-that-is-not-logged".to_string();
        let port = free_port();
        bridge
            .start(test_config(port), token.clone(), focus.clone())
            .await
            .unwrap();
        (
            bridge,
            focus,
            token,
            format!("http://127.0.0.1:{port}"),
            port,
        )
    }

    #[test]
    fn rejects_unspecified_bind_address_and_invalid_ports() {
        assert_eq!(DeviceBridgeMode::parse("").unwrap(), DeviceBridgeMode::Lan);
        assert_eq!(
            DeviceBridgeMode::parse("relay").unwrap(),
            DeviceBridgeMode::Relay
        );
        assert!(DeviceBridgeMode::parse("automatic").is_err());
        assert!(DeviceBridgeConfig::parse("0.0.0.0", 18_766)
            .unwrap_err()
            .contains("0.0.0.0"));
        assert!(DeviceBridgeConfig::parse("127.0.0.1", 0).is_err());
        assert!(DeviceBridgeConfig::parse("127.0.0.1", 65_536).is_err());
    }

    #[test]
    fn generated_tokens_are_high_entropy_and_distinct() {
        let first = generate_device_token().unwrap();
        let second = generate_device_token().unwrap();
        assert!(first.len() >= 40);
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn health_is_public_but_state_requires_the_exact_token() {
        let (bridge, _, token, url, _) = start_test_bridge().await;
        let client = reqwest::Client::new();
        let health: Value = client
            .get(format!("{url}/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            health,
            json!({"ok": true, "service": "wisp-device-bridge", "protocol": 1})
        );
        assert_eq!(
            client
                .get(format!("{url}/state"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{url}/state"))
                .header("X-Wisp-Device-Token", "wrong")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let state: Value = client
            .get(format!("{url}/state"))
            .header("X-Wisp-Device-Token", token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["type"], "pet_state");
        assert_eq!(state["state"], "working");
        assert_eq!(state["sessionId"], "frame-a");
        assert!(state.get("seq").is_some());
        bridge.stop().await;
    }

    #[tokio::test]
    async fn action_surface_is_bounded_and_ping_has_no_focus_side_effect() {
        let (bridge, focus, token, url, _) = start_test_bridge().await;
        let client = reqwest::Client::new();
        let unknown = client
            .post(format!("{url}/action"))
            .header("X-Wisp-Device-Token", &token)
            .json(&json!({"id": "bad", "action": "run_shell"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

        for index in 0..55 {
            let response = client
                .post(format!("{url}/action"))
                .header("X-Wisp-Device-Token", &token)
                .json(&json!({"id": format!("ping-{index}"), "action": "ping"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(focus.calls.load(Ordering::SeqCst), 0);
        let history: Value = client
            .get(format!("{url}/actions"))
            .header("X-Wisp-Device-Token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(history["actions"].as_array().unwrap().len(), ACTION_HISTORY_LIMIT);
        bridge.stop().await;
    }

    #[tokio::test]
    async fn focus_session_rejects_missing_and_orphaned_sessions() {
        let (bridge, focus, token, url, _) = start_test_bridge().await;
        let client = reqwest::Client::new();
        for session_id in ["missing", "orphan"] {
            let response = client
                .post(format!("{url}/action"))
                .header("X-Wisp-Device-Token", &token)
                .json(&json!({
                    "id": format!("focus-{session_id}"),
                    "action": "focus_session",
                    "sessionId": session_id,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let valid = client
            .post(format!("{url}/action"))
            .header("X-Wisp-Device-Token", &token)
            .json(&json!({
                "id": "focus-valid",
                "action": "focus_session",
                "sessionId": "valid",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(valid.status(), StatusCode::OK);
        assert_eq!(focus.calls.load(Ordering::SeqCst), 3);
        bridge.stop().await;
    }

    #[tokio::test]
    async fn repeated_start_reuses_one_listener_and_stop_releases_the_port() {
        let (bridge, focus, token, url, port) = start_test_bridge().await;
        bridge
            .start(test_config(port), token.clone(), focus)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!("{url}/state"))
            .header("X-Wisp-Device-Token", token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        bridge.stop().await;
        let rebound = TcpListener::bind(("127.0.0.1", port)).await;
        assert!(rebound.is_ok(), "listener port was not released");
    }
}
