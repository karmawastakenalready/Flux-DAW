//! A small HTTP/1.1 test server on 127.0.0.1 with range support, redirects and failure modes.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// How a path answers.
#[derive(Clone, Debug)]
pub enum Route {
    /// A file. `etag` enables `If-Range`; `ranges = false` ignores `Range` (always 200).
    File { body: Vec<u8>, etag: Option<String>, ranges: bool, content_type: &'static str },
    /// The first `times` requests send full headers but close the connection after `after` body bytes.
    Drop { body: Vec<u8>, after: usize, times: usize },
    /// 302 to this location.
    Redirect(String),
    /// A bare status with a short body.
    Status(u16),
}

/// One request as the server saw it.
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub headers: HashMap<String, String>,
}

pub struct Server {
    pub base: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl Server {
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let drops: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let (r, h) = (Arc::clone(&routes), Arc::clone(&hits));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (r, h, d) = (Arc::clone(&r), Arc::clone(&h), Arc::clone(&drops));
                thread::spawn(move || {
                    let _ = serve(stream, &r, &h, &d);
                });
            }
        });
        Server { base, routes, hits }
    }

    pub fn route(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    pub fn file(&self, path: &str, body: &[u8]) {
        self.route(path, Route::File { body: body.to_vec(), etag: Some(format!("\"{}\"", body.len())), ranges: true, content_type: "application/octet-stream" });
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    pub fn hits_for(&self, path: &str) -> Vec<Hit> {
        self.hits().into_iter().filter(|h| h.path == path).collect()
    }

    pub fn clear_hits(&self) {
        self.hits.lock().unwrap().clear();
    }
}

fn serve(
    mut s: TcpStream,
    routes: &Mutex<HashMap<String, Route>>,
    hits: &Mutex<Vec<Hit>>,
    drops: &Mutex<HashMap<String, usize>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut l = String::new();
        if reader.read_line(&mut l)? == 0 || l == "\r\n" {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    hits.lock().unwrap().push(Hit { path: path.clone(), headers: headers.clone() });
    let route = routes.lock().unwrap().get(&path).cloned();
    let head = |code: u16, extra: &str, len: usize| format!("HTTP/1.1 {code} X\r\nConnection: close\r\nContent-Length: {len}\r\n{extra}\r\n");
    match route {
        None => s.write_all(head(404, "", 0).as_bytes())?,
        Some(Route::Status(c)) => s.write_all(format!("{}{}", head(c, "", 2), "no").as_bytes())?,
        Some(Route::Redirect(to)) => s.write_all(head(302, &format!("Location: {to}\r\n"), 0).as_bytes())?,
        Some(Route::Drop { body, after, times }) => {
            let n = {
                let mut d = drops.lock().unwrap();
                let n = d.entry(path.clone()).or_default();
                *n += 1;
                *n
            };
            let (start, end, code, extra) = range_of(&headers, body.len(), None);
            let piece = &body[start..end.max(start)];
            s.write_all(head(code, &extra, piece.len()).as_bytes())?;
            if n <= times {
                s.write_all(&piece[..after.min(piece.len())])?;
                s.flush()?;
                return Ok(()); // connection closes early
            }
            s.write_all(piece)?;
        }
        Some(Route::File { body, etag, ranges, content_type }) => {
            let mut extra = format!("Content-Type: {content_type}\r\n");
            if let Some(e) = &etag {
                extra.push_str(&format!("ETag: {e}\r\n"));
            }
            if ranges {
                extra.push_str("Accept-Ranges: bytes\r\n");
            }
            let (start, end, code, range_extra) =
                if ranges { range_of(&headers, body.len(), etag.as_deref()) } else { (0, body.len(), 200, String::new()) };
            extra.push_str(&range_extra);
            if code == 416 {
                s.write_all(head(416, &extra, 0).as_bytes())?;
            } else {
                let piece = &body[start..end];
                s.write_all(head(code, &extra, piece.len()).as_bytes())?;
                s.write_all(piece)?;
            }
        }
    }
    s.flush()
}

/// Returns (start, end exclusive, status, extra headers) for a request's `Range` / `If-Range`.
fn range_of(headers: &HashMap<String, String>, len: usize, etag: Option<&str>) -> (usize, usize, u16, String) {
    let whole = (0, len, 200, String::new());
    let Some(r) = headers.get("range") else { return whole };
    if let Some(ir) = headers.get("if-range")
        && Some(ir.as_str()) != etag
    {
        return whole; // validator changed: whole file
    }
    let Some(spec) = r.strip_prefix("bytes=") else { return whole };
    let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
    let start: usize = a.parse().unwrap_or(0);
    if start >= len {
        return (0, 0, 416, format!("Content-Range: bytes */{len}\r\n"));
    }
    let end: usize = b.parse().map_or(len - 1, |e: usize| e.min(len - 1));
    (start, end + 1, 206, format!("Content-Range: bytes {start}-{end}/{len}\r\n"))
}

/// Deterministic pseudo-random bytes.
pub fn bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}

pub fn sha256(b: &[u8]) -> String {
    flux_assets::hash::sha256_bytes(b)
}
