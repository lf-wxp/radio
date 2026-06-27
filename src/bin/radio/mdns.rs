//! Minimal mDNS responder for `esp-radio.local` hostname resolution.
//!
//! Lets the user reach the LAN web console (see [`crate::web`]) via
//! `http://esp-radio.local/` instead of memorising a DHCP-assigned IP.
//!
//! > **Sync note**: pure-logic functions (`parse_query`, `build_response`,
//! > `eq_ascii_case_insensitive`) are mirrored in
//! > `tools/host-tests/src/mdns_parser.rs` so `cargo make host-test`
//! > can exercise them. Mirror any change there too.
//!
//! ## Scope (intentionally small)
//!
//! This is a passive, single-name responder \u2014 not a full DNS-SD stack:
//!
//! - Listens on `224.0.0.251:5353` (the standard mDNS multicast group).
//! - Replies to queries for `esp-radio.local` of type `A` (IPv4) or
//!   `ANY`. Everything else is silently dropped.
//! - Does **not** advertise services (no `PTR` / `SRV` / `TXT`).
//! - Does **not** announce on startup or on IP change \u2014 a passive
//!   responder is enough for browser hostname resolution because the
//!   client always queries before connecting.
//! - Does **not** implement DNS pointer-compression decoding because
//!   real-world mDNS queries are short and never compressed.
//!
//! ## Design notes
//!
//! - Buffers live on the task stack, not the heap (`mem-with-capacity`
//!   doesn't apply to `no_std`; we use fixed `[u8; N]`).
//! - The IP address is fetched fresh from the embassy-net stack on
//!   every query rather than cached, so a DHCP renewal that hands us
//!   a new lease just works on the next query without bookkeeping.
//! - All parse / build paths return `Option` instead of `Result` \u2014
//!   we don't differentiate failure reasons because the only sensible
//!   action on any malformed packet is to drop it.

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Stack};

// ============================================================================
// Constants
// ============================================================================

/// Single label of the hostname we answer for, ASCII bytes.
///
/// Stored as a single label (no dots) because DNS encodes each label
/// independently. The full name is `<HOSTNAME_LABEL>.<TLD_LABEL>`.
const HOSTNAME_LABEL: &[u8] = b"esp-radio";

/// Top-level domain label, always `local` for mDNS.
const TLD_LABEL: &[u8] = b"local";

/// IANA-assigned mDNS multicast address (RFC 6762 \u00a7 3).
const MDNS_GROUP: Ipv4Address = Ipv4Address::new(224, 0, 0, 251);

/// IANA-assigned mDNS UDP port (5353).
const MDNS_PORT: u16 = 5353;

/// Cache TTL handed back to the resolver. 120 s matches the value
/// recommended by RFC 6762 \u00a7 10 for unique records.
const ANSWER_TTL_SECS: u32 = 120;

/// DNS record type \u2014 IPv4 host address.
const QTYPE_A: u16 = 1;

/// DNS record type \u2014 wildcard.
const QTYPE_ANY: u16 = 255;

/// DNS class \u2014 Internet.
const QCLASS_IN: u16 = 1;

/// mDNS cache-flush bit (ORed into class on outgoing answers).
const CACHE_FLUSH_BIT: u16 = 0x8000;

/// Receive buffer size. mDNS queries from common implementations are
/// well under 256 bytes; 512 leaves head-room for stacked questions.
const RX_BUFFER_SIZE: usize = 512;

/// Transmit buffer size. Our response is fixed-shape (header + one A
/// record) and never exceeds ~50 bytes, so 128 is generous.
const TX_BUFFER_SIZE: usize = 128;

// ============================================================================
// Parsing
// ============================================================================

/// Decoded relevant fields of an incoming mDNS query.
///
/// We only care whether *we* are the target, so the parser collapses
/// to a single boolean outcome.
#[derive(Clone, Copy, Debug)]
struct ParsedQuery {
  /// True when the first question is for `esp-radio.local` of type
  /// `A` or `ANY`, class `IN` (with or without the unicast-response
  /// bit, which we ignore).
  is_for_us: bool,
}

/// Parse an inbound DNS packet, returning [`ParsedQuery`] when the
/// header is well-formed.
///
/// Returns `None` for any kind of malformation (short packet, response
/// rather than query, pointer compression in the QNAME, etc.) \u2014 the
/// caller will silently drop those.
fn parse_query(packet: &[u8]) -> Option<ParsedQuery> {
  // ---- Header ------------------------------------------------------------
  // [0..2]  ID         \u2014 mDNS uses 0
  // [2..4]  Flags      \u2014 bit 15 (QR) must be 0 for a query
  // [4..6]  QDCOUNT    \u2014 number of questions, must be \u2265 1
  // [6..]   ANCOUNT, NSCOUNT, ARCOUNT (unused on parse)
  if packet.len() < 12 {
    return None;
  }
  let flags = u16::from_be_bytes([packet[2], packet[3]]);
  if flags & 0x8000 != 0 {
    return None; // response, not a query
  }
  let qd_count = u16::from_be_bytes([packet[4], packet[5]]);
  if qd_count == 0 {
    return None;
  }

  // ---- First question ---------------------------------------------------
  let mut pos = 12usize;
  let mut label_index = 0u8;
  let mut hostname_ok = false;
  let mut tld_ok = false;

  loop {
    let len_byte = *packet.get(pos)?;
    pos += 1;
    if len_byte == 0 {
      break;
    }
    // Pointer compression (top two bits set) is technically legal in
    // queries but never observed in practice; refusing to decode it
    // keeps the parser tiny.
    if len_byte & 0xc0 != 0 {
      return None;
    }
    let label_end = pos.checked_add(len_byte as usize)?;
    if label_end > packet.len() {
      return None;
    }
    let label = &packet[pos..label_end];
    pos = label_end;

    match label_index {
      0 => hostname_ok = eq_ascii_case_insensitive(label, HOSTNAME_LABEL),
      1 => tld_ok = eq_ascii_case_insensitive(label, TLD_LABEL),
      // A third or deeper label means it's not our hostname; bail
      // (don't return None \u2014 still need to advance past QTYPE/QCLASS,
      // but we don't actually need them since is_for_us is already
      // false). Simpler: just stop matching and let the loop finish.
      _ => {
        hostname_ok = false;
        tld_ok = false;
      }
    }
    label_index = label_index.saturating_add(1);
  }

  // QTYPE + QCLASS occupy the next 4 bytes after the terminating 0.
  let qtype_end = pos.checked_add(2)?;
  if qtype_end > packet.len() {
    return None;
  }
  let qtype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
  // Class is read but only its low 15 bits matter; the high bit is
  // the unicast-response request which we ignore (we always reply to
  // the multicast group, which is RFC-legal and simpler).
  let qclass_end = qtype_end.checked_add(2)?;
  if qclass_end > packet.len() {
    return None;
  }
  let qclass = u16::from_be_bytes([packet[qtype_end], packet[qtype_end + 1]]) & 0x7fff;

  let type_ok = qtype == QTYPE_A || qtype == QTYPE_ANY;
  let class_ok = qclass == QCLASS_IN;
  let two_labels = label_index == 2;

  Some(ParsedQuery {
    is_for_us: hostname_ok && tld_ok && two_labels && type_ok && class_ok,
  })
}

/// ASCII case-insensitive equality (DNS names are defined case-insensitive
/// per RFC 1035 § 2.3.3).
fn eq_ascii_case_insensitive(a: &[u8], b: &[u8]) -> bool {
  a.eq_ignore_ascii_case(b)
}
// ============================================================================
// Response construction
// ============================================================================

/// Write a response packet containing a single A record for our
/// hostname into `out`, returning the byte length written.
///
/// Returns `None` if the output buffer is too small. Callers always
/// pass `TX_BUFFER_SIZE` so this only fires under programmer error.
#[allow(
  clippy::large_stack_frames,
  reason = "clippy mis-counts the cumulative size of the many `pos += k` \
            steps; actual frame is well under 100 bytes (just `pos: usize` \
            plus the slice fat pointer arguments). Splitting into helpers \
            would only obscure the byte-by-byte packet layout that mirrors \
            RFC 1035."
)]
fn build_response(out: &mut [u8], ip: [u8; 4]) -> Option<usize> {
  // 12 (header) + 1 + 9 + 1 + 5 + 1 (qname) + 2 + 2 + 4 + 2 + 4 (rdata)
  // = 43 bytes; sanity-check the buffer.
  let needed = 12 + 1 + HOSTNAME_LABEL.len() + 1 + TLD_LABEL.len() + 1 + 2 + 2 + 4 + 2 + 4;
  if out.len() < needed {
    return None;
  }

  let mut pos = 0usize;

  // ---- Header -----------------------------------------------------------
  // ID = 0; mDNS responses use 0 unless replying to a unicast-request.
  out[pos..pos + 2].copy_from_slice(&[0x00, 0x00]);
  pos += 2;
  // Flags = 0x8400  (QR=1 response, AA=1 authoritative, opcode=0,
  //                  TC=0, RD=0, RA=0, Z=0, RCODE=0)
  out[pos..pos + 2].copy_from_slice(&[0x84, 0x00]);
  pos += 2;
  // QDCOUNT = 0 \u2014 echoing the question is optional for mDNS responses
  // and most resolvers actually prefer it omitted.
  out[pos..pos + 2].copy_from_slice(&[0x00, 0x00]);
  pos += 2;
  // ANCOUNT = 1
  out[pos..pos + 2].copy_from_slice(&[0x00, 0x01]);
  pos += 2;
  // NSCOUNT, ARCOUNT
  out[pos..pos + 4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
  pos += 4;

  // ---- Answer NAME ------------------------------------------------------
  out[pos] = HOSTNAME_LABEL.len() as u8;
  pos += 1;
  out[pos..pos + HOSTNAME_LABEL.len()].copy_from_slice(HOSTNAME_LABEL);
  pos += HOSTNAME_LABEL.len();
  out[pos] = TLD_LABEL.len() as u8;
  pos += 1;
  out[pos..pos + TLD_LABEL.len()].copy_from_slice(TLD_LABEL);
  pos += TLD_LABEL.len();
  out[pos] = 0; // null terminator
  pos += 1;

  // ---- Answer TYPE / CLASS / TTL / RDLENGTH / RDATA --------------------
  out[pos..pos + 2].copy_from_slice(&QTYPE_A.to_be_bytes());
  pos += 2;
  out[pos..pos + 2].copy_from_slice(&(QCLASS_IN | CACHE_FLUSH_BIT).to_be_bytes());
  pos += 2;
  out[pos..pos + 4].copy_from_slice(&ANSWER_TTL_SECS.to_be_bytes());
  pos += 4;
  out[pos..pos + 2].copy_from_slice(&4u16.to_be_bytes());
  pos += 2;
  out[pos..pos + 4].copy_from_slice(&ip);
  pos += 4;

  Some(pos)
}

// ============================================================================
// Embassy task
// ============================================================================

/// Fetch the device's current IPv4 address from the network stack.
///
/// Returns `None` while DHCP is still in progress; the caller drops
/// the query in that case (a fresh probe will arrive within a second
/// from any well-behaved resolver).
fn current_ipv4(stack: Stack<'_>) -> Option<[u8; 4]> {
  stack.config_v4().map(|c| c.address.address().octets())
}

/// Long-running mDNS responder.
///
/// Joins the IPv4 multicast group and serves replies for
/// `esp-radio.local` until the device powers off. Failures from
/// `bind` / `join_multicast_group` are logged but not fatal \u2014 the
/// task simply enters a quiet `recv_from` loop that will never
/// receive anything (the web console still works on raw IP).
#[embassy_executor::task]
#[allow(
  clippy::large_stack_frames,
  reason = "owns 512 B rx + 128 B tx data buffers + two 2-element \
            PacketMetadata arrays on its own stack so we never \
            allocate from the heap; well under the 16 KiB Embassy \
            task stack."
)]
pub async fn mdns_task(stack: Stack<'static>) -> ! {
  // Wait for the interface to come up before joining a multicast
  // group; smoltcp rejects the join when no IP is configured.
  stack.wait_config_up().await;

  if let Err(e) = stack.join_multicast_group(IpAddress::Ipv4(MDNS_GROUP)) {
    defmt::warn!(
      "mDNS: join_multicast_group failed: {:?}",
      defmt::Debug2Format(&e)
    );
  }

  let mut rx_meta = [PacketMetadata::EMPTY; 2];
  let mut tx_meta = [PacketMetadata::EMPTY; 2];
  let mut rx_buffer = [0u8; RX_BUFFER_SIZE];
  let mut tx_buffer = [0u8; TX_BUFFER_SIZE];
  let mut socket = UdpSocket::new(
    stack,
    &mut rx_meta,
    &mut rx_buffer,
    &mut tx_meta,
    &mut tx_buffer,
  );

  if let Err(e) = socket.bind(IpListenEndpoint {
    addr: None,
    port: MDNS_PORT,
  }) {
    defmt::warn!("mDNS: bind failed: {:?}", defmt::Debug2Format(&e));
    // Park the task forever; the web console still works via IP.
    loop {
      embassy_time::Timer::after(embassy_time::Duration::from_secs(60)).await;
    }
  }
  defmt::info!("mDNS: listening on 224.0.0.251:5353 as esp-radio.local");

  let mut req = [0u8; RX_BUFFER_SIZE];
  let mut resp = [0u8; TX_BUFFER_SIZE];
  let dest = IpEndpoint::new(IpAddress::Ipv4(MDNS_GROUP), MDNS_PORT);

  loop {
    let (n, _meta) = match socket.recv_from(&mut req).await {
      Ok(v) => v,
      Err(_) => continue,
    };
    let Some(parsed) = parse_query(&req[..n]) else {
      continue;
    };
    if !parsed.is_for_us {
      continue;
    }
    let Some(ip) = current_ipv4(stack) else {
      continue;
    };
    let Some(len) = build_response(&mut resp, ip) else {
      continue;
    };
    let _ = socket.send_to(&resp[..len], dest).await;
  }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;

  /// Build a minimal mDNS query packet for the given labels and qtype.
  fn make_query(labels: &[&[u8]], qtype: u16) -> alloc::vec::Vec<u8> {
    let mut buf = alloc::vec::Vec::new();
    // Header: id=0 flags=0 qd=1 an=ns=ar=0
    buf.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in labels {
      buf.push(label.len() as u8);
      buf.extend_from_slice(label);
    }
    buf.push(0);
    buf.extend_from_slice(&qtype.to_be_bytes());
    buf.extend_from_slice(&QCLASS_IN.to_be_bytes());
    buf
  }

  #[test]
  fn parses_exact_match() {
    let pkt = make_query(&[b"esp-radio", b"local"], QTYPE_A);
    let parsed = parse_query(&pkt).expect("parses");
    assert!(parsed.is_for_us);
  }

  #[test]
  fn parses_case_insensitive() {
    let pkt = make_query(&[b"ESP-Radio", b"LOCAL"], QTYPE_A);
    assert!(parse_query(&pkt).expect("parses").is_for_us);
  }

  #[test]
  fn accepts_qtype_any() {
    let pkt = make_query(&[b"esp-radio", b"local"], QTYPE_ANY);
    assert!(parse_query(&pkt).expect("parses").is_for_us);
  }

  #[test]
  fn rejects_other_hostname() {
    let pkt = make_query(&[b"other", b"local"], QTYPE_A);
    assert!(!parse_query(&pkt).expect("parses").is_for_us);
  }

  #[test]
  fn rejects_other_tld() {
    let pkt = make_query(&[b"esp-radio", b"home"], QTYPE_A);
    assert!(!parse_query(&pkt).expect("parses").is_for_us);
  }

  #[test]
  fn rejects_short_packet() {
    assert!(parse_query(&[0u8; 5]).is_none());
  }

  #[test]
  fn rejects_response_packet() {
    let mut pkt = make_query(&[b"esp-radio", b"local"], QTYPE_A);
    pkt[2] = 0x84; // set QR + AA
    assert!(parse_query(&pkt).is_none());
  }

  #[test]
  fn rejects_pointer_compression() {
    let mut pkt = make_query(&[b"esp-radio", b"local"], QTYPE_A);
    pkt[12] = 0xc0; // pointer marker
    assert!(parse_query(&pkt).is_none());
  }

  #[test]
  fn build_response_layout() {
    let mut out = [0u8; 64];
    let len = build_response(&mut out, [192, 168, 1, 42]).expect("fits");
    // Header
    assert_eq!(&out[..2], &[0, 0]);
    assert_eq!(&out[2..4], &[0x84, 0x00]); // QR + AA
    assert_eq!(&out[4..6], &[0, 0]); // qd
    assert_eq!(&out[6..8], &[0, 1]); // an
    // QNAME = "esp-radio" "local" 0
    assert_eq!(out[12], 9);
    assert_eq!(&out[13..22], b"esp-radio");
    assert_eq!(out[22], 5);
    assert_eq!(&out[23..28], b"local");
    assert_eq!(out[28], 0);
    // Type/Class
    assert_eq!(&out[29..31], &[0, 1]); // A
    assert_eq!(&out[31..33], &[0x80, 0x01]); // IN | cache-flush
    // RDATA
    assert_eq!(&out[len - 4..len], &[192, 168, 1, 42]);
  }
}
