//! SNTP client task that wakes up after Wi-Fi association and
//! anchors [`crate::clock`] to UTC.
//!
//! ## Why not a crate?
//!
//! `sntpc`, `chrono-ntp`, and friends all expect a `std::net::UdpSocket`
//! or a custom socket trait that doesn't quite match
//! `embassy_net::udp::UdpSocket`. Wrapping that adapter would be more
//! code than the protocol itself — SNTPv4 is 48 bytes of structure
//! and one timestamp conversion. So we hand-roll the wire format in
//! [`crate::clock::sntp`] (fully host-tested) and limit this file to
//! the embassy-net plumbing.
//!
//! ## Server selection
//!
//! We hit Cloudflare's public anycast NTP service (`time.cloudflare.com`).
//! Two anycast IPs are tried in turn:
//!
//! - `162.159.200.1`
//! - `162.159.200.123`
//!
//! Anycast means the closest PoP answers, with sub-100 ms RTT from
//! most consumer ISPs. Hard-coding the IPs avoids needing a DNS
//! resolver pass before we have a clock — a chicken-and-egg trap on
//! first boot of devices behind captive portals.
//!
//! ## Sync schedule
//!
//! - Initial: try every 30 s until the first success.
//! - Steady: re-sync every 6 hours. Drift on a typical MCU crystal
//!   is ~20 ppm ≈ 0.4 s in 6 h, far below what users notice on a
//!   wall-clock.
//! - On failure mid-flight: keep the last good offset and retry on
//!   the steady-state schedule.

use embassy_net::Stack;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};
use embassy_time::{Duration, Instant, Timer, with_timeout};

use crate::clock::{self, sntp};

/// Local UDP port to bind for SNTP exchanges. `0` would let the
/// stack pick one, but smoltcp's UDP socket requires an explicit
/// port; we use the well-known NTP port on the client side too,
/// which is fine for a single in-flight request.
const LOCAL_PORT: u16 = 123;

/// Servers tried in turn on each sync attempt.
///
/// Both IPs belong to `time.cloudflare.com` (anycast).
const SERVERS: &[Ipv4Address] = &[
  Ipv4Address::new(162, 159, 200, 1),
  Ipv4Address::new(162, 159, 200, 123),
];

/// Per-attempt receive timeout. The RTT for the closest anycast PoP
/// is well under 200 ms; 3 s is generous and still keeps the task
/// responsive if a router quietly drops the packet.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// Wait between unsuccessful attempts during the initial bring-up
/// phase. Short enough that the user sees a synced timestamp within
/// a minute on a healthy network; long enough that we don't hammer
/// servers if the LAN is misconfigured.
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Wait between successful syncs. Six hours is the long-term
/// steady-state cadence (see module docs).
const SYNC_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// UDP buffer sizes — only ever holds one 48-byte SNTP packet.
const UDP_BUFFER_SIZE: usize = 64;

/// SNTP client task entry point.
///
/// Spawned from `main.rs` once the Wi-Fi stack is up. Owns its UDP
/// socket on the task's own stack (no heap), in line with the rest
/// of the firmware's allocation strategy.
#[embassy_executor::task]
#[allow(
  clippy::large_stack_frames,
  reason = "fixed UDP buffers (~256 B total) live on the task stack \
            instead of the heap; comfortably within Embassy's 16 KiB \
            task stack budget."
)]
pub async fn ntp_task(stack: Stack<'static>) -> ! {
  // Wait until DHCP / link-local has handed us an IP. Without this,
  // `socket.bind` would either fail outright or send packets that
  // the router silently drops.
  stack.wait_config_up().await;

  let mut rx_meta = [PacketMetadata::EMPTY; 2];
  let mut tx_meta = [PacketMetadata::EMPTY; 2];
  let mut rx_buffer = [0u8; UDP_BUFFER_SIZE];
  let mut tx_buffer = [0u8; UDP_BUFFER_SIZE];
  let mut socket = UdpSocket::new(
    stack,
    &mut rx_meta,
    &mut rx_buffer,
    &mut tx_meta,
    &mut tx_buffer,
  );

  if let Err(e) = socket.bind(IpListenEndpoint {
    addr: None,
    port: LOCAL_PORT,
  }) {
    defmt::warn!("NTP: bind failed: {:?}", defmt::Debug2Format(&e));
    // Park forever — wall-clock will simply stay unsynced.
    loop {
      Timer::after(Duration::from_secs(3600)).await;
    }
  }

  loop {
    let synced = try_sync_once(&mut socket).await;
    let wait = if synced {
      SYNC_INTERVAL
    } else {
      RETRY_INTERVAL
    };
    Timer::after(wait).await;
  }
}

/// Run one sync round: try each server in turn, accept the first
/// well-formed reply. Returns `true` on success.
async fn try_sync_once(socket: &mut UdpSocket<'_>) -> bool {
  let request = sntp::encode_request();
  let mut reply = [0u8; sntp::PACKET_LEN];

  for server in SERVERS {
    let dest = IpEndpoint::new(IpAddress::Ipv4(*server), sntp::DEFAULT_PORT);
    if let Err(e) = socket.send_to(&request, dest).await {
      defmt::debug!(
        "NTP: send_to {:?} failed: {:?}",
        defmt::Debug2Format(server),
        defmt::Debug2Format(&e)
      );
      continue;
    }

    match with_timeout(REPLY_TIMEOUT, socket.recv_from(&mut reply)).await {
      Ok(Ok((n, _meta))) => match sntp::decode_reply(&reply[..n]) {
        Ok(unix_secs) => {
          clock::record_sync(unix_secs, Instant::now());
          defmt::info!(
            "NTP: synced via {:?}, unix={}",
            defmt::Debug2Format(server),
            unix_secs
          );
          return true;
        }
        Err(reason) => {
          defmt::warn!("NTP: reply rejected ({:?})", defmt::Debug2Format(&reason));
        }
      },
      Ok(Err(e)) => {
        defmt::debug!("NTP: recv error: {:?}", defmt::Debug2Format(&e));
      }
      Err(_) => {
        defmt::debug!("NTP: timeout from {:?}", defmt::Debug2Format(server));
      }
    }
  }
  false
}
