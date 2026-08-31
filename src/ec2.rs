//! EC2 instance-metadata-service (IMDS) datasource.
//!
//! Talks plain HTTP/1.1 to 169.254.169.254 with a hand-rolled client so no
//! HTTP dependency is needed. IMDSv2 (session token) is tried first; if the
//! token request is answered but refused, falls back to IMDSv1.

use crate::datasource::Seed;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(2);

fn imds_addr() -> SocketAddr {
    SocketAddr::from(([169, 254, 169, 254], 80))
}

/// Try to fetch a seed from IMDS, retrying until `total_wait` has elapsed.
/// Returns None if the service never becomes reachable (i.e. not on EC2).
pub fn fetch(total_wait: Duration) -> Option<Seed> {
    let deadline = Instant::now() + total_wait;
    loop {
        if let Some(seed) = attempt() {
            return Some(seed);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn attempt() -> Option<Seed> {
    let token = match request(
        "PUT",
        "/latest/api/token",
        &[
            ("X-aws-ec2-metadata-token-ttl-seconds", "21600"),
            ("Content-Length", "0"),
        ],
    ) {
        Ok((200, body)) => Some(String::from_utf8_lossy(&body).trim().to_string()),
        // IMDS answered but refused the token request: fall back to IMDSv1.
        Ok(_) => None,
        // Nothing listening — not EC2 (or network not up yet).
        Err(_) => return None,
    };
    let token = token.as_deref();

    let instance_id = get("/latest/meta-data/instance-id", token)?;
    let instance_id = instance_id.trim().to_string();
    if instance_id.is_empty() {
        return None;
    }
    let mut meta_data = format!("instance-id: {instance_id}\n");
    if let Some(h) = get("/latest/meta-data/local-hostname", token) {
        let h = h.trim();
        if !h.is_empty() {
            meta_data.push_str(&format!("local-hostname: {h}\n"));
        }
    }
    let user_data = get("/latest/user-data", token);
    Some(Seed {
        source: format!(
            "ec2-imds{} (169.254.169.254)",
            if token.is_some() { "v2" } else { "v1" }
        ),
        meta_data,
        user_data,
    })
}

fn get(path: &str, token: Option<&str>) -> Option<String> {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some(t) = token {
        headers.push(("X-aws-ec2-metadata-token", t));
    }
    match request("GET", path, &headers) {
        Ok((200, body)) => Some(String::from_utf8_lossy(&body).into_owned()),
        _ => None,
    }
}

fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Result<(u16, Vec<u8>), String> {
    let mut stream =
        TcpStream::connect_timeout(&imds_addr(), TIMEOUT).map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 169.254.169.254\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("recv: {e}"))?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed http response")?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty http response")?;
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = &raw[sep + 4..];
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    Ok((code, body))
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let nl = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("truncated chunk header")?;
        let size_str = String::from_utf8_lossy(&b[..nl]);
        let size_tok = size_str.trim().split(';').next().unwrap_or("").trim().to_string();
        let size = usize::from_str_radix(&size_tok, 16)
            .map_err(|e| format!("bad chunk size '{size_str}': {e}"))?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err("truncated chunk body".into());
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let (code, body) = parse_response(raw).unwrap();
        assert_eq!(code, 200);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn parse_404_response() {
        let raw = b"HTTP/1.1 404 Not Found\r\n\r\n";
        let (code, body) = parse_response(raw).unwrap();
        assert_eq!(code, 404);
        assert!(body.is_empty());
    }

    #[test]
    fn parse_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (code, body) = parse_response(raw).unwrap();
        assert_eq!(code, 200);
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn dechunk_rejects_truncated() {
        assert!(dechunk(b"5\r\nhel").is_err());
        assert!(dechunk(b"zz\r\n").is_err());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_response(b"not http").is_err());
        assert!(parse_response(b"HTTP/1.1 abc\r\n\r\n").is_err());
    }
}
