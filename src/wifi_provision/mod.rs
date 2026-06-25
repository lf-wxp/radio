//! WiFi Provisioning Module
//!
//! Provides SoftAP + lightweight HTTP Server based WiFi provisioning.
//! Users connect to the ESP32's AP, visit the captive portal web page,
//! and submit their WiFi credentials (SSID + password).
//!
//! # Architecture
//! - ESP32 starts in AP mode with a configurable SSID
//! - A lightweight HTTP server (picoserve) serves a configuration page
//! - User submits WiFi credentials via HTML form
//! - Module handles the full flow: provisioning → save → connect
//!
//! # Example
//! ```no_run
//! use radio::wifi_provision::{WifiProvisioner, ProvisioningConfig, ConnectionConfig};
//!
//! // One-line provisioning + connection:
//! let stack = provisioner.provision_and_connect(&spawner, wifi_peripheral).await;
//! ```

pub mod storage;

use alloc::string::String;
use core::fmt;

use embassy_executor::Spawner;
use embassy_net::{Ipv4Address, Ipv4Cidr, StackResources, StaticConfigV4};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use esp_hal::rng::Rng;
use esp_radio::wifi::{
  Config as WifiConfig, ControllerConfig, Interface, WifiController, ap::AccessPointConfig,
  sta::StationConfig,
};
use esp_storage::FlashStorage;

use self::storage::{CredentialStorage, StorageError};

extern crate alloc;

/// WiFi credentials received from the provisioning portal.
#[derive(Clone, Debug)]
pub struct WifiCredentials {
  /// Target WiFi SSID
  pub ssid: String,
  /// Target WiFi password
  pub password: String,
}

impl fmt::Display for WifiCredentials {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "WifiCredentials {{ ssid: \"{}\", password: ****** }}",
      self.ssid
    )
  }
}

/// Configuration for the provisioning AP.
#[derive(Clone, Debug)]
pub struct ProvisioningConfig {
  /// SSID of the provisioning AP (what users see and connect to)
  pub ap_ssid: &'static str,
  /// Password for the provisioning AP (empty = open network)
  pub ap_password: &'static str,
  /// Channel for the AP (1-13)
  pub channel: u8,
  /// Gateway IP address for the AP network (also the HTTP server address)
  pub gateway: [u8; 4],
}

impl Default for ProvisioningConfig {
  fn default() -> Self {
    Self {
      ap_ssid: "ESP32-Setup",
      ap_password: "",
      channel: 1,
      gateway: [192, 168, 4, 1],
    }
  }
}

/// Configuration for Station mode connection behavior.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
  /// Maximum number of connection retry attempts before giving up.
  pub max_retries: u8,
  /// Delay between retry attempts in seconds.
  pub retry_delay_secs: u8,
  /// Whether to automatically clear credentials on connection failure.
  pub clear_on_failure: bool,
}

impl Default for ConnectionConfig {
  fn default() -> Self {
    Self {
      max_retries: 3,
      retry_delay_secs: 3,
      clear_on_failure: true,
    }
  }
}

/// Errors that can occur during WiFi connection.
#[derive(Debug, defmt::Format)]
pub enum ConnectionError {
  /// WiFi initialization failed.
  WifiInit,
  /// All connection retry attempts exhausted.
  ConnectionFailed,
  /// Credentials were cleared due to connection failure.
  CredentialsCleared,
  /// DHCP timeout - failed to obtain IP address.
  DhcpTimeout,
}

impl fmt::Display for ConnectionError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::WifiInit => write!(f, "WiFi initialization failed"),
      Self::ConnectionFailed => write!(f, "all connection retries exhausted"),
      Self::CredentialsCleared => write!(f, "credentials cleared after connection failure"),
      Self::DhcpTimeout => write!(f, "DHCP timeout"),
    }
  }
}

/// Result of a successful WiFi connection.
///
/// Contains the network stack for application use and the WiFi controller
/// for monitoring connection state.
pub struct ConnectedWifi {
  /// The embassy-net stack, ready for TCP/UDP communication.
  pub stack: embassy_net::Stack<'static>,
  /// The WiFi controller for checking/managing connection state.
  pub controller: WifiController<'static>,
  /// The connected SSID.
  pub ssid: String,
}

/// Provisioning error types.
#[derive(Debug)]
pub enum ProvisioningError {
  /// WiFi initialization failed
  WifiInit,
  /// Network stack error
  Network,
  /// HTTP server error
  HttpServer,
  /// Invalid credentials received
  InvalidCredentials,
}

impl fmt::Display for ProvisioningError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::WifiInit => write!(f, "wifi initialization failed"),
      Self::Network => write!(f, "network stack error"),
      Self::HttpServer => write!(f, "http server error"),
      Self::InvalidCredentials => write!(f, "invalid credentials received"),
    }
  }
}

/// HTML page served by the captive portal.
const PROVISION_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>WiFi Setup</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#eee;min-height:100vh;display:flex;align-items:center;justify-content:center}
.card{background:#16213e;border-radius:12px;padding:2rem;width:90%;max-width:360px;box-shadow:0 8px 32px rgba(0,0,0,.3)}
h1{text-align:center;margin-bottom:1.5rem;font-size:1.4rem;color:#4fc3f7}
label{display:block;margin-bottom:.3rem;font-size:.9rem;color:#aaa}
input{width:100%;padding:.7rem;margin-bottom:1rem;border:1px solid #333;border-radius:6px;background:#0f3460;color:#eee;font-size:1rem}
input:focus{outline:none;border-color:#4fc3f7}
button{width:100%;padding:.8rem;background:#4fc3f7;color:#1a1a2e;border:none;border-radius:6px;font-size:1rem;font-weight:bold;cursor:pointer}
button:hover{background:#81d4fa}
.info{text-align:center;margin-top:1rem;font-size:.8rem;color:#666}
</style>
</head>
<body>
<div class="card">
<h1>&#128225; WiFi Setup</h1>
<form method="POST" action="/connect">
<label for="ssid">WiFi Name (SSID)</label>
<input type="text" id="ssid" name="ssid" required maxlength="32" placeholder="Enter WiFi name">
<label for="pass">Password</label>
<input type="password" id="pass" name="password" maxlength="64" placeholder="Enter WiFi password">
<button type="submit">Connect</button>
</form>
<p class="info">ESP32 WiFi Provisioning</p>
</div>
</body>
</html>"#;

/// HTML page shown after successful credential submission.
const SUCCESS_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>WiFi Setup - Success</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#eee;min-height:100vh;display:flex;align-items:center;justify-content:center}
.card{background:#16213e;border-radius:12px;padding:2rem;width:90%;max-width:360px;box-shadow:0 8px 32px rgba(0,0,0,.3);text-align:center}
h1{margin-bottom:1rem;font-size:1.4rem;color:#66bb6a}
p{color:#aaa;line-height:1.6}
</style>
</head>
<body>
<div class="card">
<h1>&#9989; Credentials Saved!</h1>
<p>The device will now attempt to connect to your WiFi network. This AP will shut down shortly.</p>
</div>
</body>
</html>"#;

/// Parse URL-encoded form data to extract SSID and password.
///
/// Handles basic percent-decoding for common characters.
pub fn parse_form_data(body: &str) -> Option<WifiCredentials> {
  let mut ssid = None;
  let mut password = None;

  for pair in body.split('&') {
    let mut parts = pair.splitn(2, '=');
    let key = parts.next()?;
    let value = parts.next().unwrap_or("");
    let decoded = url_decode(value);

    match key {
      "ssid" => ssid = Some(decoded),
      "password" => password = Some(decoded),
      _ => {}
    }
  }

  let ssid = ssid?;
  if ssid.is_empty() {
    return None;
  }

  Some(WifiCredentials {
    ssid,
    password: password.unwrap_or_default(),
  })
}

/// Simple URL percent-decoding.
fn url_decode(input: &str) -> String {
  let mut output = String::with_capacity(input.len());
  let mut chars = input.bytes();

  while let Some(b) = chars.next() {
    match b {
      b'+' => output.push(' '),
      b'%' => {
        let hi = chars.next().and_then(hex_val);
        let lo = chars.next().and_then(hex_val);
        if let (Some(h), Some(l)) = (hi, lo) {
          output.push((h << 4 | l) as char);
        }
      }
      _ => output.push(b as char),
    }
  }

  output
}

/// Convert a hex ASCII byte to its numeric value.
fn hex_val(b: u8) -> Option<u8> {
  match b {
    b'0'..=b'9' => Some(b - b'0'),
    b'a'..=b'f' => Some(b - b'a' + 10),
    b'A'..=b'F' => Some(b - b'A' + 10),
    _ => None,
  }
}

/// Get the provisioning HTML page content.
pub fn provision_page() -> &'static str {
  PROVISION_HTML
}

/// Get the success HTML page content.
pub fn success_page() -> &'static str {
  SUCCESS_HTML
}

/// HTTP request method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
  Get,
  Post,
  Other,
}

/// A minimal HTTP request parsed from raw bytes.
#[derive(Debug)]
pub struct HttpRequest<'a> {
  /// HTTP method
  pub method: HttpMethod,
  /// Request path
  pub path: &'a str,
  /// Request body (for POST requests)
  pub body: &'a str,
  /// Content-Length header value
  pub content_length: usize,
}

/// Helper for writing formatted content into a fixed-size buffer.
///
/// Implements `core::fmt::Write` with **all-or-nothing** semantics: if a
/// chunk would overflow the buffer, the write is rejected entirely
/// (without partially copying bytes) and `Err(fmt::Error)` is returned.
/// This guarantees that on overflow the caller can detect the failure
/// and the buffer contents up to `pos` remain a valid prefix of the
/// requested format string.
struct BufWriter<'a> {
  buf: &'a mut [u8],
  pos: usize,
  /// Sticky overflow flag: once set, all subsequent writes return Err.
  overflowed: bool,
}

impl<'a> BufWriter<'a> {
  fn new(buf: &'a mut [u8]) -> Self {
    Self {
      buf,
      pos: 0,
      overflowed: false,
    }
  }

  /// Returns `Some(written_len)` if the whole format succeeded, `None` if
  /// any write overflowed (in which case `pos` represents how far we got).
  fn finish(self) -> Option<usize> {
    if self.overflowed {
      None
    } else {
      Some(self.pos)
    }
  }
}

impl<'a> core::fmt::Write for BufWriter<'a> {
  fn write_str(&mut self, s: &str) -> fmt::Result {
    if self.overflowed {
      return Err(fmt::Error);
    }
    let bytes = s.as_bytes();
    let remaining = self.buf.len() - self.pos;
    if bytes.len() > remaining {
      // All-or-nothing: refuse the partial write and remember the failure.
      self.overflowed = true;
      return Err(fmt::Error);
    }
    self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
    self.pos += bytes.len();
    Ok(())
  }
}

/// Parse a raw HTTP request from a byte buffer.
///
/// This is a minimal parser that extracts method, path, content-length, and body.
pub fn parse_http_request(buf: &[u8], len: usize) -> Option<HttpRequest<'_>> {
  let request_str = core::str::from_utf8(&buf[..len]).ok()?;

  // Split headers and body
  let (headers_part, body) = if let Some(pos) = request_str.find("\r\n\r\n") {
    (&request_str[..pos], &request_str[pos + 4..])
  } else {
    (request_str, "")
  };

  // Parse request line
  let request_line = headers_part.lines().next()?;
  let mut parts = request_line.split_whitespace();
  let method_str = parts.next()?;
  let path = parts.next()?;

  let method = match method_str {
    "GET" => HttpMethod::Get,
    "POST" => HttpMethod::Post,
    _ => HttpMethod::Other,
  };

  // Parse Content-Length
  let mut content_length = 0;
  const CL_HEADER: &[u8] = b"content-length:";
  for line in headers_part.lines().skip(1) {
    let line_bytes = line.as_bytes();
    if line_bytes.len() >= CL_HEADER.len()
      && line_bytes[..CL_HEADER.len()].eq_ignore_ascii_case(CL_HEADER)
      && let Some(val) = line.split(':').nth(1)
    {
      content_length = val.trim().parse().unwrap_or(0);
    }
  }

  Some(HttpRequest {
    method,
    path,
    body,
    content_length,
  })
}

/// Format an HTTP response with the given status code, content type, and body.
///
/// Returns the number of bytes written, or `0` if the response did not fit
/// in `buf` (caller should treat 0 as a hard failure and close the socket).
pub fn format_http_response(
  buf: &mut [u8],
  status: u16,
  status_text: &str,
  content_type: &str,
  body: &str,
) -> usize {
  use core::fmt::Write;

  let mut writer = BufWriter::new(buf);
  let _ = write!(
    writer,
    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
    status,
    status_text,
    content_type,
    body.len(),
    body
  );

  writer.finish().unwrap_or(0)
}

/// Format a redirect HTTP response (302 Found).
///
/// Returns the number of bytes written, or `0` if the response did not fit.
pub fn format_redirect_response(buf: &mut [u8], location: &str) -> usize {
  use core::fmt::Write;

  let mut writer = BufWriter::new(buf);
  let _ = write!(
    writer,
    "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    location
  );

  writer.finish().unwrap_or(0)
}

/// WiFi provisioner that encapsulates credential storage logic.
///
/// This struct provides a high-level API for managing WiFi credentials
/// with automatic Flash persistence. Callers don't need to directly
/// operate `CredentialStorage`.
///
/// # Usage Flow
/// ```no_run
/// let mut provisioner = WifiProvisioner::new(flash);
///
/// // One-line: provision (if needed) + connect
/// let wifi = provisioner.provision_and_connect(
///     &spawner, wifi_peripheral, &conn_config, &prov_config,
/// ).await.unwrap();
/// ```
pub struct WifiProvisioner<'d> {
  storage: CredentialStorage<'d>,
}

impl<'d> WifiProvisioner<'d> {
  /// Create a new provisioner with default Flash storage offset.
  pub fn new(flash: FlashStorage<'d>) -> Self {
    Self {
      storage: CredentialStorage::new(flash),
    }
  }

  /// Create a new provisioner with a custom Flash storage offset.
  ///
  /// The offset must be sector-aligned (multiple of 4096).
  pub fn with_offset(flash: FlashStorage<'d>, offset: u32) -> Self {
    Self {
      storage: CredentialStorage::with_offset(flash, offset),
    }
  }

  /// Try to load previously saved WiFi credentials from Flash.
  ///
  /// Returns `Some(credentials)` if valid credentials exist,
  /// `None` if no credentials are stored or data is corrupted
  /// (corrupted data will be automatically cleared).
  pub fn load_credentials(&mut self) -> Option<WifiCredentials> {
    match self.storage.load() {
      Ok(creds) => creds,
      Err(_) => {
        // Data corrupted, clear it silently
        let _ = self.storage.clear();
        None
      }
    }
  }

  /// Save WiFi credentials to Flash for next boot.
  ///
  /// Returns `Ok(())` on success, or `Err(StorageError)` if the
  /// save operation fails.
  pub fn save_credentials(&mut self, credentials: &WifiCredentials) -> Result<(), StorageError> {
    self.storage.save(credentials)
  }

  /// Clear stored credentials from Flash.
  ///
  /// After calling this, the next boot will require re-provisioning.
  pub fn clear_credentials(&mut self) -> Result<(), StorageError> {
    self.storage.clear()
  }

  /// Check if credentials are stored without fully parsing them.
  ///
  /// This is a fast check that only reads the magic number header.
  pub fn has_credentials(&mut self) -> bool {
    self.storage.has_credentials()
  }

  /// Consume the provisioner and return the underlying [`FlashStorage`].
  ///
  /// Used by the binary to hand the (singleton) flash peripheral to a
  /// different subsystem — e.g. the FM-radio preset store — once
  /// provisioning + connection are done.
  pub fn into_flash(self) -> FlashStorage<'d> {
    self.storage.into_flash()
  }

  /// Complete provisioning + connection flow in one call.
  ///
  /// This method handles the entire WiFi lifecycle:
  /// 1. Check Flash for saved credentials
  /// 2. If no credentials, start SoftAP captive portal for provisioning
  /// 3. Save new credentials to Flash
  /// 4. Connect to the target WiFi in Station mode
  /// 5. Wait for DHCP IP assignment
  ///
  /// On connection failure (with `clear_on_failure` enabled), credentials
  /// are automatically cleared so the next boot will re-provision.
  ///
  /// # Arguments
  /// * `spawner` - Embassy task spawner for the network task
  /// * `wifi` - WiFi peripheral (consumed)
  /// * `conn_config` - Station connection configuration
  /// * `prov_config` - Provisioning AP configuration (used only if no saved credentials)
  /// * `stack_resources` - Static network stack resources
  ///
  /// # Returns
  /// `Ok(ConnectedWifi)` with the ready-to-use network stack, or
  /// `Err(ConnectionError)` if connection fails.
  pub async fn provision_and_connect(
    &mut self,
    spawner: &Spawner,
    wifi: esp_hal::peripherals::WIFI<'static>,
    conn_config: &ConnectionConfig,
    prov_config: &ProvisioningConfig,
    stack_resources: &'static mut StackResources<3>,
  ) -> Result<ConnectedWifi, ConnectionError> {
    // Step 1: Get credentials (from Flash or via provisioning)
    let credentials = if let Some(creds) = self.load_credentials() {
      defmt::info!(
        "Loaded saved credentials, SSID: \"{}\"",
        creds.ssid.as_str()
      );
      creds
    } else {
      defmt::info!("No saved credentials. Starting provisioning...");
      let creds = run_provisioning_server(spawner, wifi, prov_config, stack_resources).await;

      // Save to Flash
      if let Err(e) = self.save_credentials(&creds) {
        defmt::info!("WARNING: Failed to save credentials: {:?}", e);
      } else {
        defmt::info!("Credentials saved to Flash.");
      }

      // After provisioning, WiFi peripheral is consumed in AP mode.
      // A reboot is required to connect in Station mode.
      defmt::info!("Provisioning complete! Reboot to connect.");
      defmt::info!("SSID: \"{}\"", creds.ssid.as_str());

      // We cannot switch from AP to STA without re-initializing,
      // so we loop here waiting for reboot.
      loop {
        defmt::info!("Please reboot the device to connect to WiFi.");
        Timer::after(Duration::from_secs(5)).await;
      }
    };

    // Step 2: Connect in Station mode
    let result = connect_station(spawner, wifi, &credentials, conn_config, stack_resources).await;

    match result {
      Ok(connected) => Ok(connected),
      Err(e) => {
        // Clear credentials on failure if configured
        if conn_config.clear_on_failure {
          defmt::info!("Connection failed, clearing saved credentials.");
          let _ = self.clear_credentials();
        }
        Err(e)
      }
    }
  }
}

/// Connect to a WiFi network in Station mode.
///
/// This function initializes WiFi in Station mode, connects to the
/// specified network with retries, and waits for DHCP IP assignment.
///
/// # Arguments
/// * `spawner` - Embassy task spawner for the network task
/// * `wifi` - WiFi peripheral (consumed)
/// * `credentials` - Target WiFi SSID and password
/// * `config` - Connection behavior configuration
/// * `stack_resources` - Static network stack resources
///
/// # Returns
/// `Ok(ConnectedWifi)` on success, `Err(ConnectionError)` on failure.
pub async fn connect_station(
  spawner: &Spawner,
  wifi: esp_hal::peripherals::WIFI<'static>,
  credentials: &WifiCredentials,
  config: &ConnectionConfig,
  stack_resources: &'static mut StackResources<3>,
) -> Result<ConnectedWifi, ConnectionError> {
  defmt::info!("Initializing WiFi in Station mode...");

  // Configure Station mode
  let sta_config = StationConfig::default()
    .with_ssid(credentials.ssid.as_str())
    .with_password(String::from(credentials.password.as_str()));

  let controller_config =
    ControllerConfig::default().with_initial_config(WifiConfig::Station(sta_config));

  let (mut wifi_controller, interfaces) =
    esp_radio::wifi::new(wifi, controller_config).map_err(|_| ConnectionError::WifiInit)?;

  defmt::info!(
    "WiFi initialized. Connecting to \"{}\"...",
    credentials.ssid.as_str()
  );

  // Set up network stack with DHCP
  let net_config = embassy_net::Config::dhcpv4(Default::default());
  // Hardware RNG provides true random seed once WiFi is enabled.
  let rng = Rng::new();
  let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
  let (stack, runner) = embassy_net::new(interfaces.station, net_config, stack_resources, seed);

  let token = net_task(runner).map_err(|_| ConnectionError::WifiInit)?;
  spawner.spawn(token);

  // Connect with retries
  let connected = connect_with_retries(
    &mut wifi_controller,
    config.max_retries,
    config.retry_delay_secs,
  )
  .await;

  if !connected {
    return Err(ConnectionError::ConnectionFailed);
  }

  // Wait for DHCP IP address
  defmt::info!("Connected! Waiting for DHCP IP...");
  wait_for_dhcp_ip(stack).await;

  Ok(ConnectedWifi {
    stack,
    controller: wifi_controller,
    ssid: credentials.ssid.clone(),
  })
}

/// Run the SoftAP + HTTP captive portal provisioning server.
///
/// Starts an AP, serves a configuration web page, and waits for
/// the user to submit WiFi credentials.
///
/// # Arguments
/// * `spawner` - Embassy task spawner for the network task
/// * `wifi` - WiFi peripheral (consumed)
/// * `config` - Provisioning AP configuration
/// * `stack_resources` - Static network stack resources
///
/// # Returns
/// The WiFi credentials submitted by the user.
pub async fn run_provisioning_server(
  spawner: &Spawner,
  wifi: esp_hal::peripherals::WIFI<'static>,
  config: &ProvisioningConfig,
  stack_resources: &'static mut StackResources<3>,
) -> WifiCredentials {
  defmt::info!("Starting provisioning AP: \"{}\"", config.ap_ssid);

  // WiFi AP Mode Setup
  let ap_config = AccessPointConfig::default()
    .with_ssid(config.ap_ssid)
    .with_channel(config.channel);

  let controller_config =
    ControllerConfig::default().with_initial_config(WifiConfig::AccessPoint(ap_config));

  let (_wifi_controller, interfaces) =
    esp_radio::wifi::new(wifi, controller_config).expect("Failed to initialize WiFi AP");

  defmt::info!("WiFi AP started successfully");

  // Network Stack Setup (Static IP for AP mode)
  let gw = config.gateway;
  let static_config = StaticConfigV4 {
    address: Ipv4Cidr::new(Ipv4Address::new(gw[0], gw[1], gw[2], gw[3]), 24),
    gateway: Some(Ipv4Address::new(gw[0], gw[1], gw[2], gw[3])),
    dns_servers: Default::default(),
  };

  let net_config = embassy_net::Config::ipv4_static(static_config);
  // Hardware RNG provides true random seed once WiFi is enabled.
  let rng = Rng::new();
  let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
  let (stack, runner) =
    embassy_net::new(interfaces.access_point, net_config, stack_resources, seed);

  spawner.spawn(net_task(runner).expect("Failed to create net_task"));

  // Wait for the stack to be ready
  defmt::info!("Waiting for network stack...");
  stack.wait_link_up().await;
  defmt::info!(
    "AP ready! IP: {}.{}.{}.{} - Connect to \"{}\"",
    gw[0],
    gw[1],
    gw[2],
    gw[3],
    config.ap_ssid
  );

  // HTTP Server - Captive Portal
  let mut rx_buf = [0u8; 2048];
  let mut tx_buf = [0u8; 4096];

  loop {
    let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(10)));

    if socket.accept(80).await.is_err() {
      continue;
    }

    // Read the HTTP request
    let mut buf = [0u8; 2048];
    let mut total_read = 0;

    loop {
      match socket.read(&mut buf[total_read..]).await {
        Ok(0) => break,
        Ok(n) => {
          total_read += n;
          if buf[..total_read]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .is_some()
          {
            let request_str = core::str::from_utf8(&buf[..total_read]).unwrap_or("");
            if request_str.starts_with("POST") {
              if let Some(req) = parse_http_request(&buf, total_read)
                && req.body.len() >= req.content_length
              {
                break;
              }
            } else {
              break;
            }
          }
          if total_read >= buf.len() {
            break;
          }
        }
        Err(_) => break,
      }
    }

    if total_read == 0 {
      socket.close();
      continue;
    }

    // Parse and handle the request
    let mut response_buf = [0u8; 4096];
    let response_len = if let Some(request) = parse_http_request(&buf, total_read) {
      match (request.method, request.path) {
        (HttpMethod::Get, "/") | (HttpMethod::Get, "/index.html") => format_http_response(
          &mut response_buf,
          200,
          "OK",
          "text/html; charset=utf-8",
          provision_page(),
        ),

        (HttpMethod::Post, "/connect") => {
          if let Some(creds) = parse_form_data(request.body) {
            defmt::info!("Received credentials - SSID: \"{}\"", creds.ssid.as_str());

            let len = format_http_response(
              &mut response_buf,
              200,
              "OK",
              "text/html; charset=utf-8",
              success_page(),
            );

            let _ = socket.write_all(&response_buf[..len]).await;
            let _ = socket.flush().await;
            Timer::after(Duration::from_millis(500)).await;
            socket.close();

            return creds;
          } else {
            format_redirect_response(&mut response_buf, "/")
          }
        }

        _ => format_redirect_response(&mut response_buf, "/"),
      }
    } else {
      format_http_response(
        &mut response_buf,
        400,
        "Bad Request",
        "text/plain",
        "Bad Request",
      )
    };

    let _ = socket.write_all(&response_buf[..response_len]).await;
    let _ = socket.flush().await;
    socket.close();
  }
}

/// Embassy network task - runs the network stack.
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface<'static>>) {
  runner.run().await;
}

/// Attempt WiFi connection with configurable retries.
async fn connect_with_retries(
  controller: &mut WifiController<'_>,
  max_retries: u8,
  retry_delay_secs: u8,
) -> bool {
  for attempt in 1..=max_retries {
    defmt::info!("Connection attempt {}/{}...", attempt, max_retries);

    match controller.connect_async().await {
      Ok(_) => {
        defmt::info!("Connected successfully!");
        return true;
      }
      Err(e) => {
        defmt::info!("Attempt {} failed: {:?}", attempt, e);
        if attempt < max_retries {
          Timer::after(Duration::from_secs(retry_delay_secs as u64)).await;
        }
      }
    }
  }

  false
}

/// Wait for DHCP to assign an IP address.
async fn wait_for_dhcp_ip(stack: embassy_net::Stack<'_>) {
  loop {
    if let Some(config) = stack.config_v4() {
      let addr = config.address.address().octets();
      defmt::info!("Got IP: {}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3]);
      if let Some(gw) = config.gateway {
        let gw_octets = gw.octets();
        defmt::info!(
          "Gateway: {}.{}.{}.{}",
          gw_octets[0],
          gw_octets[1],
          gw_octets[2],
          gw_octets[3]
        );
      }
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }
}
