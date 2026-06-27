//! Mirror of `src/bin/radio/clock/sntp.rs`.
//! See `lib.rs` § "Sync discipline".

/// Size of an SNTPv4 packet on the wire.
pub const PACKET_LEN: usize = 48;

/// Number of seconds between the NTP epoch (1900-01-01 UTC) and the
/// Unix epoch (1970-01-01 UTC).
pub const NTP_TO_UNIX_OFFSET: u64 = 2_208_988_800;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
  TooShort,
  AlarmCondition,
  WrongMode,
  KissOfDeath,
  InvalidStratum,
  EmptyTimestamp,
  PreUnixEpoch,
}

#[must_use]
pub fn encode_request() -> [u8; PACKET_LEN] {
  let mut buf = [0u8; PACKET_LEN];
  buf[0] = 0x23;
  buf
}

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
  u64::from(ntp_secs)
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
    p[40..44].copy_from_slice(&ntp_secs.to_be_bytes());
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
    let p = make_reply(0, 4, 2, 3_991_507_200);
    assert_eq!(decode_reply(&p), Ok(1_782_518_400));
  }

  #[test]
  fn rejects_short_packet() {
    assert_eq!(decode_reply(&[0u8; 32]), Err(DecodeError::TooShort));
  }

  #[test]
  fn rejects_alarm_condition() {
    let p = make_reply(3, 4, 2, 3_991_507_200);
    assert_eq!(decode_reply(&p), Err(DecodeError::AlarmCondition));
  }

  #[test]
  fn rejects_client_mode() {
    let p = make_reply(0, 3, 2, 3_991_507_200);
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
    let p = make_reply(0, 4, 2, 1);
    assert_eq!(decode_reply(&p), Err(DecodeError::PreUnixEpoch));
  }

  #[test]
  fn round_trip_known_unix_time() {
    // 2024-01-01T00:00:00 UTC = 1704067200 Unix
    let unix = 1_704_067_200u64;
    let ntp = unix + NTP_TO_UNIX_OFFSET;
    let p = make_reply(0, 4, 1, ntp as u32);
    assert_eq!(decode_reply(&p), Ok(unix));
  }
}
