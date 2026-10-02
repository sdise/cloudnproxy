//! HTTP 服务：静态界面 + JSON 命令接口 + SSE 事件流。
//!
//! 接口与 Tauri 侧的命令一一对应，因此前端只需要换掉传输层：
//!
//! | Tauri                    | Web 控制台                     |
//! |--------------------------|-------------------------------|
//! | `invoke(cmd, args)`      | `POST /api/rpc/<cmd>`          |
//! | `listen(name, cb)`       | `GET /api/events`（SSE）        |
//!
//! 鉴权：除 `/api/auth/*` 与静态资源外，全部要求 `Authorization: Bearer <JWT>`；
//! 处于「必须先改密」状态时，除改密接口外一律返回 403。

use crate::assets;
use crate::auth::Authenticator;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use t5_core::{Config, Controller};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;

/// 界面静态资源一律不缓存，升级后刷新即生效。
const NO_STORE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-store");

#[derive(Clone)]
pub struct WebApp {
    pub ctrl: Controller,
    pub auth: Arc<Authenticator>,
}

/// 按配置启动 Web 控制台。
///
/// - 未启用时返回 `Ok(None)`，不产生任何副作用；
/// - 启用时完成监听后再返回，端口占用之类的错误因此能在启动阶段暴露。
pub async fn start(ctrl: Controller) -> Result<Option<tokio::task::JoinHandle<()>>, String> {
    let (enabled, listen, exposed) = {
        let cfg = ctrl.config().await;
        (
            cfg.web.enabled,
            cfg.web.listen.clone(),
            cfg.web.exposed_to_network(),
        )
    };
    if !enabled {
        return Ok(None);
    }

    let (host, port) = {
        let cfg = ctrl.config().await;
        cfg.web.listen_parts()
    };

    let auth = Authenticator::init(ctrl.clone()).await?;
    let app = WebApp {
        ctrl: ctrl.clone(),
        auth,
    };
    let router = routes(app);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| format!("Web 控制台无法监听 {listen}：{e}"))?;

    ctrl.logs
        .info(format!("Web 控制台已启动：http://{listen}/"));
    if exposed {
        ctrl.logs
            .warn("Web 控制台监听在非回环地址，且当前为明文 HTTP：");
        ctrl.logs
            .warn("  密码与登录令牌在传输中未加密，请限制来源（防火墙 / 反向代理 + HTTPS）");
    }

    let logs = ctrl.logs.clone();
    Ok(Some(tokio::spawn(async move {
        let service = router.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, service).await {
            logs.error(format!("Web 控制台异常退出：{e}"));
        }
    })))
}

fn routes(app: WebApp) -> Router {
    let protected = Router::new()
        .route("/api/events", get(sse_events))
        .route("/api/rpc/{cmd}", post(rpc))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_auth));

    Router::new()
        .route("/", get(index_html))
        .route("/index.html", get(index_html))
        .route("/style.css", get(style_css))
        .route("/app.js", get(app_js))
        .route("/favicon.ico", get(no_content))
        .route("/api/auth/state", get(auth_state))
        .route("/api/auth/login", post(login))
        .route("/api/auth/change-password", post(change_password))
        .merge(protected)
        .with_state(app)
}

// ---------------- 静态资源 ----------------

async fn index_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8"), NO_STORE],
        assets::INDEX_HTML,
    )
}

async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8"), NO_STORE],
        assets::STYLE_CSS,
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            NO_STORE,
        ],
        assets::APP_JS,
    )
}

async fn no_content() -> StatusCode {
    StatusCode::NO_CONTENT
}

// ---------------- 认证接口 ----------------

/// 登录页需要的信息（无需鉴权）。
async fn auth_state(State(app): State<WebApp>) -> impl IntoResponse {
    let cfg = app.ctrl.config().await;
    Json(json!({
        "username": cfg.web.username,
        "version": t5_core::VERSION,
    }))
}

async fn login(
    State(app): State<WebApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if username.is_empty() || password.is_empty() {
        return err(StatusCode::BAD_REQUEST, "用户名与密码不能为空");
    }

    let ip = client_ip(peer, &headers);
    match app.auth.login(ip, &username, &password).await {
        Ok(outcome) => Json(outcome).into_response(),
        Err(message) => err(StatusCode::UNAUTHORIZED, &message),
    }
}

/// 修改密码。允许在「必须先改密」状态下调用，但令牌本身必须有效。
async fn change_password(
    State(app): State<WebApp>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(claims) = bearer_token(&headers).and_then(|t| app.auth.verify(&t)) else {
        return err(StatusCode::UNAUTHORIZED, "登录已过期，请重新登录");
    };

    let new_password = body
        .get("new_password")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match app.auth.change_password(&claims, new_password).await {
        Ok(token) => Json(json!({
            "token": token,
            "must_change_password": false,
        }))
        .into_response(),
        Err(message) => err(StatusCode::BAD_REQUEST, &message),
    }
}

/// 受保护路由的鉴权中间件。
async fn require_auth(State(app): State<WebApp>, mut req: Request, next: Next) -> Response {
    let Some(claims) = bearer_token(req.headers()).and_then(|t| app.auth.verify(&t)) else {
        return err(StatusCode::UNAUTHORIZED, "未登录或登录已过期");
    };

    if claims.pwd {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "请先修改初始密码",
                "code": "password_change_required",
            })),
        )
            .into_response();
    }

    req.extensions_mut().insert(claims);
    next.run(req).await
}

// ---------------- 事件流 ----------------

async fn sse_events(
    State(app): State<WebApp>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = app.ctrl.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(event) => {
            let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Some(Ok(SseEvent::default().data(data)))
        }
        // 前端某段时间没读走数据会收到 Lagged：跳过这些帧即可，
        // 统计是增量值，补发反而会造成误解。
        Err(_) => None,
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ---------------- 命令接口 ----------------

async fn rpc(
    State(app): State<WebApp>,
    Path(cmd): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    match dispatch(&app, &cmd, args).await {
        Ok(value) => Json(value).into_response(),
        Err(message) => err(StatusCode::BAD_REQUEST, &message),
    }
}

/// 与 `ui/app.js` 中 `invoke(...)` 的命令名一一对应。
async fn dispatch(app: &WebApp, cmd: &str, args: Value) -> Result<Value, String> {
    let ctrl = &app.ctrl;

    macro_rules! json_of {
        ($expr:expr) => {
            serde_json::to_value($expr).map_err(|e| e.to_string())
        };
    }

    match cmd {
        "get_status" => json_of!(ctrl.status().await),
        "get_config" => json_of!(ctrl.config().await),
        "apply_config" => {
            let raw = args.get("cfg").cloned().unwrap_or(Value::Null);
            let cfg: Config =
                serde_json::from_value(raw).map_err(|e| format!("配置格式无效：{e}"))?;
            json_of!(ctrl.apply_config(cfg).await?)
        }
        "start_proxy" => json_of!(ctrl.start().await?),
        "stop_proxy" => {
            ctrl.stop().await;
            Ok(Value::Null)
        }
        "get_logs" => json_of!(ctrl.logs_snapshot()),
        "clear_logs" => {
            ctrl.clear_logs();
            Ok(Value::Null)
        }
        "resolve_nodes" => {
            let domain = args
                .get("domain")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            json_of!(ctrl.resolve_nodes(domain).await?)
        }
        "benchmark_all" => {
            let only_missing = args
                .get("onlyMissing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let auto_pick = args
                .get("autoPick")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(json!(ctrl.benchmark_all(only_missing, auto_pick).await?))
        }
        "check_update" => json_of!(ctrl.check_update().await?),
        // 控制台跑在服务器上，「在浏览器中打开」应当由访问者自己的浏览器完成，
        // 前端在 Web 模式下改用 window.open，不走这里
        "open_url" => Err("Web 控制台请直接复制链接到浏览器访问".to_string()),
        "benchmark_one" => {
            let ip = require_str(&args, "ip")?;
            let port = opt_u16(&args, "port").unwrap_or(443);
            json_of!(ctrl.benchmark_one(&ip, port).await)
        }
        "set_current_node" => {
            let ip = require_str(&args, "ip")?;
            let port = opt_u16(&args, "port").unwrap_or(443);
            ctrl.set_current_node(&ip, port).await?;
            Ok(Value::Null)
        }
        "set_speed_url" => {
            let url = require_str(&args, "url")?;
            json_of!(ctrl.set_speed_url(&url).await?)
        }
        "remove_speed_url" => {
            let url = require_str(&args, "url")?;
            json_of!(ctrl.remove_speed_url(&url).await?)
        }
        "list_interfaces" => json_of!(ctrl.list_interfaces()),
        "set_egress_interface" => {
            let iface = args
                .get("iface")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            ctrl.set_egress_interface(&iface).await?;
            Ok(Value::Null)
        }
        "reset_data" => {
            ctrl.reset_data().await;
            Ok(Value::Null)
        }

        // 以下三项属于「宿主进程」的能力，Web 控制台没有对应的操作对象
        "is_autostart_enabled" => Ok(Value::Bool(false)),
        "set_autostart" => {
            Err("Web 控制台不支持设置开机自启，请在服务器上直接管理服务".to_string())
        }
        "open_config_dir" => Err("Web 控制台无法打开服务器上的目录".to_string()),

        other => Err(format!("未知命令：{other}")),
    }
}

// ---------------- 小工具 ----------------

fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 判断请求来源。
///
/// 只有当直连方是回环/私网地址（即前面是本机反向代理）时才采信
/// `X-Forwarded-For`；否则一律以 TCP 对端地址为准 —— 该头可被伪造，
/// 若无条件信任，攻击者只要每次换一个假 IP 就能绕过登录限速。
fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let direct = peer.ip();
    if is_private_or_loopback(direct) {
        if let Some(ip) = forwarded_ip(headers) {
            return ip;
        }
    }
    direct
}

fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            // X-Forwarded-For 形如 "client, proxy1, proxy2"，取最左一段
            if let Some(first) = value.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }
    None
}

fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
        }
    }
}

fn require_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn opt_u16(args: &Value, key: &str) -> Option<u16> {
    args.get(key).and_then(|v| v.as_u64()).map(|v| v as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn passes_through_public_peer_without_trusting_forwarded_header() {
        let peer: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        // 公网直连时，伪造的 XFF 应当被忽略
        let headers = headers_with("x-forwarded-for", "1.2.3.4");
        assert_eq!(client_ip(peer, &headers).to_string(), "203.0.113.7");
    }

    #[test]
    fn trusts_forwarded_header_from_loopback() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let headers = headers_with("x-forwarded-for", "1.2.3.4, 10.0.0.1");
        assert_eq!(client_ip(peer, &headers).to_string(), "1.2.3.4");
    }

    #[test]
    fn falls_back_to_peer_when_forwarded_is_garbage() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let headers = headers_with("x-forwarded-for", "not-an-ip");
        assert_eq!(client_ip(peer, &headers).to_string(), "127.0.0.1");
    }

    #[test]
    fn extracts_bearer_token() {
        let h = headers_with("authorization", "Bearer abc.def.ghi");
        assert_eq!(bearer_token(&h).as_deref(), Some("abc.def.ghi"));

        let h = headers_with("authorization", "Basic abc");
        assert!(bearer_token(&h).is_none());
    }
}
