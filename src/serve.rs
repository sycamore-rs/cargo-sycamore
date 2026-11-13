//! Axum development server with WebSocket reloads after successful builds.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
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
    backend::BackendSupervisor,
    build, html,
    options::{Options, ServeArgs},
    proxy, watch,
};

const RELOAD_CLIENT: &str = include_str!("reload.js");

/// Run a loopback-only server. Cargo and the blocking downloader remain on the
/// watcher thread, never on Tokio's async executor.
pub fn serve(args: ServeArgs) -> Result<()> {
    let options = Options::resolve_serve(&args)?;
    let directory = build::output_directory(&options)?.join("public");
    watch::validate(&options)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating development server runtime")?;
    runtime.block_on(serve_async(args, options, directory))
}

async fn serve_async(args: ServeArgs, options: Options, directory: PathBuf) -> Result<()> {
    let port = options.port;
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .with_context(|| format!("binding development server on port {port}"))?;
    let address = listener.local_addr()?;
    let mut state = ServerState::new(directory, address.port(), options.autoreload);
    state.spa = options.spa;
    state
        .published
        .store(false, std::sync::atomic::Ordering::SeqCst);
    *state
        .configuration
        .get_mut()
        .expect("new configuration lock") = Some(ServeConfiguration::from(&options));
    let state = Arc::new(state);
    let backend = Arc::new(Mutex::new(BackendSupervisor::new()));
    let backend_shutdown = Arc::clone(&backend);
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_watch = Arc::clone(&shutdown);
    let app = router(Arc::clone(&state));
    eprintln!(
        "Serving {} at http://{address}{} (Ctrl-C to stop)",
        state
            .root
            .read()
            .map_err(|_| anyhow!("server directory lock poisoned"))?
            .display(),
        options.public_url
    );

    let (sender, stopped) = oneshot::channel();
    let backend_monitor = Arc::clone(&backend);
    let monitoring_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("sycamore-watch".into())
        .spawn(move || {
            let resolve = || Options::resolve_serve(&args);
            let result = watch::watch_with_resolver(options, resolve, |event| match event {
                watch::BuildEvent::Started => {
                    state.set_phase(BuildPhase::Building);
                    Ok(())
                }
                watch::BuildEvent::Succeeded { output, options } => {
                    let mut backend = backend
                        .lock()
                        .map_err(|_| anyhow!("backend lock poisoned"))?;
                    if shutdown_watch.load(std::sync::atomic::Ordering::SeqCst) {
                        return Ok(());
                    }
                    // The public listener cannot change ports without restarting
                    // the command, even when the config is reloaded.
                    anyhow::ensure!(
                        options.backend_port != Some(state.port),
                        "backend-port must differ from the public development port"
                    );
                    if options.port != state.port && options.port != 0 {
                        eprintln!("warning: restart serve to change the public port");
                    }
                    let generation = snapshot_frontend(&output.directory.join("public"))?;
                    if let Some(executable) = &output.server {
                        let port = options.backend_port.ok_or_else(|| {
                            anyhow!("[serve].backend-port is required to run the backend")
                        })?;
                        backend.restart(executable, &output.directory, port)?;
                    } else {
                        backend.stop()?;
                    }
                    state.publish(generation, options)
                }
                watch::BuildEvent::Failed => {
                    state.set_phase(BuildPhase::Failed);
                    Ok(())
                }
            });
            let _ = sender.send(result);
        })
        .context("starting build watcher")?;

    let monitor = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            // Restart/readiness runs on the watcher thread. Never wait for its
            // blocking lock on an async worker.
            if let Ok(mut backend) = backend_monitor.try_lock()
                && let Err(error) = backend.check()
            {
                eprintln!("error: {error:#}\nWaiting for changes...");
                monitoring_state.set_phase(BuildPhase::Failed);
            }
        }
    });
    let result = tokio::select! {
        result = async { axum::serve(listener, app).await } => result.context("serving HTTP"),
        result = stopped => match result {
            Ok(result) => result.context("build watcher stopped"),
            Err(error) => Err(error).context("build watcher stopped unexpectedly"),
        },
        result = tokio::signal::ctrl_c() => result.context("listening for Ctrl-C"),
    };
    monitor.abort();
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    // No backend is left detached if the HTTP server, watcher, or signal exits.
    let stop = tokio::task::spawn_blocking(move || {
        backend_shutdown
            .lock()
            .map_err(|_| anyhow!("backend lock poisoned"))?
            .stop()
    })
    .await
    .context("joining backend shutdown")?;
    result.and(stop)
}

// Publish only complete successful generations. Subsequent builds may write
// into out-dir without replacing the currently served frontend.
fn snapshot_frontend(directory: &Path) -> Result<tempfile::TempDir> {
    let generation = tempfile::Builder::new()
        .prefix("cargo-sycamore-frontend-")
        .tempdir()?;
    crate::backend::copy_public_directory(directory, generation.path())?;
    Ok(generation)
}

#[derive(Clone, Debug)]
struct ServeConfiguration {
    serve_frontend: bool,
    backend_port: Option<u16>,
    forward_routes: Vec<String>,
    public_url: String,
    spa: bool,
    autoreload: bool,
}

impl From<&Options> for ServeConfiguration {
    fn from(options: &Options) -> Self {
        Self {
            serve_frontend: options.serve_frontend,
            backend_port: options.backend_port,
            forward_routes: options.forward_routes.clone(),
            public_url: options.public_url.clone(),
            spa: options.spa,
            autoreload: options.autoreload,
        }
    }
}

impl ServeConfiguration {
    fn forwards(&self, path: &str) -> bool {
        !self.serve_frontend
            || self
                .forward_routes
                .iter()
                .any(|route| proxy::matches_route(path, route))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildPhase {
    Building,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug)]
struct BuildState {
    revision: u64,
    phase: BuildPhase,
}

struct ServerState {
    root: RwLock<PathBuf>,
    revision: updates::Sender<BuildState>,
    session: String,
    port: u16,
    autoreload: bool,
    spa: bool,
    configuration: RwLock<Option<ServeConfiguration>>,
    generation: Mutex<Option<Arc<tempfile::TempDir>>>,
    published: std::sync::atomic::AtomicBool,
}

impl ServerState {
    fn new(directory: PathBuf, port: u16, autoreload: bool) -> Self {
        let (revision, _) = updates::channel(BuildState {
            revision: 0,
            phase: BuildPhase::Building,
        });
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
            spa: false,
            configuration: RwLock::new(None),
            generation: Mutex::new(None),
            published: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn configuration(&self) -> ServeConfiguration {
        self.configuration
            .read()
            .expect("configuration lock poisoned")
            .clone()
            .unwrap_or_else(|| ServeConfiguration {
                serve_frontend: true,
                backend_port: None,
                forward_routes: Vec::new(),
                public_url: "/".into(),
                spa: self.spa,
                autoreload: self.autoreload,
            })
    }

    fn publish(&self, generation: tempfile::TempDir, options: &Options) -> Result<()> {
        let mut previous = self
            .generation
            .lock()
            .map_err(|_| anyhow!("frontend generation lock poisoned"))?;
        *self
            .configuration
            .write()
            .map_err(|_| anyhow!("configuration lock poisoned"))? =
            Some(ServeConfiguration::from(options));
        self.built(generation.path())?;
        self.published
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *previous = Some(Arc::new(generation));
        Ok(())
    }

    fn version(&self, revision: u64) -> String {
        format!("{}-{revision}", self.session)
    }

    fn message(&self, state: BuildState) -> String {
        match state.phase {
            BuildPhase::Building => "building".into(),
            BuildPhase::Failed => "error".into(),
            BuildPhase::Ready => self.version(state.revision),
        }
    }

    fn set_phase(&self, phase: BuildPhase) {
        self.revision.send_if_modified(|state| {
            if state.phase == phase {
                return false;
            }
            state.phase = phase;
            true
        });
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
        self.revision.send_modify(|state| {
            state.revision = state.revision.wrapping_add(1);
            state.phase = BuildPhase::Ready;
        });
        Ok(())
    }
}

fn router(state: Arc<ServerState>) -> Router {
    let app = Router::new()
        .route("/__sycamore/reload", get(reload))
        .route("/__sycamore/reload.js", get(reload_client));
    app.fallback(application)
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
    let local = routing_path(request.uri().path()).is_ok_and(|path| {
        proxy::matches_route(&path, "/__sycamore") || !state.configuration().forwards(&path)
    });
    let mut response = next.run(request).await;
    if local {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

async fn reload_client(State(state): State<Arc<ServerState>>) -> Response {
    if !state.configuration().autoreload {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        RELOAD_CLIENT,
    )
        .into_response()
}

async fn reload(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    upgrade: std::result::Result<
        WebSocketUpgrade,
        axum::extract::ws::rejection::WebSocketUpgradeRejection,
    >,
) -> Response {
    if !state.configuration().autoreload {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !allowed_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(error) => return error.into_response(),
    };
    let revisions = state.revision.subscribe();
    upgrade
        .max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |socket| reload_socket(socket, state, revisions))
}

async fn reload_socket(
    mut socket: WebSocket,
    state: Arc<ServerState>,
    mut revisions: updates::Receiver<BuildState>,
) {
    let message = state.message(*revisions.borrow_and_update());
    if socket.send(Message::Text(message.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            changed = revisions.changed() => {
                if changed.is_err() || !state.configuration().autoreload { break; }
                let message = state.message(*revisions.borrow_and_update());
                if socket.send(Message::Text(message.into())).await.is_err() { break; }
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

fn routing_path(path: &str) -> std::result::Result<std::borrow::Cow<'_, str>, StatusCode> {
    let decoded = percent_decode_str(path)
        .decode_utf8()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if decoded.contains(['\\', '\0']) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if decoded.split('/').any(|part| part == "." || part == "..") {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(decoded)
}

async fn application(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    if proxy::matches_route(request.uri().path(), "/__sycamore") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state.published.load(std::sync::atomic::Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Waiting for the first successful build",
        )
            .into_response();
    }
    let configuration = state.configuration();
    let path = match routing_path(request.uri().path()) {
        Ok(path) => path,
        Err(status) => return status.into_response(),
    };
    if proxy::matches_route(&path, "/__sycamore") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if configuration.forwards(&path) {
        let Some(port) = configuration.backend_port else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        let version = configuration
            .autoreload
            .then(|| state.version(state.revision.borrow().revision));
        return proxy::forward(request, port, version).await;
    }
    static_file(State(state), request).await
}

async fn static_file(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    let configuration = state.configuration();
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response();
    }
    let decoded = match routing_path(request.uri().path()) {
        Ok(path) => path,
        Err(status) => return status.into_response(),
    };
    let prefix = percent_decode_str(&configuration.public_url)
        .decode_utf8()
        .expect("validated public URL");
    let mount = prefix.trim_end_matches('/');
    if !mount.is_empty() && decoded == mount {
        let location = match request.uri().query() {
            Some(query) => format!("{}?{query}", configuration.public_url),
            None => configuration.public_url.clone(),
        };
        return (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response();
    }
    let relative = match decoded
        .strip_prefix(mount)
        .filter(|path| path.starts_with('/'))
    {
        Some(path) => path,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    // Capture the revision before reading HTML. A build between this snapshot
    // and the WebSocket handshake is detected by the client's initial version.
    // Hold this generation alive for the whole request, even if another build
    // replaces it while we are opening or streaming a file.
    let (directory, version, _generation) = {
        let generation = match state.generation.lock() {
            Ok(generation) => generation,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        match state.root.read() {
            Ok(root) => (
                root.clone(),
                state.version(state.revision.borrow().revision),
                generation.clone(),
            ),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };
    let (path, spa_fallback) = match resolve_decoded_file(&directory, relative).await {
        Ok(path) => (path, false),
        Err(StatusCode::NOT_FOUND)
            if configuration.spa && !proxy::matches_route(request.uri().path(), "/__sycamore") =>
        {
            match resolve_file(&directory, "/index.html").await {
                Ok(path) => (path, true),
                Err(status) => return status.into_response(),
            }
        }
        Err(status) => return status.into_response(),
    };
    if (configuration.autoreload || spa_fallback)
        && mime_guess::from_path(&path).first_raw() == Some("text/html")
    {
        let template = match tokio::fs::read_to_string(&path).await {
            Ok(template) => template,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let html = (|| -> Result<String> {
            let template = if spa_fallback {
                html::with_spa_base(&template, &configuration.public_url)?
            } else {
                template
            };
            if configuration.autoreload {
                html::with_reload_client(&template, &version)
            } else {
                Ok(template)
            }
        })();
        let html = match html {
            Ok(html) => html,
            Err(error) => {
                eprintln!("error: preparing served HTML: {error:#}");
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
    resolve_decoded_file(root, &decoded).await
}

async fn resolve_decoded_file(
    root: &Path,
    decoded: &str,
) -> std::result::Result<PathBuf, StatusCode> {
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

    async fn start_backend(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, task)
    }

    fn proxy_state(
        directory: &Path,
        backend_port: u16,
        serve_frontend: bool,
        autoreload: bool,
    ) -> ServerState {
        let mut state = ServerState::new(directory.to_owned(), 8080, autoreload);
        *state.configuration.get_mut().unwrap() = Some(ServeConfiguration {
            serve_frontend,
            backend_port: Some(backend_port),
            forward_routes: vec!["/api".into()],
            public_url: "/".into(),
            spa: true,
            autoreload,
        });
        state
    }

    #[tokio::test]
    async fn forwards_api_before_static_and_spa_without_hiding_backend_errors() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("index.html"), "SPA shell").unwrap();
        fs::create_dir(temp.path().join("api")).unwrap();
        fs::write(temp.path().join("api/index.html"), "must not serve this").unwrap();
        let (port, task) = start_backend(Router::new().fallback(|request: Request| async move {
            let method = request.method().clone();
            let uri = request.uri().clone();
            let body = to_bytes(request.into_body(), 1024).await.unwrap();
            (
                StatusCode::NOT_FOUND,
                [(header::CACHE_CONTROL, "private, max-age=60")],
                format!("{method} {uri} {}", String::from_utf8_lossy(&body)),
            )
        }))
        .await;
        let app = router(Arc::new(proxy_state(temp.path(), port, true, false)));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/users?sort=asc")
                    .header(header::HOST, "localhost:8080")
                    .body(Body::from("payload"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, max-age=60"
        );
        assert_eq!(text(response).await, "POST /api/users?sort=asc payload");
        for encoded in ["/%61pi/users", "/api%2Fusers"] {
            let response = request(&app, Method::GET, encoded).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "private, max-age=60"
            );
        }
        assert_eq!(
            request(&app, Method::GET, "/api/%2e%2e/private")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert!(
            text(request(&app, Method::GET, "/apiary").await)
                .await
                .contains("SPA shell")
        );
        assert_eq!(
            request(&app, Method::GET, "/__sycamore/unknown")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        task.abort();
    }

    #[tokio::test]
    async fn backend_only_forwards_assets_and_redirects_without_changing_responses() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("app.js"), "wrong local asset").unwrap();
        let (port, task) = start_backend(Router::new().fallback(|request: Request| async move {
            if request.uri().path() == "/redirect" {
                (
                    StatusCode::FOUND,
                    [
                        (header::LOCATION, "/destination"),
                        (header::CACHE_CONTROL, "max-age=30"),
                    ],
                    "redirect",
                )
            } else {
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "text/javascript"),
                        (header::CACHE_CONTROL, "max-age=60"),
                    ],
                    "backend asset",
                )
            }
        }))
        .await;
        let app = router(Arc::new(proxy_state(temp.path(), port, false, true)));
        let response = request(&app, Method::GET, "/app.js").await;
        assert_eq!(response.headers()[header::CACHE_CONTROL], "max-age=60");
        assert_eq!(text(response).await, "backend asset");
        let response = request(&app, Method::GET, "/redirect").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()[header::LOCATION], "/destination");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "max-age=30");
        assert_eq!(text(response).await, "redirect");
        task.abort();
    }

    #[tokio::test]
    async fn backend_html_gets_reload_without_buffering_or_stale_length_and_validators() {
        let temp = tempfile::tempdir().unwrap();
        let (port, task) = start_backend(Router::new().fallback(|| async {
            let stream =
                futures_util::stream::once(async { Ok::<_, std::io::Error>("<html><body>SSR") })
                    .chain(futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok::<_, std::io::Error>("</body></html>")
                    }));
            (
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::ETAG, "\"old\""),
                ],
                Body::from_stream(stream),
            )
        }))
        .await;
        let state = Arc::new(proxy_state(temp.path(), port, false, true));
        let version = state.version(0);
        let app = router(state);
        let response = request(&app, Method::GET, "/posts/1").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        assert!(!response.headers().contains_key(header::ETAG));
        let mut stream = response.into_body().into_data_stream();
        let first = timeout(Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("SSR"));
        let mut rest = Vec::new();
        while let Some(chunk) = stream.next().await {
            rest.extend_from_slice(&chunk.unwrap());
        }
        let rest = String::from_utf8(rest).unwrap();
        assert!(rest.contains("/__sycamore/reload.js"));
        assert!(rest.contains(&version));
        task.abort();
    }

    #[tokio::test]
    async fn forwards_application_websockets_with_subprotocol() {
        use futures_util::SinkExt;
        let (backend_port, backend_task) = start_backend(Router::new().route(
            "/api/socket",
            get(|headers: HeaderMap, upgrade: WebSocketUpgrade| async move {
                assert_eq!(headers.get_all(header::HOST).iter().count(), 1);
                assert_eq!(headers[header::HOST], headers["x-forwarded-host"]);
                upgrade
                    .protocols(["app-v1"])
                    .on_upgrade(|mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            if socket.send(message).await.is_err() {
                                break;
                            }
                        }
                    })
            }),
        ))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut state = proxy_state(temp.path(), backend_port, true, true);
        state.port = address.port();
        let app = router(Arc::new(state));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut upgrade = format!("ws://{address}/api/socket?token=1")
            .into_client_request()
            .unwrap();
        upgrade
            .headers_mut()
            .insert("sec-websocket-protocol", "app-v1".parse().unwrap());
        let (mut socket, response) = connect_async(upgrade).await.unwrap();
        assert_eq!(response.headers()["sec-websocket-protocol"], "app-v1");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                "hello".into(),
            ))
            .await
            .unwrap();
        let message = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(message.into_text().unwrap(), "hello");
        socket.close(None).await.unwrap();
        server.abort();
        backend_task.abort();
    }

    #[tokio::test]
    async fn rejected_websocket_handshake_preserves_backend_response() {
        let rejected = "authentication required ".repeat(1024);
        let body = rejected.clone();
        let (backend_port, backend_task) = start_backend(Router::new().fallback(move || {
            let body = body.clone();
            async move {
                (
                    StatusCode::FORBIDDEN,
                    [
                        (header::SET_COOKIE, "auth=expired"),
                        (header::CACHE_CONTROL, "private"),
                    ],
                    body,
                )
            }
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut state = proxy_state(temp.path(), backend_port, false, false);
        state.port = address.port();
        let app = router(Arc::new(state));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/socket"))
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()[header::SET_COOKIE], "auth=expired");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "private");
        assert_eq!(response.text().await.unwrap(), rejected);
        server.abort();
        backend_task.abort();
    }

    #[tokio::test]
    async fn serves_only_published_frontend_generations() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("index.html"), "first successful build").unwrap();
        let state = Arc::new(ServerState::new(temp.path().to_owned(), 8080, false));
        let options = Options {
            autoreload: false,
            ..Default::default()
        };
        state
            .published
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let app = router(Arc::clone(&state));
        assert_eq!(
            request(&app, Method::GET, "/").await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        state
            .publish(snapshot_frontend(temp.path()).unwrap(), &options)
            .unwrap();
        let app = router(Arc::clone(&state));
        fs::write(temp.path().join("index.html"), "unpublished partial build").unwrap();
        state.set_phase(BuildPhase::Failed);
        assert_eq!(
            text(request(&app, Method::GET, "/").await).await,
            "first successful build"
        );
        assert_eq!(state.revision.borrow().revision, 1);
        state
            .publish(snapshot_frontend(temp.path()).unwrap(), &options)
            .unwrap();
        assert_eq!(
            text(request(&app, Method::GET, "/").await).await,
            "unpublished partial build"
        );
        assert_eq!(state.revision.borrow().revision, 2);
    }

    #[tokio::test]
    async fn failed_backend_returns_bad_gateway_instead_of_spa_shell() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("index.html"), "SPA shell").unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let app = router(Arc::new(proxy_state(temp.path(), port, true, false)));
        assert_eq!(
            request(&app, Method::GET, "/api/users").await.status(),
            StatusCode::BAD_GATEWAY
        );
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
    async fn spa_falls_back_to_index_without_hiding_real_files_or_unsafe_requests() {
        let temp = tempfile::tempdir().unwrap();
        let source = "<title>SPA</title><main>App shell</main>";
        fs::write(temp.path().join("index.html"), source).unwrap();
        fs::write(temp.path().join("app.js"), "export default 1;").unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(
            temp.path().join("nested/index.html"),
            "<main>Real nested page</main>",
        )
        .unwrap();
        for autoreload in [false, true] {
            let mut state = ServerState::new(temp.path().to_owned(), 8080, autoreload);
            state.spa = true;
            let configuration = ServeConfiguration {
                public_url: "/my-app/".into(),
                ..state.configuration()
            };
            *state.configuration.get_mut().unwrap() = Some(configuration);
            let app = router(Arc::new(state));
            let response = request(&app, Method::GET, "/my-app/users/profile?tab=1").await;
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
            assert!(html.contains("App shell"));
            assert!(html.contains("<base href=\"/my-app/\">"));
            assert_eq!(html.contains("data-cargo-sycamore-reload"), autoreload);
            let head = request(&app, Method::HEAD, "/my-app/users/profile").await;
            assert_eq!(head.status(), StatusCode::OK);
            assert_eq!(head.headers()[header::CONTENT_LENGTH], length);
            assert!(text(head).await.is_empty());
            assert_eq!(
                text(request(&app, Method::GET, "/my-app/app.js").await).await,
                "export default 1;"
            );
            assert!(
                text(request(&app, Method::GET, "/my-app/nested/").await)
                    .await
                    .contains("Real nested page")
            );
            assert_eq!(
                request(&app, Method::GET, "/missing.js").await.status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                request(&app, Method::GET, "/__sycamore/missing")
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                request(&app, Method::GET, "/%2e%2e/secret").await.status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                request(&app, Method::POST, "/missing").await.status(),
                StatusCode::METHOD_NOT_ALLOWED
            );
        }
        assert_eq!(
            fs::read_to_string(temp.path().join("index.html")).unwrap(),
            source
        );
        fs::remove_file(temp.path().join("index.html")).unwrap();
        let mut state = ServerState::new(temp.path().to_owned(), 8080, false);
        state.spa = true;
        assert_eq!(
            request(&router(Arc::new(state)), Method::GET, "/missing")
                .await
                .status(),
            StatusCode::NOT_FOUND
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
        state.set_phase(BuildPhase::Building);
        for socket in [&mut one, &mut two] {
            let message = timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(message.into_text().unwrap(), "building");
        }
        state.set_phase(BuildPhase::Failed);
        for socket in [&mut one, &mut two] {
            let message = timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(message.into_text().unwrap(), "error");
        }
        assert_eq!(
            state.revision.borrow().revision,
            1,
            "failed builds must not advance the reload version"
        );
        // A page connected after a failed build must also show the stale/error pill.
        let (mut after_failure, _) = connect_async(websocket_request(address, Some(&origin)))
            .await
            .unwrap();
        let message = timeout(Duration::from_secs(2), after_failure.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(message.into_text().unwrap(), "error");
        after_failure.close(None).await.unwrap();
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
