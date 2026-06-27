//! Pure-logic SNTPv4 packet encoder / decoder per RFC 4330.
//!
//! > **Sync note**: this module's pure functions and tests are
//! > mirrored in `tools/host-tests/src/sntp.rs` so `cargo make
//! > host-test` exercises them on the host. Mirror any change there.
//!
//! The radio talks SNTP, not full NTP — we only need a one-shot
//! timestamp for wall-clock display, not disciplined PLL phase
//! locking. The packet on the wire is the same 48 bytes either way;
//! we just don't bother with NTP4 client state machines.
//!
//! ## On-wire layout (all big-endian)
//!
//! ```text
//! byte 0  : LI(2) | VN(3) | Mode(3)        client request: 0x1B
//! byte 1  : Stratum                         response: 1..=15 valid
//! byte 2  : Poll                            ignored
//! byte 3  : Precision                       ignored
//! 4..8    : Root delay
//! 8..12   : Root dispersion
//! 12..16  : Reference identifier            response: KoD ASCII when stratum=0
//! 16..24  : Reference timestamp             ignored
//! 24..32  : Originate timestamp             ignored
//! 32..40  : Receive timestamp               ignored
//! 40..48  : Transmit timestamp              what we want; secs[..4]+frac[..4]
//! ```
//!
//! Timestamps are seconds since 1900-01-01 UTC. Unix epoch is
//! 1970-01-01 UTC, so we subtract [`NTP_TO_UNIX_OFFSET`] when
//! converting.

/// Size of an SNTPv4 packet on the wire.
pub const PACKET_LEN: usize = 48;

/// Number of seconds between the NTP epoch (1900-01-01 UTC) and the
/// Unix epoch (1970-01-01 UTC). 70 years × 365.2422 days × 86400 s,
/// rounded to the canonical RFC value.
pub const NTP_TO_UNIX_OFFSET: u64 = 2_208_988_800;

/// Default IANA NTP UDP port.
pub const DEFAULT_PORT: u16 = 123;

/// Reasons a server reply can be rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
  /// Packet was shorter than [`PACKET_LEN`].
  TooShort,
  /// LI field was 3 (alarm condition — server's clock is unsynced).
  AlarmCondition,
  /// Mode field was not 4 (server) or 5 (broadcast).
  WrongMode,
  /// Stratum 0 indicates a Kiss-o'-Death packet (rate-limit, deny).
  KissOfDeath,
  /// Stratum > 15 is reserved / invalid.
  InvalidStratum,
  /// Transmit timestamp seconds field was 0 — the server did not
  /// stamp the reply.
  EmptyTimestamp,
  /// Computed Unix time would underflow (server reported NTP
  /// seconds before 1970-01-01). Effectively "the clock is broken".
  PreUnixEpoch,
}

/// Build a 48-byte SNTPv4 client request.
///
/// LI=0, VN=4, Mode=3 → first byte = `0b00_100_011 = 0x23`.
/// All other fields are zero — that's enough for a basic request and
/// matches what `chrony`/`ntpd` send on the first poll.
#[must_use]
pub fn encode_request() -> [u8; PACKET_LEN] {
  let mut buf = [0u8; PACKET_LEN];
  buf[0] = 0x23;
  buf
}

/// Decode the transmit timestamp from a server reply, returning
/// Unix epoch seconds.
///
/// Validates LI / Mode / Stratum per RFC 4330 §5 before trusting
/// any field, so a malformed or hostile packet can't poison the
/// wall-clock.
///
/// # Errors
///
/// See [`DecodeError`] for the rejection reasons.
pub fn decode_reply(packet: &[u8]) -> Result<u64, DecodeError> {
  if packet.len() < PACKET_LEN {
    return Err(DecodeError::TooShort);
  }

  let li = (packet[0] >> 6) & 0b11;
  let mode = packet[0] & 0b111;
  let stratum = packet[1];

  if li == 3 {
    return Err(DecodeError::AlarmCondition);
  }
  if mode != 4 && mode != 5 {
    return Err(DecodeError::WrongMode);
  }
  match stratum {
    0 => return Err(DecodeError::KissOfDeath),
    1..=15 => {}
    _ => return Err(DecodeError::InvalidStratum),
  }

  let ntp_secs = u32::from_be_bytes([packet[40], packet[41], packet[42], packet[43]]);
  if ntp_secs == 0 {
    return Err(DecodeError::EmptyTimestamp);
  }

  let ntp_secs = u64::from(ntp_secs);
  ntp_secs
    .checked_sub(NTP_TO_UNIX_OFFSET)
    .ok_or(DecodeError::PreUnixEpoch)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_reply(li: u8, mode: u8, stratum: u8, ntp_secs: u32) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0] = ((li & 0b11) << 6) | (4 << 3) | (mode & 0b111);
    p[1] = stratum;
    let bytes = ntp_secs.to_be_bytes();
    p[40..44].copy_from_slice(&bytes);
    p
  }

  #[test]
  fn request_first_byte() {
    let req = encode_request();
    assert_eq!(req[0], 0x23);
    assert_eq!(req.len(), PACKET_LEN);
    assert!(req[1..].iter().all(|&b| b == 0));
  }

  #[test]
  fn decodes_valid_reply() {
    // 2026-06-27T00:00:00 UTC = 1782518400 Unix
    //                         = 1782518400 + 2208988800 = 3991507200 NTP
    let p = make_reply(0, 4, 2, 3_991_507_200);
    assert_eq!(decode_reply(&p), Ok(1_782_518_400));
  }

  #[test]
  fn rejects_short_packet() {
    let p = [0u8; 32];
    assert_eq!(decode_reply(&p), Err(DecodeError::TooShort));
  }

  #[test]
  fn rejects_alarm_condition() {
    let p = make_reply(3, 4, 2, 3_991_507_200);
    assert_eq!(decode_reply(&p), Err(DecodeError::AlarmCondition));
  }

  #[test]
  fn rejects_client_mode() {
    let p = make_reply(0, 3, 2, 3_991_507_200); // Mode=3 client, not server
    assert_eq!(decode_reply(&p), Err(DecodeError::WrongMode));
  }

  #[test]
  fn accepts_broadcast_mode() {
    let p = make_reply(0, 5, 2, 3_991_507_200);
    assert!(decode_reply(&p).is_ok());
  }

  #[test]
  fn rejects_kod() {
    let p = make_reply(0, 4, 0, 3_991_507_200);
    assert_eq!(decode_reply(&p), Err(DecodeError::KissOfDeath));
  }

  #[test]
  fn rejects_high_stratum() {
    let p = make_reply(0, 4, 16, 3_991_507_200);
    assert_eq!(decode_reply(&p), Err(DecodeError::InvalidStratum));
  }

  #[test]
  fn rejects_zero_timestamp() {
    let p = make_reply(0, 4, 2, 0);
    assert_eq!(decode_reply(&p), Err(DecodeError::EmptyTimestamp));
  }

  #[test]
  fn rejects_pre_unix() {
    // NTP timestamp 1 second after 1900 epoch — clearly bogus.
    let p = make_reply(0, 4, 2, 1);
    assert_eq!(decode_reply(&p), Err(DecodeError::PreUnixEpoch));
  }
}
