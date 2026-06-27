//! Mirror of the mDNS parser & response builder from
//! `src/bin/radio/mdns.rs`. See `lib.rs` § "Sync discipline".

// ── Constants ────────────────────────────────────────────────────────────

/// Single label of the hostname we answer for, ASCII bytes.
const HOSTNAME_LABEL: &[u8] = b"esp-radio";

/// Top-level domain label, always `local` for mDNS.
const TLD_LABEL: &[u8] = b"local";

/// Cache TTL handed back to the resolver (RFC 6762 § 10).
const ANSWER_TTL_SECS: u32 = 120;

/// DNS record type — IPv4 host address.
const QTYPE_A: u16 = 1;

/// DNS record type — wildcard.
const QTYPE_ANY: u16 = 255;

/// DNS class — Internet.
const QCLASS_IN: u16 = 1;

/// mDNS cache-flush bit (ORed into class on outgoing answers).
const CACHE_FLUSH_BIT: u16 = 0x8000;

// ── Parsing ──────────────────────────────────────────────────────────────

/// Decoded relevant fields of an incoming mDNS query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedQuery {
  /// True when the first question is for `esp-radio.local` of type
  /// `A` or `ANY`, class `IN`.
  pub is_for_us: bool,
}

/// Parse an inbound DNS packet, returning [`ParsedQuery`] when the
/// header is well-formed.
#[must_use]
pub fn parse_query(packet: &[u8]) -> Option<ParsedQuery> {
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
    if len_byte & 0xc0 != 0 {
      return None; // pointer compression: refused
    }
    let label_end = pos.checked_add(len_byte as usize)?;
    if label_end > packet.len() {
      return None;
    }
    let label = &packet[pos..label_end];
    pos = label_end;

    match label_index {
      0 => hostname_ok = label.eq_ignore_ascii_case(HOSTNAME_LABEL),
      1 => tld_ok = label.eq_ignore_ascii_case(TLD_LABEL),
      _ => {
        hostname_ok = false;
        tld_ok = false;
      }
    }
    label_index = label_index.saturating_add(1);
  }

  let qtype_end = pos.checked_add(2)?;
  if qtype_end > packet.len() {
    return None;
  }
  let qtype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
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

// ── Response construction ────────────────────────────────────────────────

/// Write a response packet containing a single A record for our
/// hostname into `out`, returning the byte length written.
#[must_use]
pub fn build_response(out: &mut [u8], ip: [u8; 4]) -> Option<usize> {
  let needed = 12 + 1 + HOSTNAME_LABEL.len() + 1 + TLD_LABEL.len() + 1 + 2 + 2 + 4 + 2 + 4;
  if out.len() < needed {
    return None;
  }

  let mut pos = 0usize;

  // Header
  out[pos..pos + 2].copy_from_slice(&[0x00, 0x00]); // ID
  pos += 2;
  out[pos..pos + 2].copy_from_slice(&[0x84, 0x00]); // flags QR + AA
  pos += 2;
  out[pos..pos + 2].copy_from_slice(&[0x00, 0x00]); // QDCOUNT
  pos += 2;
  out[pos..pos + 2].copy_from_slice(&[0x00, 0x01]); // ANCOUNT
  pos += 2;
  out[pos..pos + 4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]); // NS+AR
  pos += 4;

  // QNAME
  out[pos] = u8::try_from(HOSTNAME_LABEL.len()).expect("HOSTNAME_LABEL fits in u8");
  pos += 1;
  out[pos..pos + HOSTNAME_LABEL.len()].copy_from_slice(HOSTNAME_LABEL);
  pos += HOSTNAME_LABEL.len();
  out[pos] = u8::try_from(TLD_LABEL.len()).expect("TLD_LABEL fits in u8");
  pos += 1;
  out[pos..pos + TLD_LABEL.len()].copy_from_slice(TLD_LABEL);
  pos += TLD_LABEL.len();
  out[pos] = 0;
  pos += 1;

  // TYPE / CLASS / TTL / RDLENGTH / RDATA
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

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  fn make_query(labels: &[&[u8]], qtype: u16) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in labels {
      buf.push(u8::try_from(label.len()).expect("label fits in u8"));
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
    pkt[2] = 0x84;
    assert!(parse_query(&pkt).is_none());
  }

  #[test]
  fn rejects_pointer_compression() {
    let mut pkt = make_query(&[b"esp-radio", b"local"], QTYPE_A);
    pkt[12] = 0xc0;
    assert!(parse_query(&pkt).is_none());
  }

  #[test]
  fn build_response_layout() {
    let mut out = [0u8; 64];
    let len = build_response(&mut out, [192, 168, 1, 42]).expect("fits");
    assert_eq!(&out[..2], &[0, 0]);
    assert_eq!(&out[2..4], &[0x84, 0x00]); // QR + AA
    assert_eq!(&out[4..6], &[0, 0]); // qd
    assert_eq!(&out[6..8], &[0, 1]); // an
    assert_eq!(out[12], 9);
    assert_eq!(&out[13..22], b"esp-radio");
    assert_eq!(out[22], 5);
    assert_eq!(&out[23..28], b"local");
    assert_eq!(out[28], 0);
    assert_eq!(&out[29..31], &[0, 1]); // type A
    assert_eq!(&out[31..33], &[0x80, 0x01]); // class IN | cache-flush
    assert_eq!(&out[len - 4..len], &[192, 168, 1, 42]);
  }

  #[test]
  fn build_response_rejects_undersized_buffer() {
    let mut tiny = [0u8; 8];
    assert!(build_response(&mut tiny, [1, 2, 3, 4]).is_none());
  }
}
