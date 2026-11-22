//! Web API built into the scanner binary (PLAN.md 3.5a).
//!
//! Phase 3 groundwork: read-only routes under `/api/v1` with role guards.
//!
//! | Route | Needs |
//! |---|---|
//! | `GET /api/v1/status` | anonymous if `public.live` (default on), else admin |
//! | `GET /api/v1/recordings` | anonymous if `public.recordings` (default off), else admin |
//! | `GET /api/v1/recordings/{id}` | same as the list |
//! | `GET /api/v1/recordings/{id}/audio` | anonymous if `public.audio` (default off), else admin |
//!
//! Admin is a bearer API token for now (`Authorization: Bearer ...`), compared
//! in constant time. Browser sessions with argon2id passwords, CSRF and
//! hashed, revocable tokens come next.
//!
//! The DSP thread publishes a [`LiveStatus`] snapshot into a [`StatusHandle`];
//! recordings come from the SQLite index, opened per request on a blocking
//! thread so the async runtime never waits on disk.

use axum::extract::{Path as UrlPath, Query as UrlQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use scanner_store::{Index, Query, Row};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Roles recognised by the API. Each route declares the minimum role it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Anonymous,
    Admin,
}

/// What anonymous visitors may do. Fresh-install defaults: live status on,
/// recordings and audio off (PLAN.md 3.5a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Public {
    pub live: bool,
    pub recordings: bool,
    pub audio: bool,
}

impl Default for Public {
    fn default() -> Self {
        Public {
            live: true,
            recordings: false,
            audio: false,
        }
    }
}

/// A transmission being recorded now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActiveChannel {
    pub freq_hz: f64,
    pub snr_db: f32,
    pub secs: f64,
}

/// Snapshot of the running scanner, published by the DSP thread.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveStatus {
    pub source: String,
    pub sample_rate: f64,
    pub center_freq_hz: f64,
    pub profile: String,
    pub slots: usize,
    pub uptime_s: f64,
    /// Share of one core the DSP thread spends scanning.
    pub dsp_load: f64,
    pub samples_dropped: u64,
    pub recorded: u64,
    pub active: Vec<ActiveChannel>,
    /// Noise floor per segment across the band (dB per bin).
    pub noise_floor_db: Vec<f32>,
}

pub type StatusHandle = Arc<RwLock<LiveStatus>>;

#[derive(Clone)]
pub struct AppState {
    pub status: StatusHandle,
    /// Recordings folder (holds `index.sqlite`).
    pub recordings: PathBuf,
    pub public: Public,
    /// Admin API token; `None` means nobody is admin over HTTP.
    pub admin_token: Option<Arc<str>>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl AppState {
    fn role(&self, headers: &HeaderMap) -> Role {
        let Some(token) = &self.admin_token else {
            return Role::Anonymous;
        };
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match given {
            Some(g) if constant_time_eq(g.as_bytes(), token.as_bytes()) => Role::Admin,
            _ => Role::Anonymous,
        }
    }

    /// Guard: the caller's role, or 401/403.
    fn require(&self, headers: &HeaderMap, public: bool) -> Result<Role, ApiError> {
        let role = self.role(headers);
        if public || role >= Role::Admin {
            Ok(role)
        } else if headers.contains_key(header::AUTHORIZATION) {
            Err(ApiError(StatusCode::FORBIDDEN, "not allowed"))
        } else {
            Err(ApiError(StatusCode::UNAUTHORIZED, "login required"))
        }
    }

    fn index(&self) -> Result<Index, ApiError> {
        Index::open(&self.recordings.join("index.sqlite"))
            .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "index unavailable"))
    }
}

#[derive(Debug)]
pub struct ApiError(StatusCode, &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/recordings", get(list_recordings))
        .route("/api/v1/recordings/{id}", get(get_recording))
        .route("/api/v1/recordings/{id}/audio", get(get_audio))
        .with_state(state)
}

async fn status(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LiveStatus>, ApiError> {
    s.require(&headers, s.public.live)?;
    let snap = s.status.read().map(|g| g.clone()).unwrap_or_default();
    Ok(Json(snap))
}

/// Query string for the recordings list. Frequencies in Hz, times as Unix
/// seconds.
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub freq_min: Option<f64>,
    pub freq_max: Option<f64>,
    pub since: Option<f64>,
    pub until: Option<f64>,
    pub min_secs: Option<f64>,
    pub before_id: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct Recording {
    #[serde(flatten)]
    row: Row,
    audio_url: String,
}

fn recording(r: Row) -> Recording {
    Recording {
        audio_url: format!("/api/v1/recordings/{}/audio", r.id),
        row: r,
    }
}

async fn list_recordings(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlQuery(p): UrlQuery<ListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.require(&headers, s.public.recordings)?;
    let q = Query {
        freq_min: p.freq_min,
        freq_max: p.freq_max,
        since_ns: p.since.map(|t| (t * 1e9) as i64),
        until_ns: p.until.map(|t| (t * 1e9) as i64),
        min_secs: p.min_secs,
        before_id: p.before_id,
        limit: p.limit.unwrap_or(100),
    };
    let rows = blocking(move || s.index()?.search(&q).map_err(db_err)).await?;
    let next = rows.last().map(|r| r.id);
    let items: Vec<Recording> = rows.into_iter().map(recording).collect();
    Ok(Json(
        serde_json::json!({ "items": items, "next_before_id": next }),
    ))
}

async fn get_recording(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.require(&headers, s.public.recordings)?;
    let row = blocking(move || s.index()?.get(id).map_err(db_err)).await?;
    match row {
        Some(r) => Ok(Json(serde_json::to_value(recording(r)).unwrap_or_default())),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such recording")),
    }
}

/// The WAV, with single-range support so browsers can seek.
async fn get_audio(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> Result<Response, ApiError> {
    s.require(&headers, s.public.audio)?;
    let root = s.recordings.clone();
    let row = blocking(move || s.index()?.get(id).map_err(db_err)).await?;
    let Some(row) = row.filter(|r| r.status == "complete") else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such recording"));
    };
    // Paths in the index are relative and written by the recorder; refuse
    // anything that could step outside the recordings folder.
    let rel = std::path::Path::new(&row.wav_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such recording"));
    }
    let data = tokio::fs::read(root.join(rel))
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "audio file missing"))?;
    let total = data.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_range(v, total));
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    resp_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    match range {
        Some((a, b)) => {
            let v = format!("bytes {a}-{b}/{total}");
            resp_headers.insert(header::CONTENT_RANGE, HeaderValue::from_str(&v).unwrap());
            Ok((
                StatusCode::PARTIAL_CONTENT,
                resp_headers,
                data[a..=b].to_vec(),
            )
                .into_response())
        }
        None => Ok((StatusCode::OK, resp_headers, data).into_response()),
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n`, one range only.
fn parse_range(v: &str, total: usize) -> Option<(usize, usize)> {
    let spec = v.strip_prefix("bytes=")?;
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: usize = n.parse().ok()?;
            (total.saturating_sub(n), total - 1)
        }
        (a, "") => (a.parse().ok()?, total - 1),
        (a, b) => (a.parse().ok()?, b.parse::<usize>().ok()?.min(total - 1)),
    };
    (start <= end && start < total).then_some((start, end))
}

fn db_err(_: scanner_store::index::IndexError) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "index error")
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "worker failed"))?
}

/// Run the API on its own thread with a small tokio runtime, so the DSP
/// threads stay plain std threads.
pub fn spawn(
    addr: std::net::SocketAddr,
    state: AppState,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("web")
        .enable_all()
        .build()?;
    let listener = rt.block_on(tokio::net::TcpListener::bind(addr))?;
    std::thread::Builder::new()
        .name("web-main".into())
        .spawn(move || {
            rt.block_on(async move {
                if let Err(e) = axum::serve(listener, router(state)).await {
                    eprintln!("web server stopped: {e}");
                }
            });
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use scanner_store::index::NewRow;
    use tower::ServiceExt;

    const TOKEN: &str = "test-admin-token";

    fn setup(public: Public) -> (tempfile::TempDir, AppState, i64) {
        let dir = tempfile::tempdir().unwrap();
        let ix = Index::open(&dir.path().join("index.sqlite")).unwrap();
        let id = ix
            .insert(&NewRow {
                freq_hz: 146.52e6,
                channel: Some("Calling"),
                origin: "listed",
                mode: "nbfm",
                start_ns: 1_795_330_443_000_000_000,
                audio_rate: 8000,
                wav_path: "2026/11/22/a.wav",
            })
            .unwrap();
        std::fs::create_dir_all(dir.path().join("2026/11/22")).unwrap();
        std::fs::write(dir.path().join("2026/11/22/a.wav"), b"RIFF0123456789").unwrap();
        ix.complete(id, 0, 1.0, 1.0, 20.0, 15.0, "2026/11/22/a.wav", 14)
            .unwrap();
        let state = AppState {
            status: Arc::new(RwLock::new(LiveStatus {
                source: "test".into(),
                ..Default::default()
            })),
            recordings: dir.path().to_owned(),
            public,
            admin_token: Some(TOKEN.into()),
        };
        (dir, state, id)
    }

    async fn get(
        state: &AppState,
        uri: &str,
        token: Option<&str>,
        range: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut req = Request::get(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(r) = range {
            req = req.header(header::RANGE, r);
        }
        let resp = router(state.clone())
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        (status, body)
    }

    /// Every route × role × public setting. A new route must be added here.
    #[tokio::test]
    async fn route_role_matrix() {
        use StatusCode as S;
        let defaults = Public::default();
        let open = Public {
            live: true,
            recordings: true,
            audio: true,
        };
        let closed = Public {
            live: false,
            recordings: false,
            audio: false,
        };
        let routes = [
            "/api/v1/status",
            "/api/v1/recordings",
            "/api/v1/recordings/{id}",
            "/api/v1/recordings/{id}/audio",
        ];
        // (public, token, expected per route)
        let cases: [(Public, Option<&str>, [S; 4]); 7] = [
            (
                defaults,
                None,
                [S::OK, S::UNAUTHORIZED, S::UNAUTHORIZED, S::UNAUTHORIZED],
            ),
            (
                defaults,
                Some("wrong"),
                [S::OK, S::FORBIDDEN, S::FORBIDDEN, S::FORBIDDEN],
            ),
            (defaults, Some(TOKEN), [S::OK; 4]),
            (open, None, [S::OK; 4]),
            (closed, None, [S::UNAUTHORIZED; 4]),
            (closed, Some("wrong"), [S::FORBIDDEN; 4]),
            (closed, Some(TOKEN), [S::OK; 4]),
        ];
        for (public, token, expect) in cases {
            let (_dir, state, id) = setup(public);
            for (route, want) in routes.iter().zip(expect) {
                let uri = route.replace("{id}", &id.to_string());
                let (got, _) = get(&state, &uri, token, None).await;
                assert_eq!(got, want, "{uri} public={public:?} token={token:?}");
            }
        }
    }

    #[tokio::test]
    async fn lists_and_serves_audio_with_ranges() {
        let (_dir, state, id) = setup(Public::default());
        let (st, body) = get(
            &state,
            "/api/v1/recordings?freq_min=146e6&freq_max=147e6",
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["items"][0]["freq_hz"], 146.52e6);
        assert_eq!(
            v["items"][0]["audio_url"],
            format!("/api/v1/recordings/{id}/audio")
        );
        let (st, body) = get(
            &state,
            "/api/v1/recordings?freq_min=147e6",
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 0);

        let audio = format!("/api/v1/recordings/{id}/audio");
        let (st, body) = get(&state, &audio, Some(TOKEN), None).await;
        assert_eq!(
            (st, body.as_slice()),
            (StatusCode::OK, b"RIFF0123456789".as_slice())
        );
        let (st, body) = get(&state, &audio, Some(TOKEN), Some("bytes=4-7")).await;
        assert_eq!(
            (st, body.as_slice()),
            (StatusCode::PARTIAL_CONTENT, b"0123".as_slice())
        );
        let (st, body) = get(&state, &audio, Some(TOKEN), Some("bytes=-2")).await;
        assert_eq!(
            (st, body.as_slice()),
            (StatusCode::PARTIAL_CONTENT, b"89".as_slice())
        );
        let (st, _) = get(&state, "/api/v1/recordings/999", Some(TOKEN), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-0", 10), Some((0, 0)));
        assert_eq!(parse_range("bytes=5-", 10), Some((5, 9)));
        assert_eq!(parse_range("bytes=5-100", 10), Some((5, 9)));
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(parse_range("bytes=10-", 10), None);
        assert_eq!(parse_range("bytes=1-2,4-5", 10), None);
        assert_eq!(parse_range("items=1-2", 10), None);
    }
}
