//! The browser UI: static files compiled into the binary (no build step,
//! no CDN), plus the security headers every response carries.
//!
//! The UI is plain HTML, CSS and JavaScript under `crates/web/ui/`. It only
//! talks to `/api/v1`, so anything it does can also be done with a token.

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

struct Asset {
    path: &'static str,
    content_type: &'static str,
    body: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        content_type: "text/html; charset=utf-8",
        body: include_bytes!("../ui/index.html"),
    },
    Asset {
        path: "/assets/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../ui/app.js"),
    },
    Asset {
        path: "/assets/app.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../ui/app.css"),
    },
    Asset {
        path: "/assets/icon.svg",
        content_type: "image/svg+xml",
        body: include_bytes!("../ui/icon.svg"),
    },
];

/// Paths the UI is served at.
pub fn paths() -> impl Iterator<Item = &'static str> {
    ASSETS.iter().map(|a| a.path)
}

fn etags() -> &'static [String] {
    static TAGS: OnceLock<Vec<String>> = OnceLock::new();
    TAGS.get_or_init(|| {
        ASSETS
            .iter()
            .map(|a| {
                let d = Sha256::digest(a.body);
                let hex: String = d[..8].iter().map(|b| format!("{b:02x}")).collect();
                format!("\"{hex}\"")
            })
            .collect()
    })
}

/// Serve one embedded file. Browsers revalidate each time (`no-cache`) and
/// get `304 Not Modified` while it is unchanged, which keeps repeat visits
/// to a few hundred bytes on a metered uplink.
pub async fn asset(uri: axum::http::Uri, headers: HeaderMap) -> Response {
    let Some(i) = ASSETS.iter().position(|a| a.path == uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let a = &ASSETS[i];
    let tag = &etags()[i];
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, HeaderValue::from_str(tag).expect("ascii"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == tag));
    if matches {
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(a.content_type),
    );
    (StatusCode::OK, h, a.body).into_response()
}

/// Content Security Policy: only this origin's own scripts, styles, media
/// and connections; no inline script, no framing.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; media-src 'self'; connect-src 'self'; object-src 'none'; \
    base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Adds the security headers to every response.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    resp
}
