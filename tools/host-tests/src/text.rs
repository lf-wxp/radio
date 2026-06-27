//! Mirror of `clip_to_buf` from `src/bin/radio/listening_log.rs`.
//! See `lib.rs` § "Sync discipline".

/// Return the longest prefix of `src` that fits in `cap` bytes
/// without breaking a UTF-8 char boundary.
#[must_use]
pub fn clip_to_buf(src: &str, cap: usize) -> &[u8] {
  if src.len() <= cap {
    return src.as_bytes();
  }
  let mut end = 0usize;
  for (i, _) in src.char_indices() {
    if i > cap {
      break;
    }
    end = i;
  }
  if let Some(c) = src[end..].chars().next()
    && end + c.len_utf8() <= cap
  {
    end += c.len_utf8();
  }
  &src.as_bytes()[..end]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn passthrough_when_short() {
    let out = clip_to_buf("BBC", 8);
    assert_eq!(out, b"BBC");
  }

  #[test]
  fn truncates_at_char_boundary() {
    // 'é' is two bytes in UTF-8, so cap=3 should keep "aé" (3 bytes)
    // but not the trailing 'b' (which would push past 3).
    let out = clip_to_buf("aéb", 3);
    assert_eq!(out, "aé".as_bytes());
  }

  #[test]
  fn drops_partial_multibyte() {
    // cap=2 is in the middle of 'é'; we must back off to keep
    // valid UTF-8.
    let out = clip_to_buf("aéb", 2);
    assert_eq!(out, b"a");
  }

  #[test]
  fn empty_input() {
    assert_eq!(clip_to_buf("", 4), b"");
  }

  #[test]
  fn cap_zero() {
    assert_eq!(clip_to_buf("abc", 0), b"");
  }

  #[test]
  fn ascii_exact_fit() {
    assert_eq!(clip_to_buf("HELLO", 5), b"HELLO");
  }

  #[test]
  fn output_is_always_valid_utf8() {
    // Property: regardless of cap, the returned bytes must round-trip
    // through `core::str::from_utf8` cleanly.
    for cap in 0..10 {
      let bytes = clip_to_buf("aéb日c", cap);
      core::str::from_utf8(bytes).expect("clip_to_buf must produce valid UTF-8");
    }
  }
}
