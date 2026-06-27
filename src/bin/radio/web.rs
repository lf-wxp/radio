//! LAN web console — single-page HTML UI + minimal JSON API.
//!
//! Architecture:
//!
//! - One `embassy` task ([`web_task`]) owns the listening socket and
//!   serves at most one connection at a time (picoserve's design).
//! - Read-side endpoints (`GET /api/state`) snapshot
//!   [`crate::state::RADIO_STATE`] and serialise it directly.
//! - Write-side endpoints (`POST /api/tune`, etc.) push a
//!   [`RadioCommand`] into the existing [`INPUT_CMDS`] channel, so the
//!   web client and the rotary encoder share the exact same command
//!   path inside [`crate::tasks::radio_control_task`]. This keeps the
//!   chip's I2C ownership safely single-threaded without any new
//!   synchronisation primitives.
//!
//! ## Security
//!
//! There is **no authentication**. The console is intended for trusted
//! home networks only — do not expose port 80 to the public internet.
//! `picoserve`'s own README warns against direct exposure as well.

use embassy_net::Stack;
use picoserve::extract::Json;
use picoserve::response::StatusCode;
use picoserve::routing::{get, post};
use picoserve::{AppBuilder, AppRouter, Router};
use serde::{Deserialize, Serialize};

use crate::diagnostics::{self, HealthDto};
use crate::state::{
  INPUT_CMDS, OTA_CMDS, OtaCommand, OtaProgress, PRESET_EMPTY, RADIO_STATE, RadioCommand,
  is_ps_unknown, publish_web_ip,
};

// ============================================================================
// Configuration
// ============================================================================

/// TCP listening port for the web console.
const WEB_PORT: u16 = 80;

/// Per-connection HTTP framing buffer.
///
/// Sized for our largest expected payload (the embedded HTML page,
/// served chunk-by-chunk) plus headers. 2 KiB is plenty.
const HTTP_BUFFER_SIZE: usize = 2048;

/// Per-connection TCP rx/tx buffer sizes.
///
/// 1 KiB rx is enough for a single short request. tx is bumped to 4 KiB
/// because `GET /api/log` returns up to [`crate::listening_log::LOG_CAPACITY`]
/// entries, each ~110 B of JSON, which can total ~7 KiB. picoserve
/// streams the response in chunks but the underlying smoltcp tx ring
/// must be at least one MSS to avoid stalling.
const TCP_RX_BUFFER_SIZE: usize = 1024;
const TCP_TX_BUFFER_SIZE: usize = 4096;

// ============================================================================
// JSON DTOs
// ============================================================================

/// Owned snapshot returned by `GET /api/state`.
///
/// All numeric fields use the same units as the on-device state
/// (`freq_x10` in 0.1 MHz, RSSI in 0..=75, volume in 0..=15) so the
/// browser-side JS doesn't need any unit conversion. Owned strings
/// (rather than borrowed `&str`) so we can release the
/// [`RADIO_STATE`] mutex before serialisation.
#[derive(Serialize)]
struct StateDto {
  freq_x10: u16,
  rssi: u8,
  volume: u8,
  muted: bool,
  stereo: bool,
  auto_mono: bool,
  station_name: alloc::string::String,
  radio_text: alloc::string::String,
  /// RT+ "now playing" line: pre-formatted as `"{artist} — {title}"`
  /// (or just title / just artist if only one tag was decoded). `None`
  /// when the station does not transmit RT+ or while no item is
  /// currently in progress; the front-end falls back to `radio_text`.
  /// Skipped from the JSON when null to keep the payload tight at idle.
  ///
  /// We pre-join here (rather than emitting two separate fields) to
  /// shave ≈24 bytes off the [`StateDto`] stack frame; the only
  /// information the browser loses is whether the broadcaster gave us
  /// just one of the two tags, which it never disambiguates anyway.
  #[serde(skip_serializing_if = "Option::is_none")]
  rt_plus: Option<alloc::string::String>,
  pty_label: Option<&'static str>,
  /// Decoded RDS clock as `"HH:MM"`, empty when no CT received yet.
  clock: alloc::string::String,
  af_count: u8,
  af_following: bool,
  preset_idx: Option<u8>,
  /// Saved preset slots; one entry per [`crate::state::MAX_PRESETS`].
  ///
  /// Empty slots emit `{ "freq": 0 }` (no `ps` field). Populated slots
  /// always carry `freq`; `ps` is present iff the broadcaster's RDS
  /// PS name has been decoded at least once for that slot, in which
  /// case the front-end shows it as a friendly label (e.g. "BBC R1")
  /// in place of the numeric frequency.
  presets: [PresetSlotDto; crate::state::MAX_PRESETS],
  wifi_ssid: alloc::string::String,
  wifi_connected: bool,
  /// Latest snapshot of the OTA state machine. See [`OtaProgressDto`].
  ota: OtaProgressDto,
}

/// Wire shape of [`OtaProgress`] for the front-end progress widget.
///
/// Flattened into a discriminated union so JS can render with a simple
/// `switch (ota.phase)` without parsing nested Rust-style enum tags.
///
/// `received` / `total` are only populated when `phase == "downloading"`;
/// `reason` is only populated when `phase == "failed"`. Other
/// combinations stay `null` so the JSON stays small at idle.
#[derive(Serialize)]
struct OtaProgressDto {
  phase: &'static str,
  #[serde(skip_serializing_if = "Option::is_none")]
  received: Option<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  total: Option<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  reason: Option<&'static str>,
}

impl From<OtaProgress> for OtaProgressDto {
  fn from(p: OtaProgress) -> Self {
    match p {
      OtaProgress::Idle => Self {
        phase: "idle",
        received: None,
        total: None,
        reason: None,
      },
      OtaProgress::Connecting => Self {
        phase: "connecting",
        received: None,
        total: None,
        reason: None,
      },
      OtaProgress::Downloading { received, total } => Self {
        phase: "downloading",
        received: Some(received),
        // `total = 0` means the server didn't send Content-Length.
        // Surface as `null` so JS can render an indeterminate spinner.
        total: if total == 0 { None } else { Some(total) },
        reason: None,
      },
      OtaProgress::Activating => Self {
        phase: "activating",
        received: None,
        total: None,
        reason: None,
      },
      OtaProgress::Success => Self {
        phase: "success",
        received: None,
        total: None,
        reason: None,
      },
      OtaProgress::Failed(reason) => Self {
        phase: "failed",
        received: None,
        total: None,
        reason: Some(reason),
      },
    }
  }
}

/// One row in the `GET /api/log` response.
///
/// Mirrors [`crate::listening_log::LogEntry`] but with owned strings
/// so the listening-log mutex is released before serialisation, and
/// with the field names the front-end JS expects.
#[derive(Serialize)]
struct LogEntryDto {
  /// Boot-relative seconds; the front-end converts this to `mm:ss ago`.
  uptime_secs: u32,
  freq_x10: u16,
  rssi: u8,
  ps: alloc::string::String,
  rt: alloc::string::String,
}

/// Wrapper for `GET /api/log`. Wrapping the array in an object keeps
/// us free to add more top-level fields (capacity, head index, etc.)
/// without breaking the JSON contract with the browser.
#[derive(Serialize)]
struct LogDto {
  capacity: u16,
  /// Current Unix epoch seconds, populated only after
  /// [`crate::clock::wall_time_unix_secs`] returns `Some` (i.e.
  /// SNTP has synced at least once this boot). The browser uses
  /// this to render absolute timestamps; until it shows up the
  /// front-end falls back to relative `mm:ss ago`.
  #[serde(skip_serializing_if = "Option::is_none")]
  now_unix: Option<u64>,
  entries: alloc::vec::Vec<LogEntryDto>,
}

/// Wire shape for `GET /api/spectrum`.
///
/// `samples` is heap-allocated as `Vec<u8>` rather than the underlying
/// `[u8; SPECTRUM_LEN]` because (a) `serde` only derives `Serialize`
/// for arrays of length ≤ 32 and the spectrum is 52 buckets, and
/// (b) keeping it heap-borne shaves the 52 byte buffer off the
/// picoserve task's stack frame for this handler.
///
/// `bottom_x10` / `top_x10` describe the FM band the buckets span
/// (in 0.1 MHz units, i.e. 875..=1080 for the US/Europe plan). Sent
/// alongside the samples so the browser can label the X-axis without
/// having to know the band-plan separately.
#[derive(Serialize)]
struct SpectrumDto {
  samples: alloc::vec::Vec<u8>,
  /// Currently tuned frequency in 0.1 MHz units. Used by the browser
  /// to draw the cursor on the chart.
  freq_x10: u16,
  bottom_x10: u16,
  top_x10: u16,
}

/// One row in [`StateDto::presets`].
///
/// Kept tiny because the array is sized statically at
/// [`crate::state::MAX_PRESETS`] and lives inside the per-request
/// `Json<StateDto>` future on picoserve's task stack — every byte of
/// per-slot inflation pushes the serializer closer to clippy's
/// `large_stack_frames` threshold.
///
/// `freq` is in 0.1 MHz units (matching the rest of the wire format).
/// `ps` is omitted when the slot is empty *or* when no RDS PS name
/// has been decoded yet for that station; the front-end falls back
/// to formatting `freq` as e.g. `101.5` in that case.
#[derive(Serialize)]
struct PresetSlotDto {
  freq: u16,
  #[serde(skip_serializing_if = "Option::is_none")]
  ps: Option<alloc::string::String>,
}

/// Body for `POST /api/tune`.
#[derive(Deserialize)]
struct TuneBody {
  /// Target frequency in 0.1 MHz units (e.g. `1015` = 101.5 MHz).
  freq_x10: u16,
}

/// Body for `POST /api/ota`.
#[derive(Deserialize)]
struct OtaBody {
  /// Plain-HTTP firmware URL with an IPv4 literal host, e.g.
  /// `http://192.168.1.10:8000/firmware.bin`. See
  /// [`crate::ota::http_download`] for the parser's full grammar.
  url: alloc::string::String,
}

// ============================================================================
// Embedded HTML
// ============================================================================

/// Single-page console served at `GET /`.
///
/// Phone-friendly: 16 px base font, 44 px-tall buttons, no external
/// network requests (CSS + JS are inline). Polls `/api/state` once per
/// second and posts compact JSON for every action so the radio task
/// never sees a flood of commands.
const INDEX_HTML: &str = include_str!("web/index.html");

// ============================================================================
// Router
// ============================================================================

pub struct AppProps;

impl AppBuilder for AppProps {
  type PathRouter = impl picoserve::routing::PathRouter;

  fn build_app(self) -> Router<Self::PathRouter> {
    Router::new()
      .route(
        "/",
        get(|| async {
          // `picoserve::response::File::html` would be slightly nicer
          // but constructing it requires an extra import; a tuple of
          // (Content-Type header, body) is the idiomatic shortcut.
          ([("content-type", "text/html; charset=utf-8")], INDEX_HTML)
        }),
      )
      .route("/api/state", get(handle_get_state))
      .route("/api/spectrum", get(handle_get_spectrum))
      .route("/api/log", get(handle_get_log))
      .route("/api/tune", post(handle_post_tune))
      .route("/api/tune/up", post(handle_post_tune_up))
      .route("/api/tune/down", post(handle_post_tune_down))
      .route("/api/preset/cycle", post(handle_post_preset_cycle))
      .route("/api/preset/save", post(handle_post_preset_save))
      .route("/api/mute", post(handle_post_mute))
      .route("/api/spectrum/scan", post(handle_post_spectrum_scan))
      .route("/api/ota", post(handle_post_ota))
      .route("/api/health", get(handle_get_health))
  }
}

// ============================================================================
// Handlers
// ============================================================================

/// Pre-format an RT+ tag pair into the wire string the browser displays.
///
/// Returns:
/// - `Some("{artist} \u{2014} {title}")` when both tags are present (the common
///   case for music stations).
/// - `Some("{artist}")` / `Some("{title}")` when the broadcaster only sent
///   one of the two (rare, but the spec allows it).
/// - `None` when neither tag is set, so the JSON omits the field entirely
///   and the front-end falls back to the raw `radio_text` scroller.
fn format_rt_plus(artist: Option<&str>, title: Option<&str>) -> Option<alloc::string::String> {
  match (artist, title) {
    (Some(a), Some(t)) => Some(alloc::format!("{a} \u{2014} {t}")),
    (Some(a), None) => Some(alloc::string::String::from(a)),
    (None, Some(t)) => Some(alloc::string::String::from(t)),
    (None, None) => None,
  }
}

/// Build the wire-format preset slot array from the in-memory snapshot.
///
/// Walks `presets.freqs` and pairs each frequency with the optional
/// PS label cached in `presets.ps`. Empty slots (`freq == 0`) keep
/// `ps == None` so the front-end short-circuits to its "empty slot"
/// rendering without paying for a string allocation it would discard
/// anyway.
///
/// PS bytes are decoded from the raw 8-byte RDS buffer with two
/// transformations:
///
/// * leading / trailing ASCII whitespace and NUL padding stripped
///   (broadcasters routinely zero-pad short names like `"BBC R1"`),
/// * any non-ASCII bytes replaced with `?` via lossy UTF-8 (Si4703
///   sometimes emits Latin-1 high-bit characters; the JSON wire
///   format is UTF-8 only).
///
/// Returns `None` for the `ps` field when the trimmed buffer is empty
/// — that's the same sentinel
/// [`crate::state::PresetSet::ps_for`] uses to mean "unknown".
fn build_preset_slots(
  presets: &crate::state::PresetSet,
) -> [PresetSlotDto; crate::state::MAX_PRESETS] {
  core::array::from_fn(|i| {
    let freq = presets.freqs[i];
    let ps = if freq == crate::state::PRESET_EMPTY {
      None
    } else {
      decode_preset_ps(&presets.ps[i])
    };
    PresetSlotDto { freq, ps }
  })
}

/// Decode an 8-byte raw RDS PS buffer into a printable string for the
/// JSON response, or `None` when the buffer holds the unknown sentinel.
fn decode_preset_ps(buf: &[u8; 8]) -> Option<alloc::string::String> {
  // All-zero / all-blank means "unknown" — see PresetSet::ps for the
  // sentinel definition.
  if is_ps_unknown(buf) {
    return None;
  }
  // Trim NUL / space padding on the raw bytes *before* the lossy
  // ASCII conversion, so we never accidentally strip a '?' that was
  // substituted for a meaningful high-bit byte at the edges.
  let start = buf.iter().position(|&b| b != 0 && b != b' ').unwrap_or(0);
  let end = buf
    .iter()
    .rposition(|&b| b != 0 && b != b' ')
    .map_or(0, |i| i + 1);
  let trimmed_bytes = &buf[start..end];
  if trimmed_bytes.is_empty() {
    return None;
  }
  // Lossy ASCII keeps the JSON valid even when broadcasters use
  // Latin-1 / GB2312 high-bit code points (which Si4703 just hands
  // through). Only non-printable-ASCII bytes are replaced with '?'.
  let s: alloc::string::String = trimmed_bytes
    .iter()
    .map(|&b| {
      if (0x20..=0x7E).contains(&b) {
        b as char
      } else {
        '?'
      }
    })
    .collect();
  if s.is_empty() { None } else { Some(s) }
}

/// `GET /api/state` — return a JSON snapshot of [`RADIO_STATE`].
///
/// Allocates two transient `String`s (PS / RT decoded text) per call;
/// at the polling cadence the browser uses (1 Hz) this is well under
/// what `esp-alloc` can sustain.
#[allow(
  clippy::large_stack_frames,
  reason = "the StateDto + JSON wrapper holds four owned String buffers (PS, RT, \
            RT+ pre-joined, WiFi SSID) plus an inline `[u16; MAX_PRESETS]`; \
            ~1.1 KiB stays well under the 16 KiB Embassy task stack and is \
            released as soon as picoserve has serialised the response."
)]
async fn handle_get_state() -> picoserve::response::Json<StateDto> {
  let state = RADIO_STATE.lock().await;
  let dto = StateDto {
    freq_x10: state.freq_mhz_x10,
    rssi: state.rssi,
    volume: state.volume,
    muted: state.muted,
    stereo: state.stereo,
    auto_mono: state.auto_mono,
    station_name: state.station_name.clone(),
    radio_text: state.radio_text.clone(),
    rt_plus: format_rt_plus(
      state.rt_plus_artist.as_deref(),
      state.rt_plus_title.as_deref(),
    ),
    pty_label: state.pty_label,
    clock: match state.clock_hh_mm {
      Some((h, m)) => alloc::format!("{:02}:{:02}", h, m),
      None => alloc::string::String::new(),
    },
    af_count: state.af_count,
    af_following: state.af_following,
    preset_idx: state.preset_idx,
    presets: build_preset_slots(&state.presets),
    wifi_ssid: state.wifi_ssid.clone(),
    wifi_connected: state.wifi_connected,
    ota: state.ota_progress.into(),
  };
  picoserve::response::Json(dto)
}

/// `GET /api/spectrum` — latest band-wide RSSI sweep snapshot.
///
/// Pulled out of `/api/state` so the 52-byte sample buffer doesn't
/// inflate every state poll; the front-end only fetches the spectrum
/// when the user opens the chart panel or hits the "Scan" button.
///
/// The bucket layout matches [`crate::state::SPECTRUM_LEN`] (52
/// buckets across 87.5–108.0 MHz) and the cursor index is recomputed
/// on the front-end with the same formula as
/// [`crate::ui::spectrum_cursor_for`].
async fn handle_get_spectrum() -> picoserve::response::Json<SpectrumDto> {
  let state = RADIO_STATE.lock().await;
  picoserve::response::Json(SpectrumDto {
    samples: alloc::vec::Vec::from(state.spectrum),
    freq_x10: state.freq_mhz_x10,
    bottom_x10: 875,
    top_x10: 1080,
  })
}

/// `GET /api/log` — return the full listening-log ring buffer in
/// chronological order (oldest first).
///
/// The browser reverses the array client-side so the most recent
/// entry sits at the top of the panel; doing the reverse here would
/// just waste cycles.
async fn handle_get_log() -> picoserve::response::Json<LogDto> {
  let log = crate::listening_log::LISTENING_LOG.lock().await;
  let entries: alloc::vec::Vec<LogEntryDto> = log
    .iter_chronological()
    .map(|e| LogEntryDto {
      uptime_secs: e.uptime_secs,
      freq_x10: e.freq_x10,
      rssi: e.rssi,
      ps: alloc::string::String::from(e.ps_str()),
      rt: alloc::string::String::from(e.rt_str()),
    })
    .collect();
  picoserve::response::Json(LogDto {
    capacity: crate::listening_log::LOG_CAPACITY as u16,
    now_unix: crate::clock::wall_time_unix_secs(),
    entries,
  })
}

/// `POST /api/tune` \u2014 jump to an exact frequency.
///
/// Body: `{ "freq_x10": 1015 }`.
/// Returns `400 Bad Request` when the frequency is outside the FM band;
/// otherwise enqueues a [`RadioCommand::TuneAbsolute`] and returns
/// `204 No Content` (empty body).
async fn handle_post_tune(Json(body): Json<TuneBody>) -> Result<(), StatusCode> {
  // Mirror the FM band bounds enforced by `clamp_freq`. Reject obviously
  // bogus inputs early so the dial never even attempts the I2C tune.
  if !(875..=1080).contains(&body.freq_x10) {
    return Err(StatusCode::BAD_REQUEST);
  }
  send_command(RadioCommand::TuneAbsolute(body.freq_x10)).await;
  Ok(())
}

/// `POST /api/tune/up` \u2014 nudge +0.1 MHz.
async fn handle_post_tune_up() {
  send_command(RadioCommand::TuneRelative(1)).await;
}

/// `POST /api/tune/down` \u2014 nudge \u22120.1 MHz.
async fn handle_post_tune_down() {
  send_command(RadioCommand::TuneRelative(-1)).await;
}

/// `POST /api/preset/cycle` \u2014 jump to the next saved preset.
async fn handle_post_preset_cycle() {
  send_command(RadioCommand::CyclePreset).await;
}

/// `POST /api/preset/save` \u2014 persist the current frequency.
///
/// No-op when the current frequency is already saved. The radio task
/// FIFO-evicts the oldest slot once all eight are full.
async fn handle_post_preset_save() -> Result<(), StatusCode> {
  // Reject the save when no station is dialled in yet: the boot-time
  // placeholder is `PRESET_EMPTY` and saving that would just clutter
  // the table.
  let cur = RADIO_STATE.lock().await.freq_mhz_x10;
  if cur == PRESET_EMPTY {
    return Err(StatusCode::CONFLICT);
  }
  send_command(RadioCommand::SavePreset).await;
  Ok(())
}

/// `POST /api/mute` — toggle mute.
async fn handle_post_mute() {
  send_command(RadioCommand::ToggleMute).await;
}

/// `POST /api/spectrum/scan` — re-run the band-wide RSSI sweep.
///
/// Enqueues a [`RadioCommand::SpectrumScan`]; the actual sweep runs
/// inside [`crate::tasks::radio_control_task`] (~4 s of off-air time)
/// and the result is published into [`RADIO_STATE`] when complete.
/// The browser observes completion by polling `GET /api/state` and
/// noticing the `spectrum` array has changed.
///
/// Returns `204 No Content` immediately on accept; the radio task is
/// expected to honour the request within ~5 s but the HTTP layer does
/// not block on completion so a slow sweep cannot hold the socket.
async fn handle_post_spectrum_scan() {
  send_command(RadioCommand::SpectrumScan).await;
}

/// `POST /api/ota` — start an OTA update from the supplied URL.
///
/// Body: `{ "url": "http://192.168.1.10:8000/firmware.bin" }`.
///
/// Validates that the scheme is `http://` and that the URL is
/// reasonably sized; deeper checks (IPv4 literal, port range) happen
/// inside [`crate::ota::http_download::parse_url`] so we don't
/// duplicate the parser. On accept the request returns immediately
/// with `204 No Content`; the actual download runs in
/// [`crate::tasks::radio_control_task`] and progress is exposed
/// through `GET /api/state`'s `ota` field.
///
/// Returns:
/// - `204 No Content` on accept (download started in background).
/// - `400 Bad Request` for an empty / oversized URL or non-HTTP scheme.
/// - `409 Conflict` if an OTA job is already running.
async fn handle_post_ota(Json(body): Json<OtaBody>) -> Result<(), StatusCode> {
  // Bound the URL length so we don't blow heap on a hostile client.
  // 256 chars is more than enough for any realistic LAN address.
  if body.url.is_empty() || body.url.len() > 256 {
    return Err(StatusCode::BAD_REQUEST);
  }
  if !body.url.starts_with("http://") {
    return Err(StatusCode::BAD_REQUEST);
  }

  // Single-flight: refuse if an update is already in progress so a
  // refreshed browser tab can't accidentally fire two downloads at
  // the flash peripheral.
  {
    let state = RADIO_STATE.lock().await;
    if state.ota_in_progress {
      return Err(StatusCode::CONFLICT);
    }
  }

  // The signal is single-slot; an in-flight job inspects the URL
  // exactly once via [`Signal::wait`], so a stale URL queued behind
  // a running job is naturally discarded.
  OTA_CMDS.signal(OtaCommand::Start(body.url));
  Ok(())
}
// ============================================================================
// Helpers
// ============================================================================

/// `GET /api/health` — return a JSON health snapshot for remote diagnostics.
///
/// Includes uptime, heap usage, I²C error count, WiFi status, RSSI, and
/// the POST result. Designed for monitoring dashboards and quick
/// troubleshooting without physical access to the device.
async fn handle_get_health() -> picoserve::response::Json<HealthDto> {
  let post = diagnostics::get_post_result();
  let dto = if let Some(post_ref) = post {
    HealthDto::capture(post_ref).await
  } else {
    // POST hasn't completed yet (shouldn't happen in practice since
    // the web task starts after POST, but handle gracefully).
    let free = diagnostics::heap_free_bytes();
    let total = diagnostics::heap_total_bytes();
    let usage_pct = total
      .checked_sub(free)
      .and_then(|used| used.checked_mul(100))
      .and_then(|num| num.checked_div(total))
      .unwrap_or(0) as u8;
    HealthDto {
      uptime_secs: diagnostics::uptime_secs(),
      heap_free: free,
      heap_total: total,
      heap_usage_pct: usage_pct,
      i2c_errors: diagnostics::i2c_error_total(),
      wifi_connected: false,
      rssi: 0,
      tuner_ok: false,
      post_status: "pending",
      radio_task_alive: diagnostics::watchdog_ok(),
      watchdog_elapsed_secs: diagnostics::watchdog_elapsed_secs(),
    }
  };
  picoserve::response::Json(dto)
}

/// Push a command through the shared input channel.
///
/// Blocks (asynchronously) when the channel is full so the radio task
/// always processes commands in arrival order. Channel capacity (8) is
/// large enough that this branch only triggers under pathological
/// flooding from a buggy client.
async fn send_command(cmd: RadioCommand) {
  INPUT_CMDS.send(cmd).await;
}

// ============================================================================
// Embassy task
// ============================================================================

/// Long-running task that listens for incoming HTTP connections.
///
/// Runs forever; on every connection it serves a single request/response
/// cycle (keep-alive intentionally off to free the socket for the next
/// client quickly). `picoserve`'s `listen_and_serve` re-creates the
/// `TcpSocket` between connections so a misbehaving client cannot wedge
/// the server.
#[embassy_executor::task]
#[allow(
  clippy::large_stack_frames,
  reason = "the task owns its 2 KiB HTTP framing buffer and 1 KiB × 2 TCP \
            buffers on its own stack so picoserve can re-create the \
            socket between connections without re-allocating heap. \
            ~4.4 KiB still fits comfortably inside the 16 KiB \
            Embassy task stack on ESP32-C6."
)]
pub async fn web_task(
  stack: Stack<'static>,
  app: &'static AppRouter<AppProps>,
  config: &'static picoserve::Config,
) -> ! {
  // Wait until DHCP has handed us an IPv4 address — picoserve will
  // happily start listening before then but the LCD's IP badge would
  // remain blank, confusing the user. Republish on every (re)bind so a
  // lease change is reflected.
  stack.wait_config_up().await;
  if let Some(cfg) = stack.config_v4() {
    publish_web_ip(Some(cfg.address.address().octets())).await;
  }

  let mut http_buffer = [0u8; HTTP_BUFFER_SIZE];
  let mut tcp_rx = [0u8; TCP_RX_BUFFER_SIZE];
  let mut tcp_tx = [0u8; TCP_TX_BUFFER_SIZE];

  loop {
    picoserve::Server::new(app, config, &mut http_buffer)
      .listen_and_serve(0u8, stack, WEB_PORT, &mut tcp_rx, &mut tcp_tx)
      .await;
  }
}
