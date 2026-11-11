//! Axum development server with WebSocket reloads after successful builds.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::Body,
    extract::{
        Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use percent_encoding::percent_decode_str;
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch as updates},
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
    build, html,
    options::{Options, ServeArgs},
    watch,
};

const RELOAD_CLIENT: &str = include_str!("reload.js");

/// Run a loopback-only server. Cargo and the blocking downloader remain on the
/// watcher thread, never on Tokio's async executor.
pub fn serve(args: ServeArgs) -> Result<()> {
    let options = Options::resolve_serve(&args)?;
    let directory = build::output_directory(&options)?;
    watch::validate(&options)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating development server runtime")?;
    runtime.block_on(serve_async(
        args,
        options.port,
        options.autoreload,
        directory,
    ))
}

async fn serve_async(
    args: ServeArgs,
    port: u16,
    autoreload: bool,
    directory: PathBuf,
) -> Result<()> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .with_context(|| format!("binding development server on port {port}"))?;
    let address = listener.local_addr()?;
    let state = Arc::new(ServerState::new(directory, address.port(), autoreload));
    let app = router(Arc::clone(&state));
    eprintln!(
        "Serving {} at http://{address} (Ctrl-C to stop)",
        state
            .root
            .read()
            .map_err(|_| anyhow!("server directory lock poisoned"))?
            .display()
    );

    let (sender, stopped) = oneshot::channel();
    std::thread::Builder::new()
        .name("sycamore-watch".into())
        .spawn(move || {
            let result = watch::watch_with(&args.build, |directory| state.built(directory));
            let _ = sender.send(result);
        })
        .context("starting build watcher")?;

    tokio::select! {
        result = async { axum::serve(listener, app).await } => result.context("serving HTTP"),
        result = stopped => result.context("build watcher stopped unexpectedly")?.context("build watcher stopped"),
        result = tokio::signal::ctrl_c() => {
            result.context("listening for Ctrl-C")?;
            Ok(())
        }
    }
}

struct ServerState {
    root: RwLock<PathBuf>,
    revision: updates::Sender<u64>,
    session: String,
    port: u16,
    autoreload: bool,
}

impl ServerState {
    fn new(directory: PathBuf, port: u16, autoreload: bool) -> Self {
        let (revision, _) = updates::channel(0);
        Self {
            root: RwLock::new(directory),
            revision,
            // A new token after server restarts makes reconnecting pages reload.
            session: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            port,
            autoreload,
        }
    }

    fn version(&self, revision: u64) -> String {
        format!("{}-{revision}", self.session)
    }

    fn built(&self, directory: &Path) -> Result<()> {
        let mut root = self
            .root
            .write()
            .map_err(|_| anyhow!("server directory lock poisoned"))?;
        if *root != directory {
            *root = directory.to_owned();
            eprintln!("Serving updated output directory: {}", root.display());
        }
        // Called only after a successful build, including HTML generation.
        // Keep this under the root lock so HTTP snapshots see matching versions.
        // send_modify retains the latest revision even with no connected browsers.
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        Ok(())
    }
}

fn router(state: Arc<ServerState>) -> Router {
    let app = Router::new();
    let app = if state.autoreload {
        app.route("/__sycamore/reload", get(reload))
            .route("/__sycamore/reload.js", get(reload_client))
    } else {
        app
    };
    app.fallback(static_file)
        .layer(middleware::from_fn_with_state(Arc::clone(&state), guard))
        .with_state(state)
}

// Restrict Host as well as the listener address to prevent DNS rebinding.
fn allowed_host(headers: &HeaderMap, port: u16) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    [format!("127.0.0.1:{port}"), format!("localhost:{port}")]
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
        || (port == 80 && (host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1"))
}

fn allowed_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    // Browser origins have no path. Require the same authority as the page,
    // not a wildcard localhost origin or another local port.
    origin.eq_ignore_ascii_case(&format!("http://{host}"))
}

async fn guard(State(state): State<Arc<ServerState>>, request: Request, next: Next) -> Response {
    if !allowed_host(request.headers(), state.port) {
        return StatusCode::MISDIRECTED_REQUEST.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn reload_client() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        RELOAD_CLIENT,
    )
}

async fn reload(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !allowed_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let revisions = state.revision.subscribe();
    upgrade
        .max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |socket| reload_socket(socket, state, revisions))
}

async fn reload_socket(
    mut socket: WebSocket,
    state: Arc<ServerState>,
    mut revisions: updates::Receiver<u64>,
) {
    let version = state.version(*revisions.borrow_and_update());
    if socket.send(Message::Text(version.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            changed = revisions.changed() => {
                if changed.is_err() { break; }
                let version = state.version(*revisions.borrow_and_update());
                if socket.send(Message::Text(version.into())).await.is_err() { break; }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    // Reading also lets Axum handle WebSocket ping/pong frames.
                    _ => {}
                }
            }
        }
    }
}

async fn static_file(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response();
    }
    // Capture the revision before reading HTML. A build between this snapshot
    // and the WebSocket handshake is detected by the client's initial version.
    let (directory, version) = match state.root.read() {
        Ok(root) => (root.clone(), state.version(*state.revision.borrow())),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let path = match resolve_file(&directory, request.uri().path()).await {
        Ok(path) => path,
        Err(status) => return status.into_response(),
    };
    if state.autoreload && mime_guess::from_path(&path).first_raw() == Some("text/html") {
        let template = match tokio::fs::read_to_string(&path).await {
            Ok(template) => template,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let html = match html::with_reload_client(&template, &version) {
            Ok(html) => html,
            Err(error) => {
                eprintln!("error: injecting reload client: {error:#}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        let length = html.len();
        let mut response = Html(html).into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        if request.method() == Method::HEAD {
            *response.body_mut() = Body::empty();
        }
        return response;
    }
    // tower-http handles streaming, content types (including application/wasm),
    // HEAD and byte ranges. Use the checked canonical file, not an unchecked URL.
    let response = ServeFile::new(path)
        .oneshot(request)
        .await
        .expect("ServeFile is infallible");
    response.map(Body::new)
}

/// Decode once, reject traversal (including Windows paths), then enforce the
/// canonical boundary to prevent symlinks from exposing files outside dist.
async fn resolve_file(root: &Path, url: &str) -> std::result::Result<PathBuf, StatusCode> {
    let decoded = percent_decode_str(url)
        .decode_utf8()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if !decoded.starts_with('/') || decoded.contains(['\\', ':', '\0']) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let relative = Path::new(decoded.trim_start_matches('/'));
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut path = root.join(relative);
    if tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        path.push("index.html");
    }
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !path.starts_with(&root) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use futures_util::StreamExt;
    use std::{fs, time::Duration};
    use tokio::time::timeout;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Error as WsError, client::IntoClientRequest},
    };

    async fn request(app: &Router, method: Method, uri: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn serves_html_with_dev_client_without_modifying_disk() {
        let temp = tempfile::tempdir().unwrap();
        let original = "<!doctype html><title>App</title><main>Hello</main>";
        fs::write(temp.path().join("index.html"), original).unwrap();
        let state = Arc::new(ServerState::new(temp.path().to_owned(), 8080, true));
        let version = state.version(0);
        let app = router(state);
        let response = request(&app, Method::GET, "/?cache=1").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let length = response.headers()[header::CONTENT_LENGTH].clone();
        let html = text(response).await;
        assert!(html.contains("Hello"));
        assert!(html.contains("src=\"/__sycamore/reload.js\""));
        assert!(html.contains(&format!("data-version=\"{version}\"")));
        assert_eq!(
            fs::read_to_string(temp.path().join("index.html")).unwrap(),
            original
        );
        let response = request(&app, Method::HEAD, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], length);
        assert!(
            to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );
        let response = request(&app, Method::GET, "/__sycamore/reload.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .contains("javascript")
        );
        let script = text(response).await;
        assert!(script.contains("new WebSocket"));
        assert!(script.contains("window.location.reload()"));
        assert!(script.contains("data-cargo-sycamore-reload"));
    }

    #[tokio::test]
    async fn disabling_autoreload_serves_unmodified_html_without_reload_endpoints() {
        let temp = tempfile::tempdir().unwrap();
        let original = "<main>Original output</main>";
        fs::write(temp.path().join("index.html"), original).unwrap();
        let state = Arc::new(ServerState::new(temp.path().to_owned(), 8080, false));
        let app = router(Arc::clone(&state));
        assert_eq!(text(request(&app, Method::GET, "/").await).await, original);
        for path in ["/__sycamore/reload", "/__sycamore/reload.js"] {
            assert_eq!(
                request(&app, Method::GET, path).await.status(),
                StatusCode::NOT_FOUND
            );
        }
        // Disabling browser reload must not disable successful-build output updates.
        let next = temp.path().join("next");
        fs::create_dir(&next).unwrap();
        fs::write(next.join("index.html"), "new output").unwrap();
        state.built(&next).unwrap();
        assert_eq!(
            text(request(&app, Method::GET, "/").await).await,
            "new output"
        );
    }

    #[tokio::test]
    async fn streams_assets_with_types_ranges_and_head() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("app_bg.wasm"), b"wasm").unwrap();
        fs::write(temp.path().join("my file.js"), "export default 1;").unwrap();
        let app = router(Arc::new(ServerState::new(
            temp.path().to_owned(),
            8080,
            true,
        )));
        let response = request(&app, Method::GET, "/app_bg.wasm").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
        assert_eq!(text(response).await, "wasm");
        let response = request(&app, Method::HEAD, "/app_bg.wasm").await;
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
        assert!(
            to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app_bg.wasm")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::RANGE, "bytes=0-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(text(response).await, "wa");
        assert_eq!(
            request(&app, Method::GET, "/my%20file.js").await.status(),
            StatusCode::OK
        );
        assert_eq!(
            request(&app, Method::GET, "/absent").await.status(),
            StatusCode::NOT_FOUND
        );
        let response = request(&app, Method::POST, "/").await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
    }

    #[tokio::test]
    async fn resolves_index_and_rejects_traversal() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("nested/index.html"), "nested").unwrap();
        assert_eq!(
            resolve_file(temp.path(), "/nested/").await.unwrap(),
            fs::canonicalize(temp.path().join("nested/index.html")).unwrap()
        );
        for url in [
            "/../secret",
            "/%2e%2e/secret",
            "/nested/%2e%2e/secret",
            "/%2e%2e%2fsecret",
        ] {
            assert_eq!(
                resolve_file(temp.path(), url).await,
                Err(StatusCode::FORBIDDEN),
                "{url}"
            );
        }
        for url in ["/C:/secret", "/..%5csecret", "/%00", "/%ff", "relative"] {
            assert_eq!(
                resolve_file(temp.path(), url).await,
                Err(StatusCode::BAD_REQUEST),
                "{url}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("dist");
        fs::create_dir(&root).unwrap();
        fs::write(temp.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(temp.path().join("secret"), root.join("escape")).unwrap();
        assert_eq!(
            resolve_file(&root, "/escape").await,
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn validates_loopback_hosts_and_same_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        assert!(allowed_host(&headers, 8080));
        assert!(!allowed_origin(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8080"),
        );
        assert!(allowed_origin(&headers));
        for origin in [
            "http://evil.example",
            "http://127.0.0.1:9000",
            "null",
            "http://127.0.0.1:8080/evil",
        ] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(!allowed_origin(&headers), "{origin}");
        }
        for host in [
            "evil.example:8080",
            "localhost.evil.example:8080",
            "127.0.0.1:9000",
            "evil@localhost:8080",
        ] {
            headers.insert(header::HOST, host.parse().unwrap());
            assert!(!allowed_host(&headers, 8080), "{host}");
        }
        headers.insert(header::HOST, HeaderValue::from_static("localhost:8080"));
        assert!(allowed_host(&headers, 8080));
    }

    #[tokio::test]
    async fn rejects_untrusted_http_host() {
        let temp = tempfile::tempdir().unwrap();
        let app = router(Arc::new(ServerState::new(
            temp.path().to_owned(),
            8080,
            true,
        )));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "evil.example:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    fn websocket_request(
        address: SocketAddr,
        origin: Option<&str>,
    ) -> tokio_tungstenite::tungstenite::http::Request<()> {
        let mut request = format!("ws://{address}/__sycamore/reload")
            .into_client_request()
            .unwrap();
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("Origin", origin.parse().unwrap());
        }
        request
    }

    #[tokio::test]
    async fn websocket_notifies_all_clients_and_retains_latest_version_on_reconnect() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("index.html"), "old output").unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("http://{address}");
        let state = Arc::new(ServerState::new(
            temp.path().to_owned(),
            address.port(),
            true,
        ));
        // Publish before anyone subscribes: the handshake must still get revision 1.
        state.built(temp.path()).unwrap();
        let app = router(Arc::clone(&state));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (mut one, _) = connect_async(websocket_request(address, Some(&origin)))
            .await
            .unwrap();
        let (mut two, _) = connect_async(websocket_request(address, Some(&origin)))
            .await
            .unwrap();
        for socket in [&mut one, &mut two] {
            let message = timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(message.into_text().unwrap(), state.version(1));
        }
        // No success notification means no reload (e.g. a failed build).
        assert!(
            timeout(Duration::from_millis(50), one.next())
                .await
                .is_err()
        );
        let next_root = temp.path().join("next");
        fs::create_dir(&next_root).unwrap();
        fs::write(next_root.join("index.html"), "new output").unwrap();
        state.built(&next_root).unwrap();
        for socket in [&mut one, &mut two] {
            let message = timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(message.into_text().unwrap(), state.version(2));
        }
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(
            client
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .contains("new output")
        );
        one.close(None).await.unwrap();
        let (mut reconnected, _) = connect_async(websocket_request(address, Some(&origin)))
            .await
            .unwrap();
        let message = timeout(Duration::from_secs(2), reconnected.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(message.into_text().unwrap(), state.version(2));
        for origin in [
            None,
            Some("http://evil.example"),
            Some("http://127.0.0.1:1"),
        ] {
            let result = connect_async(websocket_request(address, origin)).await;
            assert!(matches!(result, Err(WsError::Http(response)) if response.status() == 403));
        }
        server.abort();
    }
}
