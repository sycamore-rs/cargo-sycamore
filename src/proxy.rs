//! Loopback development proxy. Application responses remain owned by the backend.

use std::{sync::OnceLock, time::Duration};

use axum::{
    body::{Body, Bytes},
    extract::{
        FromRequestParts, Request, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt, stream};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

/// Prefixes match whole path segments, never `/apiary` for `/api`.
pub(crate) fn matches_route(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse::<header::HeaderName>().ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn forwarded_headers(headers: &mut HeaderMap) {
    // Never trust client-supplied proxy metadata. Preserve Host and Origin so
    // applications see the browser-facing origin, not the private backend port.
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-port",
        "x-forwarded-prefix",
        "x-forwarded-server",
        "x-real-ip",
    ] {
        headers.remove(name);
    }
    if let Some(host) = headers.get(header::HOST).cloned() {
        headers.insert("x-forwarded-host", host);
    }
    headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    headers.insert("x-forwarded-for", HeaderValue::from_static("127.0.0.1"));
}

pub(crate) async fn forward(
    request: Request,
    port: u16,
    reload_version: Option<String>,
) -> Response {
    if request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return forward_websocket(request, port).await;
    }
    match forward_http(request, port, reload_version).await {
        Ok(response) => response,
        Err(error) => {
            crate::logging::error(format_args!("forwarding request to backend: {error}"));
            (StatusCode::BAD_GATEWAY, "Backend is unavailable").into_response()
        }
    }
}

async fn forward_http(
    request: Request,
    port: u16,
    reload_version: Option<String>,
) -> Result<Response, reqwest::Error> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("valid development proxy client")
    });
    let (parts, body) = request.into_parts();
    let url = format!(
        "http://127.0.0.1:{port}{}",
        parts
            .uri
            .path_and_query()
            .map_or("/", |value| value.as_str())
    );
    let head = parts.method == Method::HEAD;
    let mut headers = parts.headers;
    strip_hop_headers(&mut headers);
    forwarded_headers(&mut headers);
    // Ask for uncompressed HTML so reload injection can preserve streaming.
    // If the backend nevertheless encodes its response, leave it untouched.
    if reload_version.is_some() {
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    let response = client
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await?;
    let status = response.status();
    let mut headers = response.headers().clone();
    strip_hop_headers(&mut headers);
    let html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/html"))
        });
    let identity = headers
        .get(header::CONTENT_ENCODING)
        .is_none_or(|value| value.as_bytes().eq_ignore_ascii_case(b"identity"));
    let inject = reload_version.filter(|_| html && identity && status == StatusCode::OK);
    let body = if let Some(version) = inject {
        for name in [
            header::CONTENT_LENGTH,
            header::ETAG,
            header::LAST_MODIFIED,
            header::ACCEPT_RANGES,
        ] {
            headers.remove(name);
        }
        headers.remove("content-md5");
        headers.remove("digest");
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if head {
            Body::empty()
        } else {
            let script = Bytes::from(format!(
                r#"<script type="module" src="/__sycamore/reload.js" data-cargo-sycamore-reload data-version="{version}"></script>"#
            ));
            // Do not buffer SSR streams or parse/rewrite the application's HTML.
            Body::from_stream(
                response
                    .bytes_stream()
                    .chain(stream::once(async move { Ok::<_, reqwest::Error>(script) })),
            )
        }
    } else if head {
        Body::empty()
    } else {
        Body::from_stream(response.bytes_stream())
    };
    let mut result = Response::new(body);
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    Ok(result)
}

async fn forward_websocket(request: Request, port: u16) -> Response {
    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(error) => return error.into_response(),
    };
    let url = format!(
        "ws://127.0.0.1:{port}{}",
        parts
            .uri
            .path_and_query()
            .map_or("/", |value| value.as_str())
    );
    let mut backend = match url.into_client_request() {
        Ok(request) => request,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let mut headers = parts.headers;
    strip_hop_headers(&mut headers);
    forwarded_headers(&mut headers);
    // Retain the newly generated handshake key/version and upgrade headers.
    headers.remove("sec-websocket-key");
    headers.remove("sec-websocket-version");
    headers.remove("sec-websocket-extensions");
    if let Some(host) = headers.remove(header::HOST) {
        backend.headers_mut().insert(header::HOST, host);
    }
    backend.headers_mut().extend(headers);
    let expected_accept = tungstenite::handshake::derive_accept_key(
        backend.headers()["sec-websocket-key"].as_bytes(),
    );
    let offered: Vec<String> = backend
        .headers()
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_owned())
        .collect();
    // Perform the handshake as HTTP first so rejected upgrades retain the
    // backend's entire streamed response, not just tungstenite's read-ahead.
    let client = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("valid websocket proxy client");
    let response = match client
        .get(format!(
            "http://127.0.0.1:{port}{}",
            parts
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str())
        ))
        .headers(backend.headers().clone())
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status();
        let mut headers = response.headers().clone();
        strip_hop_headers(&mut headers);
        let mut result = Response::new(Body::from_stream(response.bytes_stream()));
        *result.status_mut() = status;
        *result.headers_mut() = headers;
        return result;
    }
    let accepted = response
        .headers()
        .get("sec-websocket-accept")
        .and_then(|value| value.to_str().ok());
    let protocol = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let valid_upgrade = response
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
        && response
            .headers()
            .get(header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            });
    if accepted != Some(expected_accept.as_str())
        || !valid_upgrade
        || protocol
            .as_ref()
            .is_some_and(|protocol| !offered.contains(protocol))
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mut headers = response.headers().clone();
    let stream = match response.upgrade().await {
        Ok(stream) => stream,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let backend = tokio_tungstenite::WebSocketStream::from_raw_socket(
        stream,
        tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    let upgrade = if let Some(protocol) = protocol {
        upgrade.protocols([protocol])
    } else {
        upgrade
    };
    let mut result = upgrade.on_upgrade(move |socket| relay_websocket(socket, backend));
    strip_hop_headers(&mut headers);
    headers.remove("sec-websocket-accept");
    headers.remove("sec-websocket-protocol");
    headers.remove("sec-websocket-extensions");
    for (name, value) in &headers {
        result.headers_mut().append(name.clone(), value.clone());
    }
    result
}

async fn relay_websocket(
    mut browser: WebSocket,
    mut backend: tokio_tungstenite::WebSocketStream<reqwest::Upgraded>,
) {
    loop {
        tokio::select! {
            message = browser.recv() => {
                let message = match message {
                    Some(Ok(Message::Text(value))) => tungstenite::Message::Text(value.as_str().into()),
                    Some(Ok(Message::Binary(value))) => tungstenite::Message::Binary(value),
                    Some(Ok(Message::Ping(value))) => tungstenite::Message::Ping(value),
                    Some(Ok(Message::Pong(value))) => tungstenite::Message::Pong(value),
                    Some(Ok(Message::Close(frame))) => {
                        let frame = frame.map(|frame| tungstenite::protocol::CloseFrame { code: frame.code.into(), reason: frame.reason.as_str().into() });
                        let _ = backend.send(tungstenite::Message::Close(frame)).await;
                        break;
                    }
                    _ => break,
                };
                if backend.send(message).await.is_err() { break; }
            }
            message = backend.next() => {
                let message = match message {
                    Some(Ok(tungstenite::Message::Text(value))) => Message::Text(value.as_str().into()),
                    Some(Ok(tungstenite::Message::Binary(value))) => Message::Binary(value),
                    Some(Ok(tungstenite::Message::Ping(value))) => Message::Ping(value),
                    Some(Ok(tungstenite::Message::Pong(value))) => Message::Pong(value),
                    Some(Ok(tungstenite::Message::Close(frame))) => {
                        let frame = frame.map(|frame| axum::extract::ws::CloseFrame { code: frame.code.into(), reason: frame.reason.as_str().into() });
                        let _ = browser.send(Message::Close(frame)).await;
                        break;
                    }
                    Some(Ok(tungstenite::Message::Frame(_))) => continue,
                    _ => break,
                };
                if browser.send(message).await.is_err() { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_match_segment_boundaries() {
        assert!(matches_route("/api", "/api"));
        assert!(matches_route("/api/users", "/api/"));
        assert!(matches_route("/anything", "/"));
        assert!(!matches_route("/apiary", "/api"));
    }

    #[test]
    fn removes_connection_nominated_headers_and_proxy_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, "keep-alive, x-private".parse().unwrap());
        headers.insert("x-private", "secret".parse().unwrap());
        headers.insert(header::PROXY_AUTHORIZATION, "secret".parse().unwrap());
        headers.insert(header::COOKIE, "session=ok".parse().unwrap());
        strip_hop_headers(&mut headers);
        assert!(!headers.contains_key("x-private"));
        assert!(!headers.contains_key(header::CONNECTION));
        assert!(!headers.contains_key(header::PROXY_AUTHORIZATION));
        assert!(headers.contains_key(header::COOKIE));
    }
}
