//! Mirror of the RT+ (RadioText Plus) bit-field parser from
//! `src/si4703/mod.rs::process_rt_plus`. See `lib.rs` § "Sync discipline".
//!
//! RT+ is layered on top of RDS RT: the broadcaster transmits the song
//! info as plain RadioText (e.g. `"Now: Daft Punk - Get Lucky"`) and
//! also publishes a small structured packet that points into the RT
//! buffer with `(content_type, start, length)` triples. This module
//! mirrors *only* the bit-extraction step \u2014 the part that's most prone
//! to silent corruption from a stray shift or mask \u2014 so it can run
//! under host `cargo test` without dragging in `embedded-hal`.

/// Parsed RT+ payload group: two (content_type, start, len) tags plus
/// the toggle / running flags from block B.
///
/// `len` is the *raw* wire field; the actual byte count of the tag is
/// `len + 1` (RT+ encoding convention to make a 1-byte item representable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtPlusPayload {
  pub item_toggle: bool,
  pub item_running: bool,
  pub tag1: RtPlusTag,
  pub tag2: RtPlusTag,
}

/// One (content_type, start, len) triple as transmitted on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtPlusTag {
  pub content_type: u8,
  pub start: u8,
  pub len: u8,
}

/// Decode an RT+ payload group from RDS blocks B / C / D.
///
/// Wire format (RDS Forum R08/008):
/// ```text
///   block B[4]    = item toggle
///   block B[3]    = item running
///   block B[2:0]  = content_type_1 [5:3]
///   block C[15:13]= content_type_1 [2:0]
///   block C[12:7] = start_1   (6 bits)
///   block C[6:1]  = length_1  (6 bits, byte count = value + 1)
///   block C[0]    = content_type_2 [5]
///   block D[15:11]= content_type_2 [4:0]
///   block D[10:5] = start_2   (6 bits)
///   block D[4:0]  = length_2  (5 bits, byte count = value + 1)
/// ```
///
/// Note the asymmetry: `length_1` is 6 bits but `length_2` is only 5,
/// since two 6-bit content type codes plus two 6-bit starts plus one
/// 6-bit length already consume 30 bits of the 32-bit C+D payload.
#[must_use]
pub fn decode_rt_plus(block_b: u16, block_c: u16, block_d: u16) -> RtPlusPayload {
  let item_toggle = (block_b & 0x0010) != 0;
  let item_running = (block_b & 0x0008) != 0;

  let ct1 = (((block_b & 0x0007) << 3) | ((block_c >> 13) & 0x0007)) as u8;
  let start1 = ((block_c >> 7) & 0x003F) as u8;
  let len1 = ((block_c >> 1) & 0x003F) as u8;

  let ct2 = (((block_c & 0x0001) << 5) | ((block_d >> 11) & 0x001F)) as u8;
  let start2 = ((block_d >> 5) & 0x003F) as u8;
  let len2 = (block_d & 0x001F) as u8;

  RtPlusPayload {
    item_toggle,
    item_running,
    tag1: RtPlusTag {
      content_type: ct1,
      start: start1,
      len: len1,
    },
    tag2: RtPlusTag {
      content_type: ct2,
      start: start2,
      len: len2,
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Build a synthetic RT+ wire frame from logical fields, exercising
  /// the *encoder* side of the same bit layout. Lets us round-trip
  /// concrete content_types/starts/lengths through `decode_rt_plus`
  /// without hand-crafting hex blobs.
  fn encode_rt_plus(
    item_toggle: bool,
    item_running: bool,
    ct1: u8,
    start1: u8,
    len1: u8,
    ct2: u8,
    start2: u8,
    len2: u8,
  ) -> (u16, u16, u16) {
    assert!(ct1 < 64);
    assert!(start1 < 64);
    assert!(len1 < 64);
    assert!(ct2 < 64);
    assert!(start2 < 64);
    assert!(len2 < 32);

    // Block B — only the low 5 bits matter for RT+; the upper bits are
    // group type / version / PTY etc., which we don't model here. Tests
    // pass `0x0000` for those upper bits.
    let mut b = 0u16;
    if item_toggle {
      b |= 0x0010;
    }
    if item_running {
      b |= 0x0008;
    }
    b |= u16::from(ct1 >> 3) & 0x0007;

    // Block C: ct1[2:0] (15..13), start1 (12..7), len1 (6..1), ct2[5] (0)
    let mut c = 0u16;
    c |= (u16::from(ct1) & 0x0007) << 13;
    c |= (u16::from(start1) & 0x003F) << 7;
    c |= (u16::from(len1) & 0x003F) << 1;
    c |= u16::from(ct2 >> 5) & 0x0001;

    // Block D: ct2[4:0] (15..11), start2 (10..5), len2 (4..0)
    let mut d = 0u16;
    d |= (u16::from(ct2) & 0x001F) << 11;
    d |= (u16::from(start2) & 0x003F) << 5;
    d |= u16::from(len2) & 0x001F;

    (b, c, d)
  }

  #[test]
  fn round_trip_typical_song() {
    // Daft Punk \u2014 Get Lucky in an RT like:
    //   "Daft Punk - Get Lucky        "
    //    0123456789...
    // tag1 = ITEM.ARTIST (4) start=0  len=8 (\u2192 9 bytes "Daft Punk")
    // tag2 = ITEM.TITLE  (1) start=12 len=8 (\u2192 9 bytes "Get Lucky")
    let (b, c, d) = encode_rt_plus(true, true, 4, 0, 8, 1, 12, 8);
    let p = decode_rt_plus(b, c, d);
    assert!(p.item_toggle);
    assert!(p.item_running);
    assert_eq!(p.tag1, RtPlusTag { content_type: 4, start: 0, len: 8 });
    assert_eq!(p.tag2, RtPlusTag { content_type: 1, start: 12, len: 8 });
  }

  #[test]
  fn item_running_flag_independent_of_toggle() {
    // running=false, toggle=true \u2192 between songs, but toggle still hot.
    let (b, c, d) = encode_rt_plus(true, false, 1, 0, 0, 4, 0, 0);
    let p = decode_rt_plus(b, c, d);
    assert!(p.item_toggle);
    assert!(!p.item_running);
  }

  #[test]
  fn content_type_spans_block_b_and_c() {
    // ct1 = 0b110011 = 51 \u2014 needs the low-3-bits-of-block-B path *and*
    // the high-3-bits-of-block-C path to combine correctly.
    let (b, c, d) = encode_rt_plus(false, true, 51, 0, 0, 0, 0, 0);
    let p = decode_rt_plus(b, c, d);
    assert_eq!(p.tag1.content_type, 51);
  }

  #[test]
  fn content_type_two_spans_block_c_and_d() {
    // ct2 = 0b100001 = 33 \u2014 needs the low-1-bit-of-block-C path *and*
    // the high-5-bits-of-block-D path.
    let (b, c, d) = encode_rt_plus(false, true, 0, 0, 0, 33, 0, 0);
    let p = decode_rt_plus(b, c, d);
    assert_eq!(p.tag2.content_type, 33);
  }

  #[test]
  fn max_length_fields_round_trip() {
    // tag1 carries a 6-bit length (max 63), tag2 carries a 5-bit one
    // (max 31). The encoder asserts these ranges, so we just need to
    // confirm the decoder reconstructs them losslessly.
    let (b, c, d) = encode_rt_plus(true, true, 1, 63, 63, 4, 63, 31);
    let p = decode_rt_plus(b, c, d);
    assert_eq!(p.tag1, RtPlusTag { content_type: 1, start: 63, len: 63 });
    assert_eq!(p.tag2, RtPlusTag { content_type: 4, start: 63, len: 31 });
  }

  #[test]
  fn zeros_decode_to_zero_payload() {
    let p = decode_rt_plus(0, 0, 0);
    assert!(!p.item_toggle);
    assert!(!p.item_running);
    assert_eq!(p.tag1, RtPlusTag { content_type: 0, start: 0, len: 0 });
    assert_eq!(p.tag2, RtPlusTag { content_type: 0, start: 0, len: 0 });
  }

  #[test]
  fn no_field_bleeds_into_neighbour() {
    // Set only tag2 fields and confirm tag1 stays clean. This catches
    // mask-off-by-one bugs where, e.g. tag2.content_type leaks into
    // tag1.len.
    let (b, c, d) = encode_rt_plus(false, true, 0, 0, 0, 31, 31, 31);
    let p = decode_rt_plus(b, c, d);
    assert_eq!(p.tag1, RtPlusTag { content_type: 0, start: 0, len: 0 });
    assert_eq!(p.tag2, RtPlusTag { content_type: 31, start: 31, len: 31 });
  }

  /// Property-style sanity: decoding an encoded payload always yields
  /// the exact same logical fields, for a small grid of inputs.
  #[test]
  fn exhaustive_round_trip_sample() {
    for &ct1 in &[0u8, 1, 4, 31, 63] {
      for &start1 in &[0u8, 7, 32, 63] {
        for &len1 in &[0u8, 1, 32, 63] {
          for &ct2 in &[0u8, 1, 4, 32, 63] {
            for &start2 in &[0u8, 5, 50] {
              for &len2 in &[0u8, 1, 31] {
                let (b, c, d) = encode_rt_plus(true, true, ct1, start1, len1, ct2, start2, len2);
                let p = decode_rt_plus(b, c, d);
                assert_eq!(p.tag1.content_type, ct1);
                assert_eq!(p.tag1.start, start1);
                assert_eq!(p.tag1.len, len1);
                assert_eq!(p.tag2.content_type, ct2);
                assert_eq!(p.tag2.start, start2);
                assert_eq!(p.tag2.len, len2);
              }
            }
          }
        }
      }
    }
  }
}
