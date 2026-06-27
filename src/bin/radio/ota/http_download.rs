//! Plain-HTTP downloader that streams a firmware image from a LAN host
//! into an [`OtaWriter`].
//!
//! # Scope
//!
//! - **HTTP only** — TLS support is intentionally deferred (see
//!   `docs/ota-design.md` § Roadmap). Threat model: the device is on a
//!   trusted home network and pulls from the user's own host (the
//!   in-tree dev server `cargo make ota-serve`, GitHub Releases via a
//!   reverse proxy, etc.).
//! - **IPv4 literal hosts only** — `embassy-net` is not built with the
//!   `dns` feature in this project, so the URL host must be a numeric
//!   IPv4 address (`http://192.168.1.10:8000/firmware.bin`). A future
//!   patch can lift this once we accept the ~6 KiB code-size cost of
//!   smoltcp's resolver.
//! - **Single in-flight job** — the caller (radio control task) gates
//!   re-entry; the OTA writer itself owns the flash peripheral
//!   exclusively for the duration.
//!
//! # Wire format
//!
//! We speak the absolute minimum of HTTP/1.1 needed to fetch a binary
//! body:
//!
//! - `GET <path> HTTP/1.0` (HTTP/1.0 forces `Connection: close` semantics
//!   so we don't have to parse keep-alive framing).
//! - Read response bytes into a small ring; locate the `\r\n\r\n` header
//!   terminator.
//! - Parse the status line (`HTTP/1.x 200 ...`) and the
//!   `Content-Length:` header (case-insensitive, optional).
//! - Anything left in the ring after the terminator is the start of the
//!   body and is fed straight to the writer.
//! - Subsequent socket reads stream directly into the writer until either
//!   `Content-Length` is satisfied or the peer closes (HTTP/1.0 EOF).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;

use core::str::FromStr;

use defmt::{Format, debug, info, warn};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Stack};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;

use crate::state::{OtaProgress, publish_ota_progress};

use super::writer::{OtaError, OtaWriter};

/// Network framing buffers. 2 KiB on each side matches the web console
/// task and is plenty for ESP32-C6 LWIP defaults.
const TCP_RX: usize = 2048;
const TCP_TX: usize = 1024;

/// Streaming chunk size used both for the response-header ring and for
/// each socket read into the writer. Sized to fit comfortably inside one
/// TCP MSS so we minimise syscalls without hoarding stack.
const READ_CHUNK: usize = 1024;

/// Maximum bytes we'll buffer while looking for `\r\n\r\n`. Real HTTP
/// servers emit headers well under 4 KiB; rejecting larger keeps us safe
/// from a (deliberately) malformed peer that might otherwise burn the
/// whole download buffer on header noise.
const HEADER_LIMIT: usize = 4096;

/// Per-connection timeouts. Chosen to be loose enough to survive a busy
/// LAN but tight enough that a wedged peer doesn't block reboot for
/// minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Re-publish [`OtaProgress::Downloading`] at most this often. Keeps the
/// shared [`crate::state::RADIO_STATE`] from being woken up on every
/// 1 KiB chunk while still giving the UI ~10 fps progress updates.
const PROGRESS_REPUBLISH_INTERVAL: Duration = Duration::from_millis(100);

// ============================================================================
// Errors
// ============================================================================

/// Errors surfaced by [`download_to_writer`].
///
/// Variants are deliberately coarse-grained: the OTA controller only
/// branches on "fatal vs not"; defmt log lines carry the diagnostic
/// detail.
#[derive(Debug, Format, Clone, Copy, PartialEq, Eq)]
pub enum HttpError {
  /// URL was malformed or used an unsupported scheme/host form.
  BadUrl,
  /// TCP connect to the resolved endpoint failed.
  ConnectFailed,
  /// Socket read/write returned an error or timed out.
  Io,
  /// The response status line did not start with `HTTP/1.` or the
  /// status code was not 2xx.
  BadStatus(u16),
  /// Headers exceeded [`HEADER_LIMIT`] before we found `\r\n\r\n`.
  HeadersTooLarge,
  /// The peer closed before delivering the advertised `Content-Length`.
  Truncated,
  /// Forwarded from the underlying [`OtaWriter`].
  Writer(OtaError),
}

impl From<OtaError> for HttpError {
  fn from(e: OtaError) -> Self {
    Self::Writer(e)
  }
}

// ============================================================================
// URL parsing
// ============================================================================

/// Borrowed view of a parsed `http://` URL with an IPv4-literal host.
#[derive(Debug, PartialEq, Eq)]
struct ParsedUrl<'a> {
  ip: Ipv4Address,
  port: u16,
  /// Path including the leading `/`, plus any query string.
  path: &'a str,
}

/// Parse `http://<ipv4>[:port]/path` into its component parts.
///
/// Returns [`HttpError::BadUrl`] for any non-HTTP scheme, missing host,
/// non-numeric IPv4, or out-of-range port.
fn parse_url(url: &str) -> Result<ParsedUrl<'_>, HttpError> {
  // 1. Strip the scheme.
  let rest = url.strip_prefix("http://").ok_or(HttpError::BadUrl)?;

  // 2. Split host[:port] from path. A URL without a path implies '/'.
  let (authority, path) = match rest.find('/') {
    Some(idx) => (&rest[..idx], &rest[idx..]),
    None => (rest, "/"),
  };
  if authority.is_empty() {
    return Err(HttpError::BadUrl);
  }

  // 3. Optional port.
  let (host, port) = match authority.find(':') {
    Some(idx) => {
      let port_str = &authority[idx + 1..];
      let port = u16::from_str(port_str).map_err(|_| HttpError::BadUrl)?;
      (&authority[..idx], port)
    }
    None => (authority, 80),
  };

  // 4. IPv4 literal only (no DNS in this build).
  let ip = parse_ipv4(host).ok_or(HttpError::BadUrl)?;

  Ok(ParsedUrl { ip, port, path })
}

/// Tiny dotted-quad parser. We avoid `Ipv4Address::from_str` because the
/// version pinned in `embassy-net 0.9` does not expose `FromStr` in
/// `no_std` builds without optional features.
fn parse_ipv4(s: &str) -> Option<Ipv4Address> {
  let mut octets = [0u8; 4];
  let mut count = 0;
  for part in s.split('.') {
    if count == 4 {
      return None;
    }
    octets[count] = u8::from_str(part).ok()?;
    count += 1;
  }
  if count != 4 {
    return None;
  }
  Some(Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]))
}

// ============================================================================
// Response header parsing
// ============================================================================

/// Header-section parse result: byte range of the body prefix already
/// read into the caller's buffer, plus the `Content-Length` if any.
struct ResponseHead {
  /// Index in the caller's buffer where the body starts (just past
  /// `\r\n\r\n`).
  body_start: usize,
  /// `Content-Length` value, or `None` if the server didn't send one
  /// (HTTP/1.0 EOF framing).
  content_length: Option<u32>,
}

/// Parse the bytes accumulated in `buf[..filled]`, returning [`None`]
/// if the `\r\n\r\n` terminator hasn't arrived yet.
///
/// On a successful 2xx parse we return [`ResponseHead`] describing where
/// the body begins and how long it is. Non-2xx responses are surfaced
/// as [`HttpError::BadStatus`].
fn parse_response_head(buf: &[u8]) -> Result<Option<ResponseHead>, HttpError> {
  // Find the end-of-headers marker.
  let Some(term) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
    return Ok(None);
  };
  let head_str = core::str::from_utf8(&buf[..term]).map_err(|_| HttpError::BadUrl)?;
  let mut lines = head_str.split("\r\n");

  // Status line: "HTTP/1.x CODE REASON"
  let status_line = lines.next().ok_or(HttpError::BadStatus(0))?;
  let status = parse_status_line(status_line)?;
  if !(200..300).contains(&status) {
    return Err(HttpError::BadStatus(status));
  }

  // Headers: case-insensitive `Content-Length: N` is the only one we
  // care about. Everything else is ignored on purpose to keep code
  // size small.
  let mut content_length = None;
  for line in lines {
    if let Some(value) = strip_header_prefix(line, "content-length:") {
      // Spec says the value is a decimal integer; tolerate leading
      // whitespace.
      content_length = value.trim().parse::<u32>().ok();
    }
  }

  Ok(Some(ResponseHead {
    body_start: term + 4,
    content_length,
  }))
}

/// Parse a status line like `HTTP/1.1 200 OK` and return the numeric
/// status code.
fn parse_status_line(line: &str) -> Result<u16, HttpError> {
  // Expect at least three space-separated tokens.
  let mut parts = line.split(' ');
  let version = parts.next().unwrap_or("");
  let code_str = parts.next().unwrap_or("");
  if !version.starts_with("HTTP/1.") {
    return Err(HttpError::BadStatus(0));
  }
  code_str.parse::<u16>().map_err(|_| HttpError::BadStatus(0))
}

/// ASCII case-insensitive header prefix matcher.
///
/// Returns the value (everything after the colon) if `line` starts
/// with `prefix` (which must already include the trailing colon).
fn strip_header_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
  if line.len() < prefix.len() {
    return None;
  }
  if line.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
    Some(&line[prefix.len()..])
  } else {
    None
  }
}

// ============================================================================
// Public entry point
// ============================================================================

/// Download `url` and stream the body into `writer`, publishing
/// [`OtaProgress`] updates as bytes flow.
///
/// On success the writer is left ready for [`OtaWriter::finalize`]. On
/// any error the caller should call [`OtaWriter::abort`] and surface
/// the failure to the user; the inactive flash slot is left in
/// whatever state the partial write reached but OTA-data is untouched
/// so the device keeps running the current image.
///
/// # Errors
/// See [`HttpError`] variants. All errors are recoverable: dropping the
/// `OtaWriter` (via [`OtaWriter::abort`]) returns the flash handle to
/// the caller for normal preset-store resumption.
#[allow(
  clippy::large_stack_frames,
  reason = "the future owns the embassy_net::tcp::TcpSocket state machine and a few \
            response-parsing locals; ~2.4 KiB is comfortable inside the radio control task's \
            16 KiB Embassy task stack and the function only runs once per OTA invocation."
)]
pub async fn download_to_writer<'d>(
  stack: Stack<'static>,
  url: &str,
  writer: &mut OtaWriter<'d>,
) -> Result<(), HttpError> {
  publish_ota_progress(OtaProgress::Connecting).await;

  let ParsedUrl { ip, port, path } = parse_url(url)?;
  let path_owned: String = path.into();

  info!(
    "OTA HTTP GET: {}.{}.{}.{}:{} {}",
    ip.octets()[0],
    ip.octets()[1],
    ip.octets()[2],
    ip.octets()[3],
    port,
    path_owned.as_str()
  );

  // TCP buffers: heap-allocated so the caller's stack frame stays
  // within `deny(clippy::large_stack_frames)`. They live for the
  // duration of one OTA job (~30 s), then drop.
  let mut rx_buf: Box<[u8]> = vec![0u8; TCP_RX].into_boxed_slice();
  let mut tx_buf: Box<[u8]> = vec![0u8; TCP_TX].into_boxed_slice();
  let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
  socket.set_timeout(Some(READ_TIMEOUT));

  // Read scratch is on the heap for the same reason: 1 KiB on the
  // stack pushes this future past the project-wide 1 KiB threshold.
  let mut chunk_buf: Box<[u8]> = vec![0u8; READ_CHUNK].into_boxed_slice();
  let chunk: &mut [u8] = &mut chunk_buf;

  // Connect with bounded wait. `embassy_futures::select` is overkill
  // here — TcpSocket honours the timeout we already configured for
  // reads, but `connect` itself doesn't, so we wrap it manually.
  let connect_fut = socket.connect(IpEndpoint::new(IpAddress::Ipv4(ip), port));
  let connected = embassy_futures::select::select(connect_fut, Timer::after(CONNECT_TIMEOUT)).await;
  match connected {
    embassy_futures::select::Either::First(Ok(())) => {}
    embassy_futures::select::Either::First(Err(e)) => {
      warn!("OTA connect error: {:?}", e);
      return Err(HttpError::ConnectFailed);
    }
    embassy_futures::select::Either::Second(()) => {
      warn!(
        "OTA connect timed out after {} s",
        CONNECT_TIMEOUT.as_secs()
      );
      // Explicitly abort the socket so the underlying TCP state machine
      // releases its slot immediately rather than lingering in a half-open
      // state until the next garbage-collection pass.
      socket.abort();
      return Err(HttpError::ConnectFailed);
    }
  }

  // Send a minimal HTTP/1.0 request. HTTP/1.0 + `Connection: close` is
  // the simplest framing — the server will close after the body so we
  // don't need a chunked-transfer parser.
  let request = format_request(&path_owned, ip, port);
  if socket.write_all(request.as_bytes()).await.is_err() {
    return Err(HttpError::Io);
  }
  if socket.flush().await.is_err() {
    return Err(HttpError::Io);
  }

  // Header-collection phase: read into a growable buffer until we
  // spot \r\n\r\n. A simple cap defends against runaway responses.
  // Pre-allocate the full HEADER_LIMIT capacity up-front so we never
  // trigger a realloc on the embedded heap (which could fragment the
  // allocator). Real HTTP headers are well under 4 KiB; the single
  // allocation is predictable and freed as soon as streaming begins.
  let mut head_buf: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(HEADER_LIMIT);
  let response_head = loop {
    match socket.read(chunk).await {
      Ok(0) => return Err(HttpError::Truncated),
      Ok(n) => {
        if head_buf.len() + n > HEADER_LIMIT {
          return Err(HttpError::HeadersTooLarge);
        }
        head_buf.extend_from_slice(&chunk[..n]);
        if let Some(head) = parse_response_head(&head_buf)? {
          break head;
        }
      }
      Err(e) => {
        warn!("OTA header read error: {:?}", e);
        return Err(HttpError::Io);
      }
    }
  };

  let total = response_head.content_length.unwrap_or(0);
  debug!(
    "OTA HTTP head ok: status=2xx, content_length={}, body_prefix_in_buf={}",
    total,
    head_buf.len() - response_head.body_start
  );

  // Feed any body bytes already in `head_buf` to the writer.
  let body_prefix = &head_buf[response_head.body_start..];
  if !body_prefix.is_empty() {
    writer.write_chunk(body_prefix).await?;
  }
  let mut received: u32 = body_prefix.len() as u32;

  publish_ota_progress(OtaProgress::Downloading { received, total }).await;
  let mut last_progress = embassy_time::Instant::now();

  // Streaming phase: read directly into a stack chunk and forward.
  // Loop terminates on EOF, error, or once we've fetched
  // `Content-Length` bytes (when known).
  loop {
    if let Some(expected) = response_head.content_length
      && received >= expected
    {
      break;
    }
    match socket.read(chunk).await {
      Ok(0) => {
        // HTTP/1.0 EOF framing. If we had a Content-Length, validate
        // it. Otherwise treat EOF as the whole body.
        if let Some(expected) = response_head.content_length
          && received < expected
        {
          warn!(
            "OTA truncated: received {}/{} bytes before peer closed",
            received, expected
          );
          return Err(HttpError::Truncated);
        }
        break;
      }
      Ok(n) => {
        writer.write_chunk(&chunk[..n]).await?;
        received = received.saturating_add(n as u32);

        // Throttle progress publishes to avoid waking the UI on
        // every 1 KiB chunk.
        if last_progress.elapsed() >= PROGRESS_REPUBLISH_INTERVAL {
          publish_ota_progress(OtaProgress::Downloading { received, total }).await;
          last_progress = embassy_time::Instant::now();
        }
      }
      Err(e) => {
        warn!("OTA body read error: {:?}", e);
        return Err(HttpError::Io);
      }
    }
  }

  publish_ota_progress(OtaProgress::Downloading {
    received,
    total: response_head.content_length.unwrap_or(received),
  })
  .await;

  // Closing politely lets the peer's accept loop free the socket
  // promptly; on this side we drop right after.
  socket.close();
  Ok(())
}

/// Build a minimal HTTP/1.0 request string.
///
/// Heap-allocated because the `Host:` header carries the IP literal
/// (5..=15 chars) plus the port, which can't be `&'static`. The
/// allocation lives for one `write_all` call and is freed immediately.
fn format_request(path: &str, ip: Ipv4Address, port: u16) -> String {
  use core::fmt::Write;
  let octets = ip.octets();
  let mut s = String::with_capacity(64 + path.len());
  // Errors on `String` `Write` are infallible; ignore.
  let _ = write!(
    s,
    "GET {} HTTP/1.0\r\nHost: {}.{}.{}.{}:{}\r\nUser-Agent: esp-radio-ota/1.0\r\nConnection: close\r\nAccept: */*\r\n\r\n",
    path, octets[0], octets[1], octets[2], octets[3], port,
  );
  s
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_typical_url() {
    let p = parse_url("http://192.168.1.10:8000/firmware.bin").unwrap();
    assert_eq!(p.ip, Ipv4Address::new(192, 168, 1, 10));
    assert_eq!(p.port, 8000);
    assert_eq!(p.path, "/firmware.bin");
  }

  #[test]
  fn defaults_port_to_80() {
    let p = parse_url("http://10.0.0.1/img").unwrap();
    assert_eq!(p.port, 80);
    assert_eq!(p.path, "/img");
  }

  #[test]
  fn empty_path_becomes_slash() {
    let p = parse_url("http://10.0.0.1").unwrap();
    assert_eq!(p.path, "/");
  }

  #[test]
  fn rejects_https() {
    assert!(matches!(
      parse_url("https://10.0.0.1/x"),
      Err(HttpError::BadUrl)
    ));
  }

  #[test]
  fn rejects_dns_hostname() {
    assert!(matches!(
      parse_url("http://example.com/x"),
      Err(HttpError::BadUrl)
    ));
  }

  #[test]
  fn parses_status_line_ok() {
    assert_eq!(parse_status_line("HTTP/1.1 200 OK").unwrap(), 200);
    assert_eq!(parse_status_line("HTTP/1.0 204 No Content").unwrap(), 204);
  }

  #[test]
  fn rejects_non_2xx_status() {
    let head = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found";
    assert!(matches!(
      parse_response_head(head),
      Err(HttpError::BadStatus(404))
    ));
  }

  #[test]
  fn extracts_content_length_case_insensitive() {
    let head = b"HTTP/1.1 200 OK\r\ncontent-length: 1234\r\n\r\nbody";
    let parsed = parse_response_head(head).unwrap().unwrap();
    assert_eq!(parsed.content_length, Some(1234));
    assert_eq!(parsed.body_start, head.len() - "body".len());
  }

  #[test]
  fn waits_for_full_terminator() {
    let head = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n";
    assert!(parse_response_head(head).unwrap().is_none());
  }
}
