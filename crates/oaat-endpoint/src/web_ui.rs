//! Mini status web UI for Tune Bridge.
//!
//! Serves a single-page HTML dashboard on port 9741 showing bridge status,
//! current audio device, available devices, connection state, and stream info.
//! Also exposes `POST /api/device` to switch the active audio device, and
//! `POST /api/release` to give up the audio output (RFC §7.3.1), for a host
//! that hands the DAC to another source.

use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};
use tracing::{error, info, warn};

/// Shared state exposed to the web UI.
#[derive(Debug, Clone)]
pub struct BridgeStatus {
    pub bridge_name: String,
    pub version: String,
    pub current_device: String,
    pub available_devices: Vec<String>,
    pub connected: bool,
    pub controller_name: Option<String>,
    pub stream_format: Option<String>,
    pub stream_sample_rate: Option<u32>,
    pub stream_bits: Option<u8>,
    pub stream_channels: Option<u8>,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    pub track_album: Option<String>,
    pub artwork_url: Option<String>,
    pub volume: u8,
    pub playing: bool,
}

impl Default for BridgeStatus {
    fn default() -> Self {
        Self {
            bridge_name: "Tune Bridge".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            current_device: String::new(),
            available_devices: Vec::new(),
            connected: false,
            controller_name: None,
            stream_format: None,
            stream_sample_rate: None,
            stream_bits: None,
            stream_channels: None,
            track_title: None,
            track_artist: None,
            track_album: None,
            artwork_url: None,
            volume: 100,
            playing: false,
        }
    }
}

/// Handle for updating bridge status from the main event loop.
#[derive(Clone)]
pub struct BridgeStatusHandle {
    inner: Arc<Mutex<BridgeStatus>>,
    notify: watch::Sender<()>,
}

impl BridgeStatusHandle {
    pub fn new(initial: BridgeStatus) -> (Self, BridgeStatusReader) {
        let inner = Arc::new(Mutex::new(initial));
        let (notify, rx) = watch::channel(());
        let handle = Self {
            inner: inner.clone(),
            notify,
        };
        let reader = BridgeStatusReader { inner, _rx: rx };
        (handle, reader)
    }

    pub async fn update<F: FnOnce(&mut BridgeStatus)>(&self, f: F) {
        let mut guard = self.inner.lock().await;
        f(&mut guard);
        let _ = self.notify.send(());
    }

    pub async fn snapshot(&self) -> BridgeStatus {
        self.inner.lock().await.clone()
    }
}

/// Read-only access to bridge status (for the web server).
#[derive(Clone)]
#[allow(dead_code)]
pub struct BridgeStatusReader {
    inner: Arc<Mutex<BridgeStatus>>,
    _rx: watch::Receiver<()>,
}

/// Channel for sending device-switch requests from the web UI to the main loop.
pub type DeviceSwitchTx = tokio::sync::mpsc::Sender<String>;
pub type DeviceSwitchRx = tokio::sync::mpsc::Receiver<String>;

/// State-файл авто-сгенерированного control_token. Устанавливается CLI при
/// старте; если не установлен — токен задан конфигом и регенерация через
/// `POST /api/token/regenerate` недоступна (409).
static TOKEN_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_token_file(path: std::path::PathBuf) {
    let _ = TOKEN_FILE.set(path);
}

/// Статический UI настроек endpoint'а (Обзор / Сеть / Spotify Connect),
/// встраивается в бинарь на этапе компиляции.
static INDEX_HTML: &str = include_str!("../assets/index.html");

/// JSON-ответ с заданным статус-кодом.
fn json_response(status: u16, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

/// Channel for release requests (`POST /api/release`) from the web UI to the
/// main loop.
pub type ReleaseTx = tokio::sync::mpsc::Sender<()>;
pub type ReleaseRx = tokio::sync::mpsc::Receiver<()>;

/// Start the web UI HTTP server on loopback. Returns immediately,
/// running in the background.
pub async fn start_web_ui(
    port: u16,
    status_handle: BridgeStatusHandle,
    device_switch_tx: DeviceSwitchTx,
) {
    start_web_ui_on(
        ([127, 0, 0, 1], port).into(),
        status_handle,
        device_switch_tx,
        None,
    )
    .await;
}

/// Like [`start_web_ui`], on a chosen address (e.g. `127.0.0.1` to keep the
/// controls local to the host), and with `POST /api/release` when `release_tx`
/// is given.
pub async fn start_web_ui_on(
    addr: SocketAddr,
    status_handle: BridgeStatusHandle,
    device_switch_tx: DeviceSwitchTx,
    release_tx: Option<ReleaseTx>,
) {
    start_web_ui_with_token(
        addr,
        status_handle,
        device_switch_tx,
        release_tx,
        None,
        Arc::new(Vec::new),
    )
    .await;
}

/// A shared token authenticates control requests, including on a LAN bind.
/// Status GETs remain read-only and do not require a token.
pub async fn start_web_ui_with_token(
    addr: SocketAddr,
    status_handle: BridgeStatusHandle,
    device_switch_tx: DeviceSwitchTx,
    release_tx: Option<ReleaseTx>,
    control_token: Option<String>,
    list_devices: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
) {
    let port = addr.port();
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => {
            info!(
                port = l.local_addr().map(|a| a.port()).unwrap_or(port),
                "web UI listening"
            );
            l
        }
        Err(e) => {
            error!(error = %e, port, "failed to start web UI server");
            return;
        }
    };
    serve_web_ui(
        listener,
        status_handle,
        device_switch_tx,
        release_tx,
        control_token,
        list_devices,
    );
}

/// Serve the web UI on an already-bound listener, in the background.
fn serve_web_ui(
    listener: TcpListener,
    status_handle: BridgeStatusHandle,
    device_switch_tx: DeviceSwitchTx,
    release_tx: Option<ReleaseTx>,
    control_token: Option<String>,
    list_devices: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
) {
    let status_handle = Arc::new(status_handle);
    let device_switch_tx = Arc::new(device_switch_tx);
    let release_tx = Arc::new(release_tx);
    let control_token = Arc::new(control_token);

    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "web UI accept error");
                    continue;
                }
            };
            let Ok(local) = stream.local_addr() else {
                continue;
            };
            let policy = ControlPolicy {
                local,
                peer,
                token: control_token.clone(),
            };
            let io = TokioIo::new(stream);
            let status = status_handle.clone();
            let switch_tx = device_switch_tx.clone();
            let release_tx = release_tx.clone();
            let list_devices = list_devices.clone();

            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let status = status.clone();
                    let switch_tx = switch_tx.clone();
                    let release_tx = release_tx.clone();
                    let policy = policy.clone();
                    let list_devices = list_devices.clone();
                    async move {
                        handle_request(req, &status, &switch_tx, &release_tx, &policy, &*list_devices).await
                    }
                });
                if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                    // Connection reset is normal for browsers
                    if !e.is_incomplete_message() {
                        warn!(error = %e, "web UI connection error");
                    }
                }
            });
        }
    });
}

#[derive(Clone)]
struct ControlPolicy {
    local: SocketAddr,
    peer: SocketAddr,
    token: Arc<Option<String>>,
}

impl ControlPolicy {
    fn authorize<B>(&self, req: &Request<B>) -> Result<(), StatusCode> {
        // Ok(None): header absent; Err: repeated or not valid UTF-8.
        let header = |name: &str| -> Result<Option<&str>, StatusCode> {
            let mut values = req.headers().get_all(name).iter();
            let Some(first) = values.next() else {
                return Ok(None);
            };
            if values.next().is_some() {
                return Err(StatusCode::FORBIDDEN);
            }
            first.to_str().map(Some).map_err(|_| StatusCode::FORBIDDEN)
        };
        let one_header = |name: &str| header(name).ok().flatten();
        // Browsers always send Origin on POST, so a missing Origin means a
        // non-browser client (e.g. a host controller calling the loopback
        // API): it cannot be driven by a web page, and still has to pass the
        // JSON, peer and token checks below. A present Origin must be the
        // page itself, on a Host that is this listener (DNS rebinding).
        if let Some(origin) = header("origin")? {
            self.check_same_origin(origin, header("host")?.ok_or(StatusCode::FORBIDDEN)?)?;
        }
        let json = one_header("content-type")
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
        if !json {
            return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }
        match self.token.as_deref() {
            Some(expected) => {
                let supplied = one_header("authorization")
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .ok_or(StatusCode::UNAUTHORIZED)?;
                if expected.is_empty() || !same_token(expected.as_bytes(), supplied.as_bytes()) {
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
            None if !self.peer.ip().is_loopback() => return Err(StatusCode::FORBIDDEN),
            None => {}
        }
        Ok(())
    }

    fn check_same_origin(&self, origin: &str, host: &str) -> Result<(), StatusCode> {
        let authority = self.local.to_string();
        let localhost = format!("localhost:{}", self.local.port());
        let default_port = self.local.port() == 80;
        let numeric_host = if default_port {
            authority.trim_end_matches(":80")
        } else {
            &authority
        };
        // Do not trust Host alone: an attacker can point their DNS at loopback.
        let local_name = self.local.ip().is_loopback()
            && (host == localhost || (default_port && host == "localhost"));
        if host != authority && host != numeric_host && !local_name {
            return Err(StatusCode::FORBIDDEN);
        }
        let origin_host = if default_port {
            host.trim_end_matches(":80")
        } else {
            host
        };
        if origin != format!("http://{origin_host}") && origin != format!("http://{host}") {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }
}

fn same_token(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    expected
        .iter()
        .zip(supplied)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

async fn handle_request<B>(
    req: Request<B>,
    status_handle: &BridgeStatusHandle,
    device_switch_tx: &DeviceSwitchTx,
    release_tx: &Option<ReleaseTx>,
    policy: &ControlPolicy,
    list_devices: &(dyn Fn() -> Vec<String> + Send + Sync),
) -> Result<Response<Full<Bytes>>, hyper::Error>
where
    B: hyper::body::Body<Data = Bytes>,
{
    if req.method() == Method::POST
        && matches!(
            req.uri().path(),
            "/api/device"
                | "/api/release"
                | "/api/token/regenerate"
                | "/api/update"
                | "/api/network/connect"
                | "/api/spotify/start"
                | "/api/spotify/stop"
        )
        && let Err(code) = policy.authorize(&req)
    {
        return Ok(Response::builder()
            .status(code)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from_static(
                b"{\"ok\":false,\"error\":\"control request rejected\"}",
            )))
            .unwrap());
    }
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/api/release") if release_tx.is_some() => {
            info!("web UI: release requested");
            let sent = match release_tx.as_ref() {
                Some(tx) => tx.send(()).await.is_ok(),
                None => false,
            };
            let (code, body) = if sent {
                (200, r#"{"ok":true}"#)
            } else {
                (503, r#"{"ok":false,"error":"endpoint not running"}"#)
            };
            Ok(Response::builder()
                .status(code)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(body)))
                .unwrap())
        }
        (&Method::POST, "/api/token/regenerate") => {
            // Регенерация авто-сгенерированного токена: удаляем state-файл и
            // выходим — супервизор (systemd/LaunchAgent) перезапустит процесс,
            // тот сгенерирует новый токен и опубликует его в mDNS TXT.
            let Some(path) = TOKEN_FILE.get() else {
                return Ok(Response::builder()
                    .status(409)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from_static(
                        b"{\"ok\":false,\"error\":\"token is set via config, regenerate unavailable\"}",
                    )))
                    .unwrap());
            };
            if let Err(e) = std::fs::remove_file(path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                warn!(error = %e, "failed to remove token state file");
            }
            info!("control_token regenerate requested; restarting endpoint");
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                std::process::exit(0);
            });
            Ok(Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from_static(
                    b"{\"ok\":true,\"restart\":true}",
                )))
                .unwrap())
        }
        (&Method::GET, "/") | (&Method::GET, "/index.html") => Ok(Response::builder()
            .status(200)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from_static(INDEX_HTML.as_bytes())))
            .unwrap()),
        (&Method::GET, "/api/devices") => {
            let devs = list_devices();
            let arr = devs
                .iter()
                .map(|d| format!(r#""{}""#, d.replace('"', r#"\""#)))
                .collect::<Vec<_>>()
                .join(",");
            let json = format!(r#"{{"devices":[{}]}}"#, arr);
            Ok(Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(json)))
                .unwrap())
        }
        (&Method::GET, "/api/status") => {
            let status = status_handle.snapshot().await;
            let json = render_status_json(&status);
            Ok(Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(json)))
                .unwrap())
        }
        (&Method::POST, "/api/device") => {
            // Read body to get device name
            use http_body_util::BodyExt;
            let body = req
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let device_name = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("device")?.as_str().map(str::to_owned));

            match device_name {
                Some(name) if !name.is_empty() => {
                    info!(device = %name, "web UI: device switch requested");
                    let _ = device_switch_tx.send(name.clone()).await;
                    Ok(Response::builder()
                        .status(200)
                        .header("Content-Type", "application/json")
                        .body(Full::new(Bytes::from(
                            serde_json::json!({"ok":true,"device":name}).to_string(),
                        )))
                        .unwrap())
                }
                _ => Ok(Response::builder()
                    .status(400)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(
                        r#"{"ok":false,"error":"missing device name"}"#,
                    )))
                    .unwrap()),
            }
        }
        (&Method::POST, "/api/update") => {
            // OTA (P2.10): тело — JSON {"b64": "<новый tune-bridge в base64>"}
            // (сервер SoundVault сам качает бинарь из GitHub-релизов; токен на
            // endpoint не передаётся, а JSON-обязательство ControlPolicy остаётся
            // нетронутым). Пишем временный файл рядом с текущим exe, переименовываем
            // запущенный в .old, новый — на его место и выходим: супервизор
            // (systemd/LaunchAgent) перезапускает процесс уже новой версией.
            use base64::Engine;
            use http_body_util::BodyExt;
            let body = req
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let bad = |err: &str| {
                Response::builder()
                    .status(400)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(
                        serde_json::json!({"ok": false, "error": err}).to_string(),
                    )))
                    .unwrap()
            };
            let Some(b64) = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("b64")?.as_str().map(str::to_owned))
            else {
                return Ok(bad(r#"expected {"b64": "..."}"#));
            };
            let Ok(bin) = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) else {
                return Ok(bad("invalid base64"));
            };
            if bin.len() < 1024 {
                return Ok(bad("payload too small"));
            }
            let result = (|| -> Result<(), String> {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let dir = exe.parent().ok_or("no exe dir")?;
                let tmp = dir.join("tune-bridge.update.tmp");
                std::fs::write(&tmp, &bin).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                        .map_err(|e| e.to_string())?;
                }
                let old = dir.join("tune-bridge.old");
                let _ = std::fs::remove_file(&old);
                // rename запущенного exe работает и на unix, и на Windows
                std::fs::rename(&exe, &old).map_err(|e| e.to_string())?;
                if let Err(e) = std::fs::rename(&tmp, &exe) {
                    let _ = std::fs::rename(&old, &exe); // rollback
                    return Err(e.to_string());
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    info!("web UI: binary updated, restarting");
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        std::process::exit(0);
                    });
                    Ok(Response::builder()
                        .status(200)
                        .header("Content-Type", "application/json")
                        .body(Full::new(Bytes::from_static(
                            b"{\"ok\":true,\"restarting\":true}",
                        )))
                        .unwrap())
                }
                Err(e) => Ok(Response::builder()
                    .status(500)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(
                        serde_json::json!({"ok": false, "error": e}).to_string(),
                    )))
                    .unwrap()),
            }
        }
        (&Method::GET, "/api/system") => {
            let body = tokio::task::spawn_blocking(|| {
                crate::sysinfo::system_json(&crate::sysinfo::collect())
            })
            .await
            .unwrap_or_else(|_| "{}".to_string());
            Ok(json_response(200, body))
        }
        (&Method::GET, "/api/network") => {
            let body = tokio::task::spawn_blocking(crate::network::network_json)
                .await
                .unwrap_or_else(|_| "{}".to_string());
            Ok(json_response(200, body))
        }
        (&Method::GET, "/api/spotify/status") => {
            let body = tokio::task::spawn_blocking(crate::spotify::spotify_status_json)
                .await
                .unwrap_or_else(|_| "{}".to_string());
            Ok(json_response(200, body))
        }
        (&Method::POST, "/api/network/connect") => {
            use http_body_util::BodyExt;
            let body = req
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let v = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default();
            let ssid = v
                .get("ssid")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let password = v
                .get("password")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if ssid.is_empty() {
                return Ok(json_response(
                    400,
                    r#"{"ok":false,"error":"missing ssid"}"#.to_string(),
                ));
            }
            info!("web UI: wi-fi connect requested");
            match tokio::task::spawn_blocking(move || {
                crate::network::connect_wifi(&ssid, &password)
            })
            .await
            {
                Ok(Ok(())) => Ok(json_response(200, r#"{"ok":true}"#.to_string())),
                Ok(Err(e)) => Ok(json_response(
                    400,
                    serde_json::json!({"ok": false, "error": e}).to_string(),
                )),
                Err(_) => Ok(json_response(
                    500,
                    r#"{"ok":false,"error":"task failed"}"#.to_string(),
                )),
            }
        }
        (&Method::POST, "/api/spotify/start") => {
            match tokio::task::spawn_blocking(crate::spotify::spotify_start).await {
                Ok(Ok(())) => Ok(json_response(200, r#"{"ok":true}"#.to_string())),
                Ok(Err(e)) => Ok(json_response(
                    400,
                    serde_json::json!({"ok": false, "error": e}).to_string(),
                )),
                Err(_) => Ok(json_response(
                    500,
                    r#"{"ok":false,"error":"task failed"}"#.to_string(),
                )),
            }
        }
        (&Method::POST, "/api/spotify/stop") => {
            match tokio::task::spawn_blocking(crate::spotify::spotify_stop).await {
                Ok(Ok(())) => Ok(json_response(200, r#"{"ok":true}"#.to_string())),
                Ok(Err(e)) => Ok(json_response(
                    400,
                    serde_json::json!({"ok": false, "error": e}).to_string(),
                )),
                Err(_) => Ok(json_response(
                    500,
                    r#"{"ok":false,"error":"task failed"}"#.to_string(),
                )),
            }
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap()),
    }
}

fn json_str(s: &Option<String>) -> String {
    match s {
        Some(v) => format!(r#""{}""#, v.replace('"', r#"\""#)),
        None => "null".into(),
    }
}

fn render_status_json(status: &BridgeStatus) -> String {
    let devices_json: Vec<String> = status
        .available_devices
        .iter()
        .map(|d| format!(r#""{}""#, d.replace('"', r#"\""#)))
        .collect();
    format!(
        r#"{{"bridge_name":"{}","version":"{}","os":"{}","arch":"{}","current_device":"{}","available_devices":[{}],"connected":{},"controller_name":{},"playing":{},"volume":{},"track_title":{},"track_artist":{},"track_album":{},"artwork_url":{},"stream_format":{},"stream_sample_rate":{},"stream_bits":{},"stream_channels":{}}}"#,
        status.bridge_name.replace('"', r#"\""#),
        status.version,
        std::env::consts::OS,
        std::env::consts::ARCH,
        status.current_device.replace('"', r#"\""#),
        devices_json.join(","),
        status.connected,
        json_str(&status.controller_name),
        status.playing,
        status.volume,
        json_str(&status.track_title),
        json_str(&status.track_artist),
        json_str(&status.track_album),
        json_str(&status.artwork_url),
        json_str(&status.stream_format),
        status.stream_sample_rate.unwrap_or(0),
        status.stream_bits.unwrap_or(0),
        status.stream_channels.unwrap_or(0),
    )
}


#[cfg(test)]
mod control_tests;
