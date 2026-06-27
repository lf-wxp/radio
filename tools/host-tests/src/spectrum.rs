//! Mirror of the spectrum bucket math from
//! [`crate::si4703::Si4703::sweep_rssi`] and
//! [`crate::ui::spectrum_cursor_for`] in the firmware.
//!
//! Both routines split the FM band into `n` evenly-spaced buckets
//! over `[bottom, top]` (in 0.1 MHz units) and the firmware relies on
//! the forward map (bucket index → centre frequency) and the inverse
//! map (frequency → bucket index) staying consistent. If they drift
//! the LCD cursor would land on a different bar than the spectrum
//! sample for the listener's tuned frequency, which is exactly the
//! kind of off-by-one we want a test to catch.

/// Forward map: bucket index `i` to the centre frequency it samples.
///
/// Mirrors the inner expression of `Si4703::sweep_rssi`:
/// ```text
///     mid = bottom + span * (2i + 1) / (2N)
/// ```
/// computed in `u32` to avoid pulling in softfloat on `no_std`.
#[must_use]
pub fn bucket_centre_x10(bottom_x10: u16, top_x10: u16, n: usize, i: usize) -> u16 {
  assert!(n > 0, "bucket count must be non-zero");
  assert!(i < n, "bucket index out of range");
  let span_x10 = u32::from(top_x10.saturating_sub(bottom_x10));
  let mid = u32::from(bottom_x10) + (span_x10 * (2 * i as u32 + 1)) / (2 * n as u32);
  mid.min(u32::from(top_x10)) as u16
}

/// Inverse map: frequency to the bucket containing it.
///
/// Mirrors `crate::ui::spectrum_cursor_for`. Frequencies below
/// `bottom_x10` clamp to bucket 0; frequencies at or above `top_x10`
/// clamp to bucket `n - 1`.
#[must_use]
pub fn freq_to_bucket(bottom_x10: u16, top_x10: u16, n: usize, freq_x10: u16) -> usize {
  assert!(n > 0, "bucket count must be non-zero");
  if freq_x10 < bottom_x10 {
    return 0;
  }
  let span_x10 = u32::from(top_x10.saturating_sub(bottom_x10));
  if span_x10 == 0 {
    return 0;
  }
  let offset = u32::from(freq_x10 - bottom_x10);
  let n_u32 = n as u32;
  ((offset * n_u32 / span_x10).min(n_u32 - 1)) as usize
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Default FM band, US/Europe plan, 52 buckets — exactly the
  /// configuration the firmware uses at boot.
  const FM_BOTTOM_X10: u16 = 875;
  const FM_TOP_X10: u16 = 1080;
  const FM_BUCKETS: usize = 52;

  #[test]
  fn bucket_centres_are_monotonic_and_in_band() {
    let mut prev = 0u16;
    for i in 0..FM_BUCKETS {
      let mid = bucket_centre_x10(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, i);
      assert!(
        mid >= FM_BOTTOM_X10 && mid <= FM_TOP_X10,
        "bucket {i} centre {mid} out of band",
      );
      assert!(mid > prev, "bucket {i} centre {mid} not strictly increasing from {prev}");
      prev = mid;
    }
  }

  #[test]
  fn first_bucket_centre_is_lower_half() {
    // First bucket spans [bottom, bottom + span/N), so its centre is
    // at bottom + span/(2N) — strictly above bottom but well below
    // the second bucket's centre.
    let mid0 = bucket_centre_x10(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, 0);
    let mid1 = bucket_centre_x10(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, 1);
    assert!(mid0 > FM_BOTTOM_X10);
    assert!(mid0 < mid1);
  }

  #[test]
  fn last_bucket_centre_is_below_top() {
    // The `min(top)` clamp in the formula caps the last bucket at
    // top, but on the standard 875..=1080 plan with 52 buckets the
    // raw formula already lands inside the band.
    let last = bucket_centre_x10(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, FM_BUCKETS - 1);
    assert!(last <= FM_TOP_X10);
    assert!(last > FM_BOTTOM_X10);
  }

  #[test]
  fn freq_to_bucket_clamps_below_band() {
    // Anything < bottom must report bucket 0 so the LCD cursor stays
    // visible even before the first tune lands.
    assert_eq!(freq_to_bucket(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, 0), 0);
    assert_eq!(freq_to_bucket(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, 800), 0);
    assert_eq!(
      freq_to_bucket(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, FM_BOTTOM_X10 - 1),
      0,
    );
  }

  #[test]
  fn freq_to_bucket_clamps_above_band() {
    // At or above the top we always pin to the last bucket. The
    // `min(N-1)` clamp in the implementation guards against the
    // exact-top case where `(span * N / span) == N`.
    assert_eq!(
      freq_to_bucket(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, FM_TOP_X10),
      FM_BUCKETS - 1,
    );
    assert_eq!(
      freq_to_bucket(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, 1200),
      FM_BUCKETS - 1,
    );
  }

  #[test]
  fn bucket_centre_round_trips_to_same_bucket() {
    // The strongest invariant: for every bucket `i`, looking up the
    // bucket containing `bucket_centre_x10(i)` must return `i`. This
    // is what keeps the LCD cursor aligned with the bar the user just
    // tuned to.
    for i in 0..FM_BUCKETS {
      let mid = bucket_centre_x10(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, i);
      let inv = freq_to_bucket(FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS, mid);
      assert_eq!(inv, i, "round-trip mismatch: bucket {i} centre {mid} mapped to {inv}");
    }
  }

  #[test]
  fn common_stations_land_in_expected_buckets() {
    // Spot-check well-known US frequencies to guard against off-by-one
    // when the formula is touched. These were computed by hand from
    // bottom + span * (2i + 1) / (2N) on the 52-bucket plan and so
    // are independent of the implementation under test.
    //
    // Span = 1080 - 875 = 205, N = 52, so bucket width = 205/52 ≈
    // 3.94 (in 0.1 MHz units, i.e. 0.394 MHz).
    let band = (FM_BOTTOM_X10, FM_TOP_X10, FM_BUCKETS);
    // 88.1 MHz → (881 - 875) * 52 / 205 = 312 / 205 ≈ 1
    assert_eq!(freq_to_bucket(band.0, band.1, band.2, 881), 1);
    // 101.5 MHz → (1015 - 875) * 52 / 205 = 7280 / 205 ≈ 35
    assert_eq!(freq_to_bucket(band.0, band.1, band.2, 1015), 35);
    // 107.9 MHz → (1079 - 875) * 52 / 205 = 10608 / 205 ≈ 51
    assert_eq!(freq_to_bucket(band.0, band.1, band.2, 1079), 51);
  }

  #[test]
  fn zero_span_does_not_panic() {
    // Defensive: if a future band-plan ever has bottom == top the
    // routine must still return a valid bucket index without dividing
    // by zero. (The firmware can't currently reach this state but the
    // host test costs nothing to keep honest.)
    assert_eq!(freq_to_bucket(1000, 1000, 4, 1000), 0);
  }
}
