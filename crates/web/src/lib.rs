//! Web API built into the scanner binary (PLAN.md 3.5a).
//!
//! The browser UI ([`ui`]) and `/api/v1` with role guards.
//!
//! | Route | Needs |
//! |---|---|
//! | `GET /api/v1/status` | anonymous if `public.live` (default on), else admin |
//! | `GET /api/v1/recordings` | anonymous if `public.recordings` (default off), else admin |
//! | `GET /api/v1/recordings/timeline` | same as the list, same filters: compact `[id, freq_hz, start_ms, secs]` rows for the timeline (`limit` default 2000) |
//! | `GET /api/v1/recordings/{id}` | same as the list |
//! | `PATCH /api/v1/recordings/{id}` | admin (+ CSRF): `{"starred": bool}`; starred recordings are kept by retention |
//! | `DELETE /api/v1/recordings/{id}` | admin (+ CSRF): delete the recording's files and its row |
//! | `GET /api/v1/recordings/{id}/audio` | anonymous if `public.audio` (default off), else admin: the Opus copy, or the WAV with `?format=wav` or when there's no copy |
//! | `GET /api/v1/packets` | same as the recordings list: decoded AX.25 frames, newest first (`before_id`, `limit`) |
//! | `GET /api/v1/channels` | same as status |
//! | `GET /api/v1/storage` | admin: disk usage, budget, free space and what retention deleted |
//! | `GET /api/v1/egress` | admin: bytes sent per role this month and earlier ones (see [`egress`]) |
//! | `PATCH /api/v1/channels/{freq_hz}` | admin (+ CSRF): `{"locked_out": bool, "priority": bool}`, either or both |
//! | `DELETE /api/v1/channels/{freq_hz}/override` | admin (+ CSRF): back to the config file |
//! | `GET /api/v1/live` (WebSocket) | same as status; see [`live`] |
//! | `GET /api/v1/audio` (WebSocket) | same as recording audio; see [`audio`] |
//! | `GET /`, `/assets/*` | anyone: the browser UI (see [`ui`]) |
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

pub mod audio;
pub mod auth;
pub mod egress;
pub mod live;
pub mod ui;

use auth::{AuthStore, LoginLimiter, Session, Token, constant_time_eq, now_ns};
use axum::extract::{
    DefaultBodyLimit, FromRequestParts, Path as UrlPath, Query as UrlQuery, State,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use scanner_store::settings::{Override, Settings};
use scanner_store::{Index, Query, Row};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

pub use live::{ChannelInfo, ChannelsHandle, Spectrum, SpectrumHandle};

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
    /// Display spectrum, published by the DSP thread.
    pub spectrum: SpectrumHandle,
    /// The watched channels.
    pub channels: ChannelsHandle,
    /// Lockout and priority overrides; `None` means they can't be edited.
    pub settings: Option<Arc<Settings>>,
    /// Commands to the DSP thread.
    pub control: Option<ControlSender>,
    /// Live listening; `None` when the scanner offers none.
    pub audio: Option<Arc<audio::AudioHub>>,
    /// Disk budget and usage; `None` when retention is off.
    pub storage: Option<Arc<scanner_store::retention::DiskUsage>>,
    /// Bytes sent per role and month, and the anonymous budget; `None`
    /// counts nothing.
    pub egress: Option<Arc<egress::Egress>>,
    /// Packets as they are decoded, for the Live page (see [`live`]).
    pub packets: PacketFeed,
}

/// Decoded packets, published by the scanner as they arrive. Sending with no
/// one subscribed is fine (the packet is dropped).
pub type PacketFeed = tokio::sync::broadcast::Sender<Arc<scanner_store::index::PacketRow>>;

/// A packet feed with room for a burst of packets per slow client.
pub fn packet_feed() -> PacketFeed {
    tokio::sync::broadcast::channel(64).0
}

/// A command for the running scanner.
#[derive(Debug)]
pub enum Control {
    /// The channel overrides changed: apply this full set, then reply.
    Overrides {
        list: Vec<Override>,
        done: tokio::sync::oneshot::Sender<()>,
    },
}

pub type ControlSender = std::sync::mpsc::Sender<Control>;

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
            spectrum: Default::default(),
            channels: Default::default(),
            settings: None,
            control: None,
            audio: None,
            storage: None,
            egress: None,
            packets: packet_feed(),
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

    fn egress_level(&self, role: Role) -> egress::Level {
        self.egress
            .as_ref()
            .map_or(egress::Level::Ok, |e| e.level(role))
    }

    /// A counter for one connection by `role`.
    fn meter(&self, role: Role) -> egress::Meter {
        match &self.egress {
            Some(e) => e.meter(role),
            None => egress::Meter::none(role),
        }
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
    let mut r = Router::new();
    for path in ui::paths() {
        r = r.route(path, get(ui::asset));
    }
    r.route("/api/v1/status", get(status))
        .route("/api/v1/channels", get(channels))
        .route("/api/v1/storage", get(storage))
        .route("/api/v1/egress", get(egress_report))
        .route(
            "/api/v1/channels/{freq_hz}",
            axum::routing::patch(edit_channel),
        )
        .route("/api/v1/channels/{freq_hz}/override", delete(reset_channel))
        .route("/api/v1/live", get(live::live_ws))
        .route("/api/v1/audio", get(audio::audio_ws))
        .route("/api/v1/recordings", get(list_recordings))
        .route("/api/v1/recordings/timeline", get(recordings_timeline))
        .route("/api/v1/packets", get(list_packets))
        .route(
            "/api/v1/recordings/{id}",
            get(get_recording)
                .patch(edit_recording)
                .delete(delete_recording),
        )
        .route("/api/v1/recordings/{id}/audio", get(get_audio))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/session", get(session_info))
        .route("/api/v1/auth/password", post(change_password))
        .route("/api/v1/auth/tokens", get(list_tokens).post(create_token))
        .route("/api/v1/auth/tokens/{id}", delete(revoke_token))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(axum::middleware::from_fn(ui::security_headers))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            count_egress,
        ))
        .with_state(state)
}

/// Count each HTTP response against its caller's role. WebSockets count
/// their own messages (see [`egress::Sock`]).
async fn count_egress(
    State(s): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(eg) = s.egress.clone() else {
        return next.run(req).await;
    };
    // Only a real login counts as admin, so a made-up cookie can't move
    // anonymous traffic off the anonymous budget.
    let creds = bearer(req.headers()).is_some() || session_cookie(req.headers()).is_some();
    let role = if creds {
        s.caller(req.headers())
            .await
            .map_or(Role::Anonymous, |c| c.role)
    } else {
        Role::Anonymous
    };
    let resp = next.run(req).await;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        use axum::body::HttpBody;
        let body = resp.body().size_hint().exact().unwrap_or(0);
        // Status line, header lines and the blank line.
        let head: usize = 17
            + resp
                .headers()
                .iter()
                .map(|(k, v)| k.as_str().len() + v.len() + 4)
                .sum::<usize>();
        eg.add(role, body + head as u64);
    }
    resp
}

/// Data sent this month and in earlier ones (admin).
async fn egress_report(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.caller(&headers).await?.require(false)?;
    let current = s.egress.as_ref().map(|e| e.status());
    let settings = s.settings.clone();
    let history = blocking(move || {
        Ok(match settings {
            Some(st) => st.egress(12).map_err(settings_err)?,
            None => Vec::new(),
        })
    })
    .await?;
    Ok(Json(serde_json::json!({
        "current": current,
        "history": history,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelEdit {
    locked_out: Option<bool>,
    priority: Option<bool>,
}

fn settings_err(_: scanner_store::settings::SettingsError) -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "settings database error".into(),
    )
}

impl AppState {
    fn settings(&self) -> Result<Arc<Settings>, ApiError> {
        self.settings.clone().ok_or(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "channel settings can't be changed on this scanner".into(),
        ))
    }

    /// Hand the full override list to the DSP thread and wait until it has
    /// applied it (so the next channel list reflects the change).
    async fn apply_overrides(&self, settings: Arc<Settings>) -> Result<(), ApiError> {
        let list = blocking(move || settings.overrides().map_err(settings_err)).await?;
        let Some(tx) = &self.control else {
            return Ok(());
        };
        let (done, rx) = tokio::sync::oneshot::channel();
        tx.send(Control::Overrides { list, done }).map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the scanner has stopped".into(),
            )
        })?;
        match tokio::time::timeout(std::time::Duration::from_secs(3), rx).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "saved, but the scanner didn't confirm it in time".into(),
            )),
        }
    }

    fn channel_at(&self, freq_hz: i64) -> Option<ChannelInfo> {
        self.channels.read().ok().and_then(|g| {
            g.iter()
                .find(|c| scanner_core::config::same_freq(c.freq_hz, freq_hz as f64))
                .cloned()
        })
    }
}

/// Lock out a frequency or make it a priority channel, or undo either.
/// Priority needs a watched channel there; a lockout can go anywhere in the
/// capture (an off-grid signal or a birdie) and then covers +/- 5 kHz.
async fn edit_channel(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
    UrlPath(freq_hz): UrlPath<i64>,
    Json(req): Json<ChannelEdit>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let c = s.caller(&headers).await?;
    c.require_write(&headers)?;
    let settings = s.settings()?;
    if req.locked_out.is_none() && req.priority.is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "send locked_out and/or priority".into(),
        ));
    }
    let watched = s.channel_at(freq_hz).filter(|c| c.kind != "lockout");
    if req.priority.is_some() && watched.is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "priority can only be set on a watched channel".into(),
        ));
    }
    let snap = s.status.read().map(|g| g.clone()).unwrap_or_default();
    let half = snap.sample_rate * 0.45;
    if watched.is_none()
        && snap.sample_rate > 0.0
        && (freq_hz as f64 - snap.center_freq_hz).abs() > half
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "that frequency is outside what the radio receives".into(),
        ));
    }
    let actor = c.actor();
    let (st, a2) = (settings.clone(), actor.clone());
    let o = blocking(move || {
        st.set(freq_hz, req.locked_out, req.priority, now_ns(), &a2)
            .map_err(settings_err)
    })
    .await?;
    if let Some(auth) = &s.auth {
        let what = [
            req.locked_out
                .map(|l| if l { "locked out" } else { "unlocked" }),
            req.priority
                .map(|p| if p { "priority on" } else { "priority off" }),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        auth.audit(
            now_ns(),
            &actor,
            "channel_changed",
            &format!("{:.4} MHz {what}", freq_hz as f64 / 1e6),
            &client,
        );
    }
    s.apply_overrides(settings).await?;
    Ok(Json(serde_json::json!({
        "override": o,
        "channel": s.channel_at(freq_hz),
    })))
}

async fn reset_channel(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
    UrlPath(freq_hz): UrlPath<i64>,
) -> Result<StatusCode, ApiError> {
    let c = s.caller(&headers).await?;
    c.require_write(&headers)?;
    let settings = s.settings()?;
    let st = settings.clone();
    if !blocking(move || st.clear(freq_hz).map_err(settings_err)).await? {
        return Err(ApiError(StatusCode::NOT_FOUND, "no override there".into()));
    }
    if let Some(auth) = &s.auth {
        auth.audit(
            now_ns(),
            &c.actor(),
            "channel_reset",
            &format!("{:.4} MHz", freq_hz as f64 / 1e6),
            &client,
        );
    }
    s.apply_overrides(settings).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn channels(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ChannelInfo>>, ApiError> {
    s.caller(&headers).await?.require(s.public.live)?;
    Ok(Json(
        s.channels.read().map(|g| g.clone()).unwrap_or_default(),
    ))
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
    /// Exact tone label: `100.0` or `D023N`.
    pub tone: Option<String>,
    /// DTMF digits the recording must contain; `any` for any digits.
    pub dtmf: Option<String>,
    /// Only starred (`true`) or unstarred (`false`) recordings.
    pub starred: Option<bool>,
    /// Digital calls to this talkgroup or radio id.
    pub dst_id: Option<i64>,
    /// Digital calls from this radio id.
    pub src_id: Option<i64>,
    pub before_id: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct Recording {
    #[serde(flatten)]
    row: Row,
    /// The Opus copy when there is one, else the WAV.
    audio_url: String,
    /// Always the WAV.
    wav_url: String,
}

fn recording(r: Row) -> Recording {
    Recording {
        audio_url: format!("/api/v1/recordings/{}/audio", r.id),
        wav_url: format!("/api/v1/recordings/{}/audio?format=wav", r.id),
        row: r,
    }
}

#[derive(Deserialize)]
struct AudioParams {
    /// `wav`, `opus` or `amb` (a digital call's raw voice frames); left
    /// out, the Opus copy if there is one.
    format: Option<String>,
}

fn list_query(p: ListParams, default_limit: usize) -> Query {
    Query {
        freq_min: p.freq_min,
        freq_max: p.freq_max,
        since_ns: p.since.map(|t| (t * 1e9) as i64),
        until_ns: p.until.map(|t| (t * 1e9) as i64),
        min_secs: p.min_secs,
        tone: p.tone.filter(|t| !t.is_empty()),
        dtmf: p
            .dtmf
            .map(|d| d.trim().to_ascii_uppercase())
            .filter(|d| !d.is_empty())
            .map(|d| if d == "ANY" { String::new() } else { d }),
        starred: p.starred,
        dst_id: p.dst_id,
        src_id: p.src_id,
        before_id: p.before_id,
        limit: p.limit.unwrap_or(default_limit),
    }
}

async fn list_recordings(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlQuery(p): UrlQuery<ListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.caller(&headers).await?.require(s.public.recordings)?;
    let q = list_query(p, 100);
    let rows = blocking(move || s.index()?.search(&q).map_err(db_err)).await?;
    let next = rows.last().map(|r| r.id);
    let items: Vec<Recording> = rows.into_iter().map(recording).collect();
    Ok(Json(
        serde_json::json!({ "items": items, "next_before_id": next }),
    ))
}

/// When and where the recordings matching the list's filters were, for the
/// timeline above the recordings table. Compact, since it can be thousands
/// of rows: `items` are `[id, freq_hz, start_ms, secs]`, newest first, and
/// `truncated` says the limit (default 2000) cut off older ones.
async fn recordings_timeline(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlQuery(p): UrlQuery<ListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.caller(&headers).await?.require(s.public.recordings)?;
    let q = list_query(p, 2000);
    let limit = q.limit.clamp(1, Index::TIMELINE_MAX);
    let spans = blocking(move || s.index()?.timeline(&q).map_err(db_err)).await?;
    let truncated = spans.len() == limit;
    let items: Vec<serde_json::Value> = spans
        .into_iter()
        .map(|sp| {
            serde_json::json!([
                sp.id,
                sp.freq_hz.round() as i64,
                sp.start_ns / 1_000_000,
                (sp.secs * 10.0).round() / 10.0
            ])
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "items": items, "truncated": truncated }),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct PacketParams {
    before_id: Option<i64>,
    limit: Option<usize>,
}

async fn list_packets(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlQuery(p): UrlQuery<PacketParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.caller(&headers).await?.require(s.public.recordings)?;
    let limit = p.limit.unwrap_or(100);
    let rows = blocking(move || s.index()?.packets(p.before_id, limit).map_err(db_err)).await?;
    let next = (rows.len() == limit.clamp(1, 1000))
        .then(|| rows.last().map(|r| r.id))
        .flatten();
    Ok(Json(
        serde_json::json!({ "items": rows, "next_before_id": next }),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordingEdit {
    starred: bool,
}

/// Star or unstar a recording (admin). Starred recordings are never
/// deleted by retention.
async fn edit_recording(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
    Json(req): Json<RecordingEdit>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let c = s.caller(&headers).await?;
    c.require_write(&headers)?;
    let s2 = s.clone();
    let row = blocking(move || {
        let ix = s2.index()?;
        if !ix.set_starred(id, req.starred).map_err(db_err)? {
            return Ok(None);
        }
        ix.get(id).map_err(db_err)
    })
    .await?
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such recording".into()))?;
    if let Some(auth) = &s.auth {
        auth.audit(
            now_ns(),
            &c.actor(),
            if req.starred {
                "recording_starred"
            } else {
                "recording_unstarred"
            },
            &format!("#{id} {:.4} MHz", row.freq_hz / 1e6),
            &client,
        );
    }
    Ok(Json(
        serde_json::to_value(recording(row)).unwrap_or_default(),
    ))
}

/// Delete a recording's files and its row (admin), starred or not.
async fn delete_recording(
    State(s): State<AppState>,
    Client(client): Client,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, ApiError> {
    use scanner_store::retention::DeleteError;
    let c = s.caller(&headers).await?;
    c.require_write(&headers)?;
    let s2 = s.clone();
    let (freq, freed) = blocking(move || {
        let ix = s2.index()?;
        let freq = ix.get(id).map_err(db_err)?.map(|r| r.freq_hz);
        match scanner_store::retention::delete_recording(&s2.recordings, &ix, id) {
            Ok(n) => Ok((freq.unwrap_or_default(), n)),
            Err(DeleteError::NotFound) => {
                Err(ApiError(StatusCode::NOT_FOUND, "no such recording".into()))
            }
            Err(e @ DeleteError::Recording) => {
                Err(ApiError(StatusCode::CONFLICT, e.to_string().into()))
            }
            Err(DeleteError::Index(e)) => Err(db_err(e)),
        }
    })
    .await?;
    if let Some(u) = &s.storage {
        u.released(freed);
    }
    if let Some(auth) = &s.auth {
        auth.audit(
            now_ns(),
            &c.actor(),
            "recording_deleted",
            &format!("#{id} {:.4} MHz", freq / 1e6),
            &client,
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The recording's audio, with single-range support so browsers can seek.
async fn get_audio(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
    UrlQuery(p): UrlQuery<AudioParams>,
) -> Result<Response, ApiError> {
    let caller = s.caller(&headers).await?;
    caller.require(s.public.audio)?;
    if s.egress_level(caller.role) == egress::Level::Spent {
        return Err(budget_spent());
    }
    let want_amb = p.format.as_deref() == Some("amb");
    let want_opus = match p.format.as_deref() {
        None | Some("amb") => None,
        Some("wav") => Some(false),
        Some("opus") => Some(true),
        Some(_) => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "format is wav, opus or amb".into(),
            ));
        }
    };
    let root = s.recordings.clone();
    let row = blocking(move || s.index()?.get(id).map_err(db_err)).await?;
    let Some(row) = row.filter(|r| r.status == "complete") else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such recording".into()));
    };
    let amb = scanner_store::recorder::amb_path(&row.wav_path);
    let (path, mime) = match (want_opus, row.opus_path.as_deref()) {
        _ if want_amb => {
            if row.amb_bytes.is_none() {
                return Err(ApiError(StatusCode::NOT_FOUND, "no voice frames".into()));
            }
            (amb.as_str(), "application/octet-stream")
        }
        (Some(false), _) | (None, None) => (row.wav_path.as_str(), "audio/wav"),
        (Some(true) | None, Some(p)) => (p, "audio/ogg; codecs=opus"),
        (Some(true), None) => {
            return Err(ApiError(StatusCode::NOT_FOUND, "no Opus copy".into()));
        }
    };
    // Paths in the index are relative and written by the recorder; refuse
    // anything that could step outside the recordings folder.
    let rel = std::path::Path::new(path);
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
    resp_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    resp_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // A finished recording never changes, but who may fetch it can, so
    // only the viewer's own browser keeps it, and not for long.
    resp_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
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

fn budget_spent() -> ApiError {
    ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "this scanner's data budget for visitors is used up for the month".into(),
    )
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
    /// What anonymous visitors may see.
    public: Public,
    /// Where this caller stands against the monthly data budget.
    budget: egress::Level,
    /// When the budget starts again, if there is one.
    budget_resets_ns: Option<i64>,
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
        public: s.public,
        budget: egress::Level::Ok,
        budget_resets_ns: None,
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
    let budget = s.egress_level(c.role);
    let budget_resets_ns = s
        .egress
        .as_ref()
        .map(|e| e.status())
        .filter(|st| st.budget_bytes > 0)
        .map(|st| st.resets_ns);
    Ok(Json(match c.via {
        Via::Session { session, .. } => SessionInfo {
            role: c.role,
            username: Some(session.username),
            csrf_token: Some(session.csrf),
            expires_ns: Some(session.expires_ns),
            via: Some("session"),
            accounts_configured: configured,
            public: s.public,
            budget,
            budget_resets_ns,
        },
        Via::Token(t) => SessionInfo {
            role: c.role,
            username: Some(format!("token:{}", t.name)),
            csrf_token: None,
            expires_ns: t.expires_ns,
            via: Some("token"),
            accounts_configured: configured,
            public: s.public,
            budget,
            budget_resets_ns,
        },
        _ => SessionInfo {
            role: c.role,
            username: None,
            csrf_token: None,
            expires_ns: None,
            via: None,
            accounts_configured: configured,
            public: s.public,
            budget,
            budget_resets_ns,
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

/// Disk usage of the recordings, and the retention budget when there is one.
async fn storage(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.caller(&headers).await?.require(false)?;
    let retention = s.storage.as_ref().map(|u| u.snapshot());
    let (used, oldest) = blocking(move || {
        let ix = s.index()?;
        Ok((
            ix.usage_bytes().map_err(db_err)?,
            ix.oldest_complete_ns().map_err(db_err)?,
        ))
    })
    .await?;
    Ok(Json(serde_json::json!({
        "used_bytes": used,
        "oldest_ns": oldest,
        "retention": retention,
    })))
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
                if let Some(e) = state.egress.clone() {
                    tokio::spawn(async move {
                        let mut t = tokio::time::interval(std::time::Duration::from_secs(10));
                        loop {
                            t.tick().await;
                            let e = e.clone();
                            let _ = tokio::task::spawn_blocking(move || e.flush()).await;
                        }
                    });
                }
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
        let mut state = state;
        state.audio = Some(audio::AudioHub::new());
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
        // (route, which column of `expect` applies). Routes guarded like
        // status share its column.
        let routes = [
            ("/api/v1/status", 0),
            ("/api/v1/channels", 0),
            ("/api/v1/live", 0),
            ("/api/v1/recordings", 1),
            ("/api/v1/packets", 1),
            ("/api/v1/recordings/{id}", 2),
            ("/api/v1/recordings/{id}/audio", 3),
            ("/api/v1/audio", 3),
            ("/api/v1/auth/tokens", 4),
            ("/api/v1/storage", 4),
            ("/api/v1/egress", 4),
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
            for (route, col) in routes {
                let want = expect[col];
                let uri = route.replace("{id}", &id.to_string());
                let mut got = match who {
                    Who::Nobody => get(&state, &uri, None, None).await.0,
                    Who::WrongToken => get(&state, &uri, Some("scn_wrong"), None).await.0,
                    Who::Token => get(&state, &uri, Some(&token), None).await.0,
                    Who::Session => get_with_cookie(&state, &uri, &cookie).await,
                    Who::ExpiredCookie => get_with_cookie(&state, &uri, "stale").await,
                };
                // A plain GET that passes the guard is then refused as a
                // bad WebSocket handshake; that is the "allowed" answer.
                if route.starts_with("/api/v1/live") && got == S::BAD_REQUEST
                    || route == "/api/v1/audio" && got == S::BAD_REQUEST
                {
                    got = S::OK;
                }
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
    async fn star_and_delete_recordings() {
        let (dir, state, id, token) = setup(Public {
            recordings: true,
            ..Public::default()
        });
        let uri = format!("/api/v1/recordings/{id}");
        let body = serde_json::json!({"starred": true});
        let bearer = |method: &str, uri: &str, body: Option<serde_json::Value>| {
            let mut req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"));
            let b = match body {
                Some(v) => {
                    req = req.header(header::CONTENT_TYPE, "application/json");
                    Body::from(v.to_string())
                }
                None => Body::empty(),
            };
            req.body(b).unwrap()
        };
        // Visitors may list recordings here but not change them.
        let r = send(&state, json_req("PATCH", &uri, body.clone(), None, None)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = send(&state, Request::delete(&uri).body(Body::empty()).unwrap()).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        // A session needs its CSRF token.
        let l = login(&state, "masen", PASSWORD).await;
        let cookie = l.cookie().unwrap();
        let csrf = l.json()["csrf_token"].as_str().unwrap().to_string();
        let r = send(
            &state,
            json_req("PATCH", &uri, body.clone(), Some(&cookie), None),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = send(
            &state,
            json_req("PATCH", &uri, body.clone(), Some(&cookie), Some(&csrf)),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.json()["starred"], true);
        let count = |q: &'static str| {
            let state = state.clone();
            async move {
                let (_, body) = get(&state, q, None, None).await;
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["items"]
                    .as_array()
                    .unwrap()
                    .len()
            }
        };
        assert_eq!(count("/api/v1/recordings?starred=true").await, 1);
        assert_eq!(count("/api/v1/recordings?starred=false").await, 0);
        let r = send(
            &state,
            bearer("PATCH", "/api/v1/recordings/999", Some(body.clone())),
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        // Delete takes the file and the row.
        let r = send(&state, bearer("DELETE", &uri, None)).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert!(!dir.path().join("2026/11/22/a.wav").exists());
        assert_eq!(count("/api/v1/recordings").await, 0);
        let r = send(&state, bearer("DELETE", &uri, None)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let log = state.auth.as_ref().unwrap().recent_audit(10).unwrap();
        let actions: Vec<&str> = log.iter().map(|e| e.action.as_str()).collect();
        assert!(actions.contains(&"recording_starred") && actions.contains(&"recording_deleted"));
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
    async fn timeline_takes_the_list_filters() {
        let (_dir, state, id, token) = setup(Public::default());
        let (st, body) = get(
            &state,
            "/api/v1/recordings/timeline?freq_min=146e6&freq_max=147e6",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["items"][0][0], id);
        assert_eq!(v["items"][0][1], 146_520_000);
        assert_eq!(v["truncated"], false);
        let (_, body) = get(
            &state,
            "/api/v1/recordings/timeline?freq_min=147e6",
            Some(&token),
            None,
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 0);
        // Private like the list.
        let (st, _) = get(&state, "/api/v1/recordings/timeline", None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn filters_recordings_by_dtmf() {
        let (dir, state, id, token) = setup(Public::default());
        let ix = Index::open(&dir.path().join("index.sqlite")).unwrap();
        let count = |q: &'static str| {
            let state = state.clone();
            let token = token.clone();
            async move {
                let (st, body) = get(&state, q, Some(&token), None).await;
                assert_eq!(st, StatusCode::OK);
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                v["items"].as_array().unwrap().len()
            }
        };
        assert_eq!(count("/api/v1/recordings?dtmf=any").await, 0);
        ix.set_dtmf(id, Some("*73 1234#")).unwrap();
        assert_eq!(count("/api/v1/recordings?dtmf=any").await, 1);
        assert_eq!(count("/api/v1/recordings?dtmf=1234").await, 1);
        assert_eq!(count("/api/v1/recordings?dtmf=%2A73").await, 1);
        assert_eq!(count("/api/v1/recordings?dtmf=999").await, 0);
        assert_eq!(count("/api/v1/recordings?dtmf=").await, 1);
        let (_, body) = get(&state, "/api/v1/recordings", Some(&token), None).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["items"][0]["dtmf"], "*73 1234#");
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

    #[tokio::test]
    async fn serves_the_opus_copy_by_default_and_wav_on_request() {
        let (dir, state, id, tok) = setup(Public::default());
        let token = Some(tok.as_str());
        let audio = format!("/api/v1/recordings/{id}/audio");
        // No copy yet: the default is the WAV, and asking for Opus is a 404.
        let (st, body) = get(&state, &audio, token, None).await;
        assert_eq!((st, &body[..4]), (StatusCode::OK, b"RIFF".as_slice()));
        let (st, _) = get(&state, &format!("{audio}?format=opus"), token, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        std::fs::write(dir.path().join("2026/11/22/a.opus"), b"OggS-opus").unwrap();
        Index::open(&dir.path().join("index.sqlite"))
            .unwrap()
            .set_opus(id, "2026/11/22/a.opus", 9)
            .unwrap();
        let r = send(
            &state,
            Request::get(&audio)
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            (r.status, r.body.as_slice()),
            (StatusCode::OK, b"OggS-opus".as_slice())
        );
        assert_eq!(r.headers[header::CONTENT_TYPE], "audio/ogg; codecs=opus");
        assert_eq!(r.headers[header::CACHE_CONTROL], "private, max-age=3600");
        let (st, body) = get(&state, &format!("{audio}?format=wav"), token, None).await;
        assert_eq!((st, &body[..4]), (StatusCode::OK, b"RIFF".as_slice()));
        let (st, _) = get(&state, &format!("{audio}?format=mp3"), token, None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        let (_, body) = get(&state, &format!("/api/v1/recordings/{id}"), token, None).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["opus_path"], "2026/11/22/a.opus");
        assert_eq!(v["wav_url"], format!("{audio}?format=wav"));
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

    #[tokio::test]
    async fn ui_is_served_with_security_headers_and_etags() {
        let (_dir, state, _, _) = setup(Public::default());
        for path in ui::paths() {
            let r = send(&state, Request::get(path).body(Body::empty()).unwrap()).await;
            assert_eq!(r.status, StatusCode::OK, "{path}");
            assert!(!r.body.is_empty());
            assert_eq!(r.headers[header::CONTENT_SECURITY_POLICY], ui::CSP);
            assert_eq!(r.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert_eq!(r.headers[header::X_FRAME_OPTIONS], "DENY");
            let tag = r.headers[header::ETAG].clone();
            let again = send(
                &state,
                Request::get(path)
                    .header(header::IF_NONE_MATCH, tag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(again.status, StatusCode::NOT_MODIFIED, "{path}");
            assert!(again.body.is_empty());
        }
        let html = send(&state, Request::get("/").body(Body::empty()).unwrap()).await;
        let text = String::from_utf8(html.body).unwrap();
        assert!(text.contains("/assets/app.js"));
        assert!(
            !text.contains("<script>"),
            "no inline script (CSP forbids it)"
        );
        // API errors carry the headers too.
        let r = send(
            &state,
            Request::get("/api/v1/recordings")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }

    #[tokio::test]
    async fn egress_is_counted_by_role_and_the_budget_stops_visitors() {
        let open = Public {
            live: true,
            recordings: true,
            audio: true,
        };
        let (_dir, mut state, id, token) = setup(open);
        let eg = Arc::new(egress::Egress::new(2000, None));
        state.egress = Some(eg.clone());
        let audio = format!("/api/v1/recordings/{id}/audio");
        let (st, body) = get(&state, &audio, None, None).await;
        assert_eq!(st, StatusCode::OK);
        let anon = eg.status().anonymous_bytes;
        assert!(
            anon > body.len() as u64 && anon < body.len() as u64 + 1000,
            "{anon}"
        );
        // A made-up token is still anonymous.
        get(&state, &audio, Some("scn_made_up"), None).await;
        assert!(eg.status().anonymous_bytes > anon);
        assert_eq!(eg.status().admin_bytes, 0);
        get(&state, &audio, Some(&token), None).await;
        assert!(eg.status().admin_bytes > 0);

        // Spend the budget: visitors lose audio, the admin keeps it.
        eg.add(Role::Anonymous, 2000);
        let (st, _) = get(&state, &audio, None, None).await;
        assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
        let (st, _) = get(&state, &audio, Some(&token), None).await;
        assert_eq!(st, StatusCode::OK);
        let (_, body) = get(&state, "/api/v1/auth/session", None, None).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["budget"], "spent");
        assert!(v["budget_resets_ns"].as_i64().unwrap() > now_ns());
        let (_, body) = get(&state, "/api/v1/egress", Some(&token), None).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["current"]["level"], "spent");
        assert_eq!(v["current"]["budget_bytes"], 2000);
    }

    #[tokio::test]
    async fn storage_reports_usage_and_the_budget() {
        let (_dir, mut state, _, token) = setup(Public::default());
        let (st, body) = get(&state, "/api/v1/storage", Some(&token), None).await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["used_bytes"], 14);
        assert_eq!(v["oldest_ns"], 1_795_330_443_000_000_000i64);
        assert!(v["retention"].is_null());

        let ix = Index::open(&state.recordings.join("index.sqlite")).unwrap();
        let r = scanner_store::retention::Retention::new(Default::default(), &ix).unwrap();
        state.storage = Some(r.usage());
        let (_, body) = get(&state, "/api/v1/storage", Some(&token), None).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["retention"]["budget_bytes"], 8_000_000_000u64);
        assert_eq!(v["retention"]["used_bytes"], 14);
        assert_eq!(v["retention"]["expired"], 0);
    }

    #[tokio::test]
    async fn channels_and_session_report_what_the_ui_needs() {
        let (_dir, state, _, _) = setup(Public::default());
        state.channels.write().unwrap().push(ChannelInfo {
            freq_hz: 146.52e6,
            name: Some("Calling".into()),
            kind: "channel",
            mode: "nbfm".into(),
            bandwidth_hz: 16e3,
            priority: true,
            locked_out: false,
            pair_hz: None,
            overridden: false,
            spur: false,
            sites: Vec::new(),
        });
        let (status, body) = get(&state, "/api/v1/channels", None, None).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v[0]["name"], "Calling");
        assert_eq!(v[0]["kind"], "channel");
        assert!(v[0].get("sites").is_none(), "empty sites are left out");
        assert_eq!(v[0]["priority"], true);
        let (_, body) = get(&state, "/api/v1/auth/session", None, None).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["role"], "anonymous");
        assert_eq!(v["public"]["live"], true);
        assert_eq!(v["public"]["recordings"], false);
    }

    #[tokio::test]
    async fn live_socket_refuses_other_sites() {
        let (_dir, state, _, token) = setup(Public::default());
        let req = Request::get("/api/v1/live")
            .header(header::HOST, "scanner.example")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&state, req).await.status, StatusCode::FORBIDDEN);
        let req = Request::get("/api/v1/live")
            .header(header::HOST, "scanner.example")
            .header(header::ORIGIN, "https://scanner.example")
            .body(Body::empty())
            .unwrap();
        // Past the origin check, refused only as a handshake without upgrade
        // headers.
        assert_eq!(send(&state, req).await.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn channel_edits_need_admin_persist_and_reach_the_scanner() {
        let (_dir, mut state, _, token) = setup(Public::default());
        state.settings = Some(Arc::new(Settings::in_memory().unwrap()));
        state.channels.write().unwrap().push(ChannelInfo {
            freq_hz: 146.52e6,
            name: Some("Calling".into()),
            kind: "channel",
            mode: "nbfm".into(),
            bandwidth_hz: 16e3,
            priority: false,
            locked_out: false,
            pair_hz: None,
            overridden: false,
            spur: false,
            sites: Vec::new(),
        });
        {
            let mut g = state.status.write().unwrap();
            g.sample_rate = 8e6;
            g.center_freq_hz = 146.0125e6;
        }
        // A stand-in DSP thread that applies the list to the channel list.
        let (tx, rx) = std::sync::mpsc::channel::<Control>();
        state.control = Some(tx);
        let channels = state.channels.clone();
        let applied = Arc::new(Mutex::new(Vec::new()));
        let seen = applied.clone();
        std::thread::spawn(move || {
            while let Ok(Control::Overrides { list, done }) = rx.recv() {
                let mut g = channels.write().unwrap();
                for c in g.iter_mut() {
                    let o = list.iter().find(|o| o.freq_hz as f64 == c.freq_hz);
                    c.locked_out = o.and_then(|o| o.locked_out).unwrap_or(false);
                    c.priority = o.and_then(|o| o.priority).unwrap_or(false);
                    c.overridden = o.is_some();
                }
                seen.lock().unwrap().push(list.len());
                let _ = done.send(());
            }
        });
        let cookie = login(&state, "masen", PASSWORD).await.cookie().unwrap();
        let lock = serde_json::json!({"locked_out": true});
        let uri = "/api/v1/channels/146520000";
        let r = send(&state, json_req("PATCH", uri, lock.clone(), None, None)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = send(
            &state,
            json_req("PATCH", uri, lock.clone(), Some(&cookie), None),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "cookie without CSRF");
        let bearer = |method: &str, uri: &str, body: serde_json::Value| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let r = send(&state, bearer("PATCH", uri, lock.clone())).await;
        assert_eq!(r.status, StatusCode::OK);
        let v = r.json();
        assert_eq!(v["channel"]["locked_out"], true);
        assert_eq!(v["channel"]["overridden"], true);
        assert_eq!(v["override"]["updated_by"], "token:tests");
        // Priority is merged in, the lockout stays.
        let r = send(
            &state,
            bearer("PATCH", uri, serde_json::json!({"priority": true})),
        )
        .await;
        assert_eq!(r.json()["channel"]["priority"], true);
        assert_eq!(r.json()["channel"]["locked_out"], true);
        // Priority off a watched channel, lockouts outside the capture,
        // empty and unknown bodies: refused.
        for (u, body) in [
            (
                "/api/v1/channels/145212500",
                serde_json::json!({"priority": true}),
            ),
            (
                "/api/v1/channels/440000000",
                serde_json::json!({"locked_out": true}),
            ),
            (uri, serde_json::json!({})),
        ] {
            let r = send(&state, bearer("PATCH", u, body)).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{u}");
        }
        let r = send(
            &state,
            bearer("PATCH", uri, serde_json::json!({"record": true})),
        )
        .await;
        assert!(r.status.is_client_error());
        // An off-grid lockout inside the capture is fine.
        let r = send(
            &state,
            bearer("PATCH", "/api/v1/channels/145212500", lock.clone()),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(
            state.settings.as_ref().unwrap().overrides().unwrap().len(),
            2
        );
        // Reset.
        let del = "/api/v1/channels/146520000/override";
        let r = send(&state, bearer("DELETE", del, serde_json::json!(null))).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        let r = send(&state, bearer("DELETE", del, serde_json::json!(null))).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let c = state.channel_at(146_520_000).unwrap();
        assert!(!c.locked_out && !c.priority && !c.overridden);
        assert_eq!(*applied.lock().unwrap(), vec![1, 1, 2, 1]);
        let log = state.auth.as_ref().unwrap().recent_audit(20).unwrap();
        assert!(
            log.iter()
                .any(|e| e.action == "channel_changed"
                    && e.detail.contains("146.5200 MHz locked out"))
        );
        assert!(log.iter().any(|e| e.action == "channel_reset"));
    }

    #[tokio::test]
    async fn channel_edits_are_refused_without_a_settings_database() {
        let (_dir, state, _, token) = setup(Public::default());
        let req = Request::builder()
            .method("PATCH")
            .uri("/api/v1/channels/146520000")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"locked_out":true}"#))
            .unwrap();
        assert_eq!(
            send(&state, req).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
