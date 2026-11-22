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
//! Accounts (see [`auth`]):
//!
//! | Route | Needs |
//! |---|---|
//! | `POST /api/v1/auth/login` | anyone (rate limited); sets the session cookie |
//! | `POST /api/v1/auth/logout` | session + CSRF header |
//! | `GET /api/v1/auth/session` | anyone: who am I, plus the CSRF token |
//! | `POST /api/v1/auth/password` | session + CSRF header + current password |
//! | `GET /api/v1/auth/tokens` | admin |
//! | `POST /api/v1/auth/tokens` | admin (+ CSRF header when using a session) |
//! | `DELETE /api/v1/auth/tokens/{id}` | admin (+ CSRF header when using a session) |
//!
//! Admin is either a browser session (an `HttpOnly`, `SameSite=Strict`,
//! `Secure` cookie) or an API token (`Authorization: Bearer scn_...`).
//! Requests that change something and are authenticated by the cookie must
//! also send the session's CSRF token in `X-CSRF-Token`.
//!
//! The DSP thread publishes a [`LiveStatus`] snapshot into a [`StatusHandle`];
//! recordings come from the SQLite index, opened per request on a blocking
//! thread so the async runtime never waits on disk.

pub mod auth;

use auth::{AuthStore, LoginLimiter, Session, Token, constant_time_eq, now_ns};
use axum::extract::{
    DefaultBodyLimit, FromRequestParts, Path as UrlPath, Query as UrlQuery, State,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use scanner_store::{Index, Query, Row};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

/// Name of the browser session cookie.
pub const SESSION_COOKIE: &str = "scanner_session";
/// Header carrying the session's CSRF token on state-changing requests.
pub const CSRF_HEADER: &str = "x-csrf-token";

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
    /// Accounts, sessions and tokens; `None` means nobody can be admin over
    /// HTTP.
    pub auth: Option<Arc<AuthStore>>,
    pub limiter: Arc<Mutex<LoginLimiter>>,
    /// Mark the session cookie `Secure` (browsers then send it only over
    /// HTTPS, or to localhost). Off only for plain-HTTP setups on a LAN.
    pub secure_cookies: bool,
}

impl AppState {
    pub fn new(
        status: StatusHandle,
        recordings: PathBuf,
        public: Public,
        auth: Option<Arc<AuthStore>>,
    ) -> Self {
        AppState {
            status,
            recordings,
            public,
            auth,
            limiter: Default::default(),
            secure_cookies: true,
        }
    }
}

/// How the caller authenticated.
#[derive(Debug, Clone)]
enum Via {
    Nothing,
    /// Credentials were sent but are not valid (bad or revoked token).
    Rejected,
    Session {
        secret: String,
        session: Session,
    },
    Token(Token),
}

#[derive(Debug, Clone)]
struct Caller {
    role: Role,
    via: Via,
}

impl Caller {
    fn actor(&self) -> String {
        match &self.via {
            Via::Session { session, .. } => session.username.clone(),
            Via::Token(t) => format!("token:{}", t.name),
            _ => "-".into(),
        }
    }

    /// Guard: the route is open to anonymous callers, or the caller is admin.
    fn require(&self, public: bool) -> Result<(), ApiError> {
        if public || self.role >= Role::Admin {
            Ok(())
        } else if matches!(self.via, Via::Rejected) {
            Err(ApiError(StatusCode::FORBIDDEN, "not allowed".into()))
        } else {
            Err(ApiError(StatusCode::UNAUTHORIZED, "login required".into()))
        }
    }

    /// Guard for requests that change something: admin, and when the
    /// session cookie is what authenticates, its CSRF token in the header.
    fn require_write(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        self.require(false)?;
        if let Via::Session { session, .. } = &self.via {
            check_csrf(session, headers)?;
        }
        Ok(())
    }
}

fn check_csrf(session: &Session, headers: &HeaderMap) -> Result<(), ApiError> {
    let given = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if constant_time_eq(given.as_bytes(), session.csrf.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "missing or wrong CSRF token".into(),
        ))
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_string())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string())
}

/// The client address (for rate limiting and the audit log).
struct Client(String);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Client(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0.ip().to_string())
                .unwrap_or_else(|| "unknown".into()),
        ))
    }
}

impl AppState {
    /// Who is calling. A bearer token wins over a cookie.
    async fn caller(&self, headers: &HeaderMap) -> Result<Caller, ApiError> {
        let anonymous = |via| Caller {
            role: Role::Anonymous,
            via,
        };
        let Some(auth) = self.auth.clone() else {
            return Ok(anonymous(if headers.contains_key(header::AUTHORIZATION) {
                Via::Rejected
            } else {
                Via::Nothing
            }));
        };
        if let Some(tok) = bearer(headers) {
            let now = now_ns();
            let t = blocking(move || auth.token(&tok, now).map_err(auth_err)).await?;
            return Ok(match t {
                Some(t) => Caller {
                    role: Role::Admin,
                    via: Via::Token(t),
                },
                None => anonymous(Via::Rejected),
            });
        }
        if let Some(secret) = session_cookie(headers) {
            let now = now_ns();
            let s2 = secret.clone();
            let found = blocking(move || auth.session(&s2, now).map_err(auth_err)).await?;
            if let Some(session) = found {
                return Ok(Caller {
                    role: Role::Admin,
                    via: Via::Session { secret, session },
                });
            }
        }
        Ok(anonymous(Via::Nothing))
    }

    fn auth(&self) -> Result<Arc<AuthStore>, ApiError> {
        self.auth.clone().ok_or(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "accounts are not set up on this scanner".into(),
        ))
    }

    fn set_cookie(&self, value: &str, max_age_s: i64) -> HeaderValue {
        let v = format!(
            "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_s}{}",
            if self.secure_cookies { "; Secure" } else { "" }
        );
        HeaderValue::from_str(&v).expect("cookie is ASCII")
    }

    fn index(&self) -> Result<Index, ApiError> {
        // The recorder owns the index; reading must not migrate it or mark
        // rows being recorded as interrupted.
        Index::open_reader(&self.recordings.join("index.sqlite"))
            .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "index unavailable".into()))
    }
}

fn auth_err(e: auth::AuthError) -> ApiError {
    match e {
        auth::AuthError::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, m.into()),
        auth::AuthError::Db(_) | auth::AuthError::Schema(_) => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth database error".into(),
        ),
    }
}

#[derive(Debug)]
pub struct ApiError(StatusCode, Cow<'static, str>);

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
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/session", get(session_info))
        .route("/api/v1/auth/password", post(change_password))
        .route("/api/v1/auth/tokens", get(list_tokens).post(create_token))
        .route("/api/v1/auth/tokens/{id}", delete(revoke_token))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}

async fn status(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LiveStatus>, ApiError> {
    s.caller(&headers).await?.require(s.public.live)?;
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
    s.caller(&headers).await?.require(s.public.recordings)?;
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
    s.caller(&headers).await?.require(s.public.recordings)?;
    let row = blocking(move || s.index()?.get(id).map_err(db_err)).await?;
    match row {
        Some(r) => Ok(Json(serde_json::to_value(recording(r)).unwrap_or_default())),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such recording".into())),
    }
}

/// The WAV, with single-range support so browsers can seek.
async fn get_audio(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> Result<Response, ApiError> {
    s.caller(&headers).await?.require(s.public.audio)?;
    let root = s.recordings.clone();
    let row = blocking(move || s.index()?.get(id).map_err(db_err)).await?;
    let Some(row) = row.filter(|r| r.status == "complete") else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such recording".into()));
    };
    // Paths in the index are relative and written by the recorder; refuse
    // anything that could step outside the recordings folder.
    let rel = std::path::Path::new(&row.wav_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such recording".into()));
    }
    let data = tokio::fs::read(root.join(rel))
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "audio file missing".into()))?;
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

// ---- accounts ----

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct SessionInfo {
    role: Role,
    username: Option<String>,
    /// Send this in `X-CSRF-Token` on requests that change something.
    csrf_token: Option<String>,
    expires_ns: Option<i64>,
    /// "session", "token" or null.
    via: Option<&'static str>,
    accounts_configured: bool,
}

fn limiter_keys(client: &str, username: &str) -> Vec<String> {
    vec![format!("ip:{client}"), format!("user:{username}")]
}

fn too_many(wait_s: u64) -> Response {
    let mut r = ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        format!("too many failed logins; try again in {wait_s} s").into(),
    )
    .into_response();
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(wait_s));
    r
}

/// Check a password with rate limiting; `Ok(Some(admin_id))` on success.
async fn check_password(
    s: &AppState,
    auth: Arc<AuthStore>,
    client: &str,
    username: String,
    password: String,
) -> Result<Result<i64, Response>, ApiError> {
    let keys = limiter_keys(client, &username);
    let now = now_ns();
    if let Some(wait) = s.limiter.lock().unwrap().check(&keys, now) {
        return Ok(Err(too_many(wait)));
    }
    let a2 = auth.clone();
    let u2 = username.clone();
    let ok = blocking(move || a2.verify_password(&u2, &password).map_err(auth_err)).await?;
    let mut lim = s.limiter.lock().unwrap();
    match ok {
        Some(id) => {
            lim.succeeded(&keys);
            Ok(Ok(id))
        }
        None => {
            lim.failed(&keys, now);
            drop(lim);
            auth.audit(now, &username, "login_failed", "", client);
            Ok(Err(ApiError(
                StatusCode::UNAUTHORIZED,
                "wrong username or password".into(),
            )
            .into_response()))
        }
    }
}

async fn login(
    State(s): State<AppState>,
    Client(client): Client,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let auth = s.auth()?;
    let id = match check_password(
        &s,
        auth.clone(),
        &client,
        req.username.clone(),
        req.password,
    )
    .await?
    {
        Ok(id) => id,
        Err(resp) => return Ok(resp),
    };
    let now = now_ns();
    let a2 = auth.clone();
    let new = blocking(move || a2.create_session(id, now).map_err(auth_err)).await?;
    auth.audit(now, &req.username, "login", "", &client);
    let mut resp = Json(SessionInfo {
        role: Role::Admin,
        username: Some(req.username),
        csrf_token: Some(new.csrf),
        expires_ns: Some(new.expires_ns),
        via: Some("session"),
        accounts_configured: true,
    })
    .into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        s.set_cookie(&new.secret, auth::SESSION_MAX_NS / 1_000_000_000),
    );
    Ok(resp)
}

async fn logout(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let c = s.caller(&headers).await?;
    if let Via::Session { secret, session } = &c.via {
        check_csrf(session, &headers)?;
        let auth = s.auth()?;
        let secret = secret.clone();
        let a2 = auth.clone();
        blocking(move || a2.delete_session(&secret).map_err(auth_err)).await?;
        auth.audit(now_ns(), &session.username, "logout", "", &client);
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, s.set_cookie("", 0));
    Ok(resp)
}

async fn session_info(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionInfo>, ApiError> {
    let c = s.caller(&headers).await?;
    let configured = s.auth.is_some();
    Ok(Json(match c.via {
        Via::Session { session, .. } => SessionInfo {
            role: c.role,
            username: Some(session.username),
            csrf_token: Some(session.csrf),
            expires_ns: Some(session.expires_ns),
            via: Some("session"),
            accounts_configured: configured,
        },
        Via::Token(t) => SessionInfo {
            role: c.role,
            username: Some(format!("token:{}", t.name)),
            csrf_token: None,
            expires_ns: t.expires_ns,
            via: Some("token"),
            accounts_configured: configured,
        },
        _ => SessionInfo {
            role: c.role,
            username: None,
            csrf_token: None,
            expires_ns: None,
            via: None,
            accounts_configured: configured,
        },
    }))
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

/// Change your own password. Ends every session of the account, then logs
/// this browser back in with a fresh one.
async fn change_password(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
    Json(req): Json<PasswordChange>,
) -> Result<Response, ApiError> {
    let c = s.caller(&headers).await?;
    c.require(false)?;
    let Via::Session { session, .. } = &c.via else {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "log in with a password to change it".into(),
        ));
    };
    check_csrf(session, &headers)?;
    let auth = s.auth()?;
    let username = session.username.clone();
    let id = match check_password(&s, auth.clone(), &client, username.clone(), req.current).await? {
        Ok(id) => id,
        Err(resp) => return Ok(resp),
    };
    let now = now_ns();
    let a2 = auth.clone();
    let u2 = username.clone();
    let new = blocking(move || {
        a2.set_password(&u2, &req.new).map_err(auth_err)?;
        a2.create_session(id, now).map_err(auth_err)
    })
    .await?;
    auth.audit(now, &username, "password_changed", "", &client);
    let mut resp = Json(serde_json::json!({ "csrf_token": new.csrf })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        s.set_cookie(&new.secret, auth::SESSION_MAX_NS / 1_000_000_000),
    );
    Ok(resp)
}

async fn list_tokens(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Token>>, ApiError> {
    s.caller(&headers).await?.require(false)?;
    let auth = s.auth()?;
    Ok(Json(
        blocking(move || auth.tokens().map_err(auth_err)).await?,
    ))
}

#[derive(Deserialize)]
struct NewTokenRequest {
    name: String,
    /// Days until it stops working; absent for no expiry.
    expires_days: Option<f64>,
}

async fn create_token(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
    Json(req): Json<NewTokenRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let c = s.caller(&headers).await?;
    c.require_write(&headers)?;
    let auth = s.auth()?;
    let now = now_ns();
    let expires = match req.expires_days {
        Some(d) if !(d > 0.0 && d <= 3650.0) => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "expires_days must be between 0 and 3650".into(),
            ));
        }
        Some(d) => Some(now + (d * 86400e9) as i64),
        None => None,
    };
    let a2 = auth.clone();
    let name = req.name.clone();
    let (id, secret) =
        blocking(move || a2.create_token(&name, expires, now).map_err(auth_err)).await?;
    auth.audit(
        now,
        &c.actor(),
        "token_created",
        &format!("{id} {}", req.name),
        &client,
    );
    Ok(Json(serde_json::json!({
        "id": id,
        "name": req.name.trim(),
        "token": secret,
        "expires_ns": expires,
    })))
}

async fn revoke_token(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, ApiError> {
    let c = s.caller(&headers).await?;
    c.require_write(&headers)?;
    let auth = s.auth()?;
    let a2 = auth.clone();
    if !blocking(move || a2.revoke_token(id).map_err(auth_err)).await? {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such token".into()));
    }
    auth.audit(
        now_ns(),
        &c.actor(),
        "token_revoked",
        &id.to_string(),
        &client,
    );
    Ok(StatusCode::NO_CONTENT)
}

fn db_err(_: scanner_store::index::IndexError) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "index error".into())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "worker failed".into()))?
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
                let app =
                    router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
                if let Err(e) = axum::serve(listener, app).await {
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

    const PASSWORD: &str = "correct horse battery";

    /// State with one recording, an admin "masen" and an API token.
    fn setup(public: Public) -> (tempfile::TempDir, AppState, i64, String) {
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
        let auth = AuthStore::in_memory().unwrap();
        auth.create_admin("masen", PASSWORD, now_ns()).unwrap();
        let (_, token) = auth.create_token("tests", None, now_ns()).unwrap();
        let state = AppState::new(
            Arc::new(RwLock::new(LiveStatus {
                source: "test".into(),
                ..Default::default()
            })),
            dir.path().to_owned(),
            public,
            Some(Arc::new(auth)),
        );
        (dir, state, id, token)
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Reply {
        fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).unwrap()
        }
        fn cookie(&self) -> Option<String> {
            self.headers
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(';').next())
                .and_then(|kv| kv.split_once('='))
                .map(|(_, v)| v.to_string())
        }
    }

    async fn send(state: &AppState, req: Request<Body>) -> Reply {
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    fn json_req(
        method: &str,
        uri: &str,
        body: serde_json::Value,
        cookie: Option<&str>,
        csrf: Option<&str>,
    ) -> Request<Body> {
        let mut r = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(c) = cookie {
            r = r.header(header::COOKIE, format!("{SESSION_COOKIE}={c}"));
        }
        if let Some(t) = csrf {
            r = r.header(CSRF_HEADER, t);
        }
        r.body(Body::from(body.to_string())).unwrap()
    }

    async fn login(state: &AppState, user: &str, pw: &str) -> Reply {
        send(
            state,
            json_req(
                "POST",
                "/api/v1/auth/login",
                serde_json::json!({"username": user, "password": pw}),
                None,
                None,
            ),
        )
        .await
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
        let r = send(state, req.body(Body::empty()).unwrap()).await;
        (r.status, r.body)
    }

    async fn get_with_cookie(state: &AppState, uri: &str, cookie: &str) -> StatusCode {
        let req = Request::get(uri)
            .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"))
            .body(Body::empty())
            .unwrap();
        send(state, req).await.status
    }

    /// Every read route × caller × public setting. A new route must be
    /// added here (write routes are covered by `write_routes_need_admin_and_csrf`).
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
            "/api/v1/auth/tokens",
        ];
        #[derive(Clone, Copy, Debug)]
        enum Who {
            Nobody,
            WrongToken,
            Token,
            Session,
            ExpiredCookie,
        }
        // (public, caller, expected per route)
        let cases: [(Public, Who, [S; 5]); 11] = [
            (
                defaults,
                Who::Nobody,
                [
                    S::OK,
                    S::UNAUTHORIZED,
                    S::UNAUTHORIZED,
                    S::UNAUTHORIZED,
                    S::UNAUTHORIZED,
                ],
            ),
            (
                defaults,
                Who::WrongToken,
                [
                    S::OK,
                    S::FORBIDDEN,
                    S::FORBIDDEN,
                    S::FORBIDDEN,
                    S::FORBIDDEN,
                ],
            ),
            (
                defaults,
                Who::ExpiredCookie,
                [
                    S::OK,
                    S::UNAUTHORIZED,
                    S::UNAUTHORIZED,
                    S::UNAUTHORIZED,
                    S::UNAUTHORIZED,
                ],
            ),
            (defaults, Who::Token, [S::OK; 5]),
            (defaults, Who::Session, [S::OK; 5]),
            (
                open,
                Who::Nobody,
                [S::OK, S::OK, S::OK, S::OK, S::UNAUTHORIZED],
            ),
            (closed, Who::Nobody, [S::UNAUTHORIZED; 5]),
            (closed, Who::WrongToken, [S::FORBIDDEN; 5]),
            (closed, Who::ExpiredCookie, [S::UNAUTHORIZED; 5]),
            (closed, Who::Token, [S::OK; 5]),
            (closed, Who::Session, [S::OK; 5]),
        ];
        for (public, who, expect) in cases {
            let (_dir, state, id, token) = setup(public);
            let cookie = login(&state, "masen", PASSWORD).await.cookie().unwrap();
            for (route, want) in routes.iter().zip(expect) {
                let uri = route.replace("{id}", &id.to_string());
                let got = match who {
                    Who::Nobody => get(&state, &uri, None, None).await.0,
                    Who::WrongToken => get(&state, &uri, Some("scn_wrong"), None).await.0,
                    Who::Token => get(&state, &uri, Some(&token), None).await.0,
                    Who::Session => get_with_cookie(&state, &uri, &cookie).await,
                    Who::ExpiredCookie => get_with_cookie(&state, &uri, "stale").await,
                };
                assert_eq!(got, want, "{uri} public={public:?} who={who:?}");
            }
        }
    }

    #[tokio::test]
    async fn login_sets_a_hardened_cookie_and_logout_ends_it() {
        let (_dir, state, _, _) = setup(Public::default());
        let r = login(&state, "masen", "wrong password!").await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert!(r.cookie().is_none());
        let r = login(&state, "masen", PASSWORD).await;
        assert_eq!(r.status, StatusCode::OK);
        let set = r.headers[header::SET_COOKIE].to_str().unwrap().to_string();
        for attr in ["HttpOnly", "SameSite=Strict", "Secure", "Path=/"] {
            assert!(set.contains(attr), "{set}");
        }
        let cookie = r.cookie().unwrap();
        let csrf = r.json()["csrf_token"].as_str().unwrap().to_string();
        // Who am I.
        let req = Request::get("/api/v1/auth/session")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"))
            .body(Body::empty())
            .unwrap();
        let me = send(&state, req).await.json();
        assert_eq!(me["role"], "admin");
        assert_eq!(me["username"], "masen");
        assert_eq!(me["csrf_token"], csrf);
        // Logout needs the CSRF token.
        let r = send(
            &state,
            json_req(
                "POST",
                "/api/v1/auth/logout",
                serde_json::json!({}),
                Some(&cookie),
                None,
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = send(
            &state,
            json_req(
                "POST",
                "/api/v1/auth/logout",
                serde_json::json!({}),
                Some(&cookie),
                Some(&csrf),
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert!(
            r.headers[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        assert_eq!(
            get_with_cookie(&state, "/api/v1/recordings", &cookie).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn write_routes_need_admin_and_csrf() {
        let (_dir, state, _, token) = setup(Public::default());
        let r = login(&state, "masen", PASSWORD).await;
        let cookie = r.cookie().unwrap();
        let csrf = r.json()["csrf_token"].as_str().unwrap().to_string();
        let body = serde_json::json!({"name": "cron", "expires_days": 30});
        // Anonymous, cookie without CSRF, cookie with the wrong CSRF: refused.
        let r = send(
            &state,
            json_req("POST", "/api/v1/auth/tokens", body.clone(), None, None),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = send(
            &state,
            json_req(
                "POST",
                "/api/v1/auth/tokens",
                body.clone(),
                Some(&cookie),
                None,
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = send(
            &state,
            json_req(
                "POST",
                "/api/v1/auth/tokens",
                body.clone(),
                Some(&cookie),
                Some("nope"),
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        // Cookie + CSRF works and returns the secret once.
        let r = send(
            &state,
            json_req(
                "POST",
                "/api/v1/auth/tokens",
                body.clone(),
                Some(&cookie),
                Some(&csrf),
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let new = r.json();
        let secret = new["token"].as_str().unwrap().to_string();
        assert!(secret.starts_with("scn_"));
        assert_eq!(
            get(&state, "/api/v1/recordings", Some(&secret), None)
                .await
                .0,
            StatusCode::OK
        );
        // A bearer token needs no CSRF header, and can revoke.
        let mut req = Request::delete(format!("/api/v1/auth/tokens/{}", new["id"]))
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        req = req.header(header::CONTENT_TYPE, "application/json");
        let r = send(&state, req.body(Body::empty()).unwrap()).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert_eq!(
            get(&state, "/api/v1/recordings", Some(&secret), None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        // The listing never includes secrets.
        let (_, body) = get(&state, "/api/v1/auth/tokens", Some(&token), None).await;
        let text = String::from_utf8(body).unwrap();
        assert!(
            !text.contains(&secret[4..]) && text.contains("cron"),
            "{text}"
        );
        // Audit trail.
        let log = state.auth.as_ref().unwrap().recent_audit(10).unwrap();
        let actions: Vec<&str> = log.iter().map(|e| e.action.as_str()).collect();
        assert!(actions.contains(&"token_created") && actions.contains(&"token_revoked"));
    }

    #[tokio::test]
    async fn password_change_ends_other_sessions() {
        let (_dir, state, _, _) = setup(Public::default());
        let a = login(&state, "masen", PASSWORD).await;
        let b = login(&state, "masen", PASSWORD).await;
        let (ca, csrf) = (
            a.cookie().unwrap(),
            a.json()["csrf_token"].as_str().unwrap().to_string(),
        );
        let cb = b.cookie().unwrap();
        let change = |current: &str| serde_json::json!({"current": current, "new": "an even better passphrase"});
        let r = send(
            &state,
            json_req(
                "POST",
                "/api/v1/auth/password",
                change("wrong one!!"),
                Some(&ca),
                Some(&csrf),
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = send(
            &state,
            json_req(
                "POST",
                "/api/v1/auth/password",
                change(PASSWORD),
                Some(&ca),
                Some(&csrf),
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let fresh = r.cookie().unwrap();
        assert_eq!(
            get_with_cookie(&state, "/api/v1/recordings", &fresh).await,
            StatusCode::OK
        );
        for old in [&ca, &cb] {
            assert_eq!(
                get_with_cookie(&state, "/api/v1/recordings", old).await,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            login(&state, "masen", PASSWORD).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            login(&state, "masen", "an even better passphrase")
                .await
                .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn repeated_failed_logins_are_throttled() {
        let (_dir, state, _, _) = setup(Public::default());
        for _ in 0..LoginLimiter::FREE_FAILURES {
            assert_eq!(
                login(&state, "masen", "guess guess guess").await.status,
                StatusCode::UNAUTHORIZED
            );
        }
        // The next failure arms the lockout; after that even the right
        // password waits.
        assert_eq!(
            login(&state, "masen", "guess guess guess").await.status,
            StatusCode::UNAUTHORIZED
        );
        let r = login(&state, "masen", PASSWORD).await;
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers.contains_key(header::RETRY_AFTER));
        // Unknown usernames count as failures too (and leak nothing).
        let r = login(&state, "nobody", "guess guess guess").await;
        assert_eq!(
            r.status,
            StatusCode::TOO_MANY_REQUESTS,
            "same client address"
        );
    }

    #[tokio::test]
    async fn without_accounts_nobody_is_admin() {
        let (_dir, mut state, _, token) = setup(Public::default());
        state.auth = None;
        assert_eq!(
            get(&state, "/api/v1/recordings", Some(&token), None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            login(&state, "masen", PASSWORD).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn lists_and_serves_audio_with_ranges() {
        let (_dir, state, id, token) = setup(Public::default());
        let token = token.as_str();
        let (st, body) = get(
            &state,
            "/api/v1/recordings?freq_min=146e6&freq_max=147e6",
            Some(token),
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
            Some(token),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 0);

        let audio = format!("/api/v1/recordings/{id}/audio");
        let (st, body) = get(&state, &audio, Some(token), None).await;
        assert_eq!(
            (st, body.as_slice()),
            (StatusCode::OK, b"RIFF0123456789".as_slice())
        );
        let (st, body) = get(&state, &audio, Some(token), Some("bytes=4-7")).await;
        assert_eq!(
            (st, body.as_slice()),
            (StatusCode::PARTIAL_CONTENT, b"0123".as_slice())
        );
        let (st, body) = get(&state, &audio, Some(token), Some("bytes=-2")).await;
        assert_eq!(
            (st, body.as_slice()),
            (StatusCode::PARTIAL_CONTENT, b"89".as_slice())
        );
        let (st, _) = get(&state, "/api/v1/recordings/999", Some(token), None).await;
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
