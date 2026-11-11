//! Local static-file development server with the normal build/watch pipeline.

use std::{
    fs::{self, File},
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock, mpsc},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use percent_encoding::percent_decode_str;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::{
    build,
    options::{Options, ServeArgs},
    watch,
};

/// Serve the selected package's dist (or configured output) on loopback only.
/// The port is fixed at startup; build configuration is reloaded by watch mode.
pub fn serve(args: ServeArgs) -> Result<()> {
    let options = Options::resolve_serve(&args)?;
    let directory = build::output_directory(&options)?;
    watch::validate(&options)?;
    let server = Server::http(("127.0.0.1", options.port)).map_err(|error| {
        anyhow!(
            "binding development server on port {}: {error}",
            options.port
        )
    })?;
    eprintln!(
        "Serving {} at http://{} (Ctrl-C to stop)",
        directory.display(),
        server.server_addr()
    );
    let root = Arc::new(RwLock::new(directory));
    let build_root = Arc::clone(&root);
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("sycamore-watch".into())
        .spawn(move || {
            let result = watch::watch_with(&args.build, |directory| {
                let mut root = build_root
                    .write()
                    .map_err(|_| anyhow!("server directory lock poisoned"))?;
                if *root != directory {
                    *root = directory.to_owned();
                    eprintln!("Serving updated output directory: {}", root.display());
                }
                Ok(())
            });
            let _ = sender.send(result);
        })
        .context("starting build watcher")?;

    loop {
        match receiver.try_recv() {
            Ok(result) => return result.context("build watcher stopped"),
            Err(mpsc::TryRecvError::Disconnected) => bail!("build watcher stopped unexpectedly"),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(request) = server
            .recv_timeout(Duration::from_millis(100))
            .context("receiving HTTP request")?
        {
            let directory = root
                .read()
                .map_err(|_| anyhow!("server directory lock poisoned"))?
                .clone();
            // Client disconnects should not stop serving or rebuilding.
            if let Err(error) = respond(request, &directory) {
                eprintln!("warning: HTTP response failed: {error}");
            }
        }
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("valid static HTTP header")
}

fn respond(request: Request, root: &Path) -> std::io::Result<()> {
    if !matches!(request.method(), Method::Get | Method::Head) {
        return request.respond(
            Response::from_string("Method not allowed\n")
                .with_status_code(StatusCode(405))
                .with_header(header("Allow", "GET, HEAD")),
        );
    }
    let path = match resolve_file(root, request.url()) {
        Ok(path) => path,
        Err(code) => return request.respond(Response::empty(StatusCode(code))),
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            let code = if error.kind() == std::io::ErrorKind::PermissionDenied {
                403
            } else {
                404
            };
            return request.respond(Response::empty(StatusCode(code)));
        }
    };
    // tiny_http suppresses the body for HEAD, preserving Content-Length.
    request.respond(
        Response::from_file(file)
            .with_header(header(
                "Content-Type",
                mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .as_ref(),
            ))
            .with_header(header("Cache-Control", "no-store")),
    )
}

/// Decode once, reject traversal (including Windows paths), then enforce the
/// canonical boundary to prevent symlinks from exposing files outside dist.
fn resolve_file(root: &Path, url: &str) -> std::result::Result<PathBuf, u16> {
    let url_path = url.split('?').next().unwrap_or(url);
    let decoded = percent_decode_str(url_path)
        .decode_utf8()
        .map_err(|_| 400u16)?;
    if !decoded.starts_with('/') || decoded.contains(['\\', ':', '\0']) {
        return Err(400);
    }
    let relative = Path::new(decoded.trim_start_matches('/'));
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(403);
    }
    let root = fs::canonicalize(root).map_err(|_| 404u16)?;
    let mut path = root.join(relative);
    if path.is_dir() {
        path.push("index.html");
    }
    let path = fs::canonicalize(path).map_err(|_| 404u16)?;
    if !path.starts_with(&root) {
        return Err(403);
    }
    if !path.is_file() {
        return Err(404);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_index_assets_query_strings_and_encoded_names() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("index.html"), "app").unwrap();
        fs::write(temp.path().join("app_bg.wasm"), b"wasm").unwrap();
        fs::write(temp.path().join("my file.js"), "js").unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("nested/index.html"), "nested").unwrap();
        for (url, file) in [
            ("/", "index.html"),
            ("/app_bg.wasm?cache=1", "app_bg.wasm"),
            ("/my%20file.js", "my file.js"),
            ("/nested/", "nested/index.html"),
        ] {
            assert_eq!(
                resolve_file(temp.path(), url).unwrap(),
                fs::canonicalize(temp.path().join(file)).unwrap()
            );
        }
        assert_eq!(resolve_file(temp.path(), "/absent"), Err(404));
    }

    #[test]
    fn rejects_traversal_and_platform_specific_paths() {
        let temp = tempfile::tempdir().unwrap();
        for url in [
            "/../secret",
            "/%2e%2e/secret",
            "/nested/%2e%2e/secret",
            "/%2e%2e%2fsecret",
        ] {
            assert_eq!(resolve_file(temp.path(), url), Err(403), "{url}");
        }
        for url in ["/C:/secret", "/..%5csecret", "/%00", "/%ff", "relative"] {
            assert_eq!(resolve_file(temp.path(), url), Err(400), "{url}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("dist");
        fs::create_dir(&root).unwrap();
        fs::write(temp.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(temp.path().join("secret"), root.join("escape")).unwrap();
        assert_eq!(resolve_file(&root, "/escape"), Err(403));
    }

    #[test]
    fn serves_files_head_and_errors_over_http() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("index.html"), "<h1>Hello</h1>").unwrap();
        fs::write(temp.path().join("app.wasm"), b"wasm").unwrap();
        let root = temp.path().to_owned();
        let server = Server::http(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            for _ in 0..5 {
                let request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                respond(request, &root).unwrap();
            }
        });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client.get(format!("{url}/")).send().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert_eq!(response.text().unwrap(), "<h1>Hello</h1>");
        let response = client.head(format!("{url}/")).send().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-length"], "14");
        assert!(response.bytes().unwrap().is_empty());
        let response = client.get(format!("{url}/app.wasm")).send().unwrap();
        assert_eq!(response.headers()["content-type"], "application/wasm");
        assert_eq!(
            client.get(format!("{url}/absent")).send().unwrap().status(),
            404
        );
        let response = client.post(format!("{url}/")).send().unwrap();
        assert_eq!(response.status(), 405);
        assert_eq!(response.headers()["allow"], "GET, HEAD");
        worker.join().unwrap();
    }
}
