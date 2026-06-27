//! ota-serve — minimal single-file HTTP server for esp-radio OTA development.
//!
//! Replaces the `python -m http.server` workflow described in the project
//! README. Reads one firmware image from disk and serves it under
//! `/firmware.bin`. Prints every reachable LAN URL plus a terminal QR code so
//! that the URL can be pasted into the device's web console with one tap.
//!
//! Design notes:
//! - Single-shot read into memory: OTA images are typically 1-2 MB on
//!   ESP32-C6, far below host RAM budget; keeps every request lock-free.
//! - `tiny_http` chosen over `axum`/`hyper`: zero async runtime, ~2 deps,
//!   fast `cargo build`. This binary is dev-only.
//! - No HTTPS: the device-side downloader is plain HTTP (see
//!   `src/bin/radio/ota/http_download.rs`).

use std::error::Error;
use std::fs;
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use tiny_http::{Header, Method, Response, Server};

const DEFAULT_PORT: u16 = 8000;
const DEFAULT_IMAGE: &str = "target/riscv32imac-unknown-none-elf/release/radio.bin";
const FIRMWARE_PATH: &str = "/firmware.bin";

struct Args {
  image: PathBuf,
  port: u16,
  bind: IpAddr,
}

impl Args {
  fn parse() -> Result<Self, String> {
    let mut image: Option<PathBuf> = None;
    let mut port = DEFAULT_PORT;
    let mut bind = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
      match arg.as_str() {
        "-h" | "--help" => {
          print_help();
          std::process::exit(0);
        }
        "--image" | "-i" => {
          image = Some(
            iter
              .next()
              .ok_or_else(|| "--image requires a path".to_string())?
              .into(),
          );
        }
        "--port" | "-p" => {
          let raw = iter
            .next()
            .ok_or_else(|| "--port requires a value".to_string())?;
          port = raw
            .parse()
            .map_err(|err| format!("invalid --port: {err}"))?;
        }
        "--bind" | "-b" => {
          let raw = iter
            .next()
            .ok_or_else(|| "--bind requires an IP".to_string())?;
          bind = raw
            .parse()
            .map_err(|err| format!("invalid --bind: {err}"))?;
        }
        other => return Err(format!("unknown argument: {other}")),
      }
    }

    Ok(Self {
      image: image.unwrap_or_else(|| PathBuf::from(DEFAULT_IMAGE)),
      port,
      bind,
    })
  }
}

fn print_help() {
  println!(
    "ota-serve — serve a single firmware image for esp-radio OTA\n\
         \n\
         USAGE:\n    \
             ota-serve [--image PATH] [--port N] [--bind IP]\n\
         \n\
         OPTIONS:\n    \
             -i, --image PATH   Firmware .bin to serve (default: {DEFAULT_IMAGE})\n    \
             -p, --port  N      TCP port (default: {DEFAULT_PORT})\n    \
             -b, --bind  IP     Listen address (default: 0.0.0.0)\n    \
             -h, --help         Show this help and exit\n\
         \n\
         The image is exposed under {FIRMWARE_PATH}. Paste the printed URL into\n\
         the esp-radio web console (`http://esp-radio.local/`) → OTA card.\n"
  );
}

fn main() -> ExitCode {
  let args = match Args::parse() {
    Ok(args) => args,
    Err(err) => {
      eprintln!("error: {err}\n");
      print_help();
      return ExitCode::from(2);
    }
  };

  if let Err(err) = run(args) {
    eprintln!("error: {err}");
    ExitCode::FAILURE
  } else {
    ExitCode::SUCCESS
  }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
  let bytes = fs::read(&args.image).map_err(|err| {
    format!(
      "cannot read firmware image '{}': {err}\n  hint: run `cargo make ota-image` first",
      args.image.display()
    )
  })?;
  let size_kib = bytes.len() as f64 / 1024.0;
  println!(
    "📦 firmware: {} ({:.1} KiB)",
    args.image.display(),
    size_kib
  );

  let addr = SocketAddr::new(args.bind, args.port);
  let server = Server::http(addr).map_err(|err| format!("bind {addr} failed: {err}"))?;
  println!("🚀 listening on http://{addr}{FIRMWARE_PATH}");

  print_lan_urls(args.port);
  print_curl_hint(args.port);
  println!("\nPress Ctrl-C to stop.\n");

  // `Arc<[u8]>` so each request handler shares the buffer without cloning.
  let payload: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());

  for request in server.incoming_requests() {
    handle(request, &payload);
  }
  Ok(())
}

fn handle(request: tiny_http::Request, payload: &Arc<[u8]>) {
  let started = Instant::now();
  let method = request.method().clone();
  let url = request.url().to_owned();
  let remote = request
    .remote_addr()
    .map(ToString::to_string)
    .unwrap_or_else(|| "?".into());
  let ua = request
    .headers()
    .iter()
    .find(|h| h.field.equiv("User-Agent"))
    .map(|h| h.value.as_str().to_owned())
    .unwrap_or_else(|| "-".to_owned());

  let result = match (&method, url.as_str()) {
    (Method::Get, FIRMWARE_PATH) => respond_firmware(request, payload, false),
    (Method::Head, FIRMWARE_PATH) => respond_firmware(request, payload, true),
    (Method::Get, "/") => respond_index(request),
    _ => respond_404(request),
  };

  let elapsed = started.elapsed();
  match result {
    Ok(status) => println!(
      "  {method:5} {url:24} → {status} ({remote}, {ua}, {:.0?})",
      elapsed
    ),
    Err(err) => eprintln!("  {method:5} {url:24} ✗ {err} ({remote}, {ua})"),
  }
}

fn respond_firmware(
  request: tiny_http::Request,
  payload: &Arc<[u8]>,
  head_only: bool,
) -> std::io::Result<u16> {
  let header = Header::from_bytes(b"Content-Type".as_slice(), b"application/octet-stream")
    .expect("static header");
  if head_only {
    let response = Response::empty(200)
      .with_header(header)
      .with_header(content_length(payload.len()));
    request.respond(response)?;
    return Ok(200);
  }
  let cursor = Cursor::new(Arc::clone(payload));
  let response = Response::new(
    tiny_http::StatusCode(200),
    vec![header],
    cursor,
    Some(payload.len()),
    None,
  );
  request.respond(response)?;
  Ok(200)
}

fn content_length(len: usize) -> Header {
  let value = len.to_string();
  Header::from_bytes(b"Content-Length".as_slice(), value.as_bytes()).expect("numeric header")
}

fn respond_index(request: tiny_http::Request) -> std::io::Result<u16> {
  let body = format!(
    "esp-radio OTA dev server\n\nGET {FIRMWARE_PATH} → firmware image\n\
         \nPaste the printed URL into the device's web console.\n"
  );
  let response = Response::from_string(body).with_header(
    Header::from_bytes(b"Content-Type".as_slice(), b"text/plain; charset=utf-8")
      .expect("static header"),
  );
  request.respond(response)?;
  Ok(200)
}

fn respond_404(request: tiny_http::Request) -> std::io::Result<u16> {
  let body = format!("404 — only GET {FIRMWARE_PATH} is served\n");
  let response = Response::from_string(body).with_status_code(404);
  request.respond(response)?;
  Ok(404)
}

fn print_lan_urls(port: u16) {
  let ips = match local_ip_address::list_afinet_netifas() {
    Ok(list) => list,
    Err(err) => {
      eprintln!("⚠️  cannot list network interfaces: {err}");
      return;
    }
  };

  let mut shown_any = false;
  let mut qr_target: Option<String> = None;

  println!("\nReachable URLs:");
  for (name, addr) in ips {
    let IpAddr::V4(v4) = addr else {
      continue; // ESP-radio downloader is IPv4-only
    };
    if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
      continue;
    }
    let url = format!("  http://{v4}:{port}{FIRMWARE_PATH}  ({name})");
    println!("{url}");
    shown_any = true;
    if qr_target.is_none() {
      qr_target = Some(format!("http://{v4}:{port}{FIRMWARE_PATH}"));
    }
  }

  if !shown_any {
    println!("  (no non-loopback IPv4 found — check Wi-Fi/Ethernet)");
    return;
  }

  if let Some(url) = qr_target {
    match QrCode::new(url.as_bytes()) {
      Ok(code) => {
        let rendered = code
          .render::<Dense1x2>()
          .dark_color(Dense1x2::Light)
          .light_color(Dense1x2::Dark)
          .quiet_zone(true)
          .build();
        println!("\n{rendered}");
      }
      Err(err) => eprintln!("⚠️  QR render failed: {err}"),
    }
  }
}

fn print_curl_hint(port: u16) {
  println!(
    "Trigger from a shell:\n  \
         curl -X POST http://esp-radio.local/api/ota \\\n      \
             -H 'Content-Type: application/json' \\\n      \
             -d '{{\"url\":\"http://<your-ip>:{port}{FIRMWARE_PATH}\"}}'"
  );
}
