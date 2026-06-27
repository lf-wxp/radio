//! Mirror of the URL parser from `src/bin/radio/ota/http_download.rs`.
//! See `lib.rs` § "Sync discipline".
//!
//! The firmware's parser returns an `Ipv4Address` from `embassy-net`;
//! we use a plain `[u8; 4]` here because the host has no reason to
//! depend on a no_std net stack.

use core::str::FromStr;

/// Reasons URL parsing can fail. Mirrors the relevant variant of
/// `HttpError::BadUrl` in the firmware.
#[derive(Debug, PartialEq, Eq)]
pub enum UrlError {
  BadUrl,
}

/// Borrowed view of a parsed `http://` URL with an IPv4-literal host.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedUrl<'a> {
  pub ip: [u8; 4],
  pub port: u16,
  pub path: &'a str,
}

/// Parse `http://<ipv4>[:port]/path` into its component parts.
pub fn parse_url(url: &str) -> Result<ParsedUrl<'_>, UrlError> {
  let rest = url.strip_prefix("http://").ok_or(UrlError::BadUrl)?;

  let (authority, path) = match rest.find('/') {
    Some(idx) => (&rest[..idx], &rest[idx..]),
    None => (rest, "/"),
  };
  if authority.is_empty() {
    return Err(UrlError::BadUrl);
  }

  let (host, port) = match authority.find(':') {
    Some(idx) => {
      let port_str = &authority[idx + 1..];
      let port = u16::from_str(port_str).map_err(|_| UrlError::BadUrl)?;
      (&authority[..idx], port)
    }
    None => (authority, 80),
  };

  let ip = parse_ipv4(host).ok_or(UrlError::BadUrl)?;
  Ok(ParsedUrl { ip, port, path })
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
  let mut octets = [0u8; 4];
  let mut count = 0;
  for part in s.split('.') {
    if count == 4 {
      return None;
    }
    octets[count] = u8::from_str(part).ok()?;
    count += 1;
  }
  if count != 4 {
    return None;
  }
  Some(octets)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_typical_url() {
    let p = parse_url("http://192.168.1.10:8000/firmware.bin").unwrap();
    assert_eq!(p.ip, [192, 168, 1, 10]);
    assert_eq!(p.port, 8000);
    assert_eq!(p.path, "/firmware.bin");
  }

  #[test]
  fn defaults_port_to_80() {
    let p = parse_url("http://10.0.0.1/img").unwrap();
    assert_eq!(p.port, 80);
    assert_eq!(p.path, "/img");
  }

  #[test]
  fn empty_path_becomes_slash() {
    let p = parse_url("http://10.0.0.1").unwrap();
    assert_eq!(p.path, "/");
  }

  #[test]
  fn rejects_https() {
    assert_eq!(parse_url("https://10.0.0.1/x"), Err(UrlError::BadUrl));
  }

  #[test]
  fn rejects_dns_hostname() {
    assert_eq!(parse_url("http://example.com/x"), Err(UrlError::BadUrl));
  }

  #[test]
  fn rejects_empty_authority() {
    assert_eq!(parse_url("http:///x"), Err(UrlError::BadUrl));
  }

  #[test]
  fn rejects_bad_port() {
    assert_eq!(
      parse_url("http://10.0.0.1:99999/x"),
      Err(UrlError::BadUrl)
    );
  }

  #[test]
  fn rejects_truncated_ipv4() {
    assert_eq!(parse_url("http://10.0.0/x"), Err(UrlError::BadUrl));
  }

  #[test]
  fn parses_query_string_in_path() {
    let p = parse_url("http://10.0.0.1/img?v=1&t=abc").unwrap();
    assert_eq!(p.path, "/img?v=1&t=abc");
  }
}
