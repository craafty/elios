//! Downloading over HTTP.
//!
//! Plain HTTP only: eliOS has no TLS yet. Requests are HTTP/1.1 with
//! `Connection: close`, so the response ends when the server closes the
//! connection; bodies sent in chunks are put back together.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::net::{self, SocketAddr, TcpStream};
use crate::sys::Error;

/// A parsed `http://host[:port]/path` URL.
#[derive(Debug, Clone)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.starts_with("https://") {
            return Err("https isn't supported yet (eliOS has no TLS); try http://".into());
        }
        let rest = text.strip_prefix("http://").unwrap_or(text);
        let (authority, path) = match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => (rest, "/"),
        };
        let (host, port) = net::split_host_port(authority, 80).ok_or("bad port")?;
        Ok(Self { host, port, path: path.into() })
    }
}

pub struct Response {
    pub status: u32,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone())
    }
}

/// One request, without following redirects. `progress` is told the bytes
/// received so far and the total, if the server said.
pub fn request(url: &Url, progress: &mut dyn FnMut(usize, Option<usize>)) -> Result<Response, Error> {
    let address = net::resolve(&url.host)?;
    let stream = TcpStream::connect(SocketAddr::new(address, url.port))?;
    let request = alloc::format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: eliOS\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path,
        url.host
    );
    stream.write_all(request.as_bytes())?;
    let mut data = Vec::new();
    let mut buffer = alloc::vec![0u8; 64 * 1024];
    let mut header_end = None;
    let mut total = None;
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..count]);
        if header_end.is_none()
            && let Some(split) = data.windows(4).position(|window| window == b"\r\n\r\n")
        {
            header_end = Some(split);
            let head = String::from_utf8_lossy(&data[..split]).to_lowercase();
            total = head.lines().find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok());
        }
        if let Some(split) = header_end {
            progress(data.len() - split - 4, total);
        }
    }
    let split = header_end.ok_or(Error::INVALID)?;
    let head = String::from_utf8_lossy(&data[..split]);
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or("").splitn(3, ' ');
    let _version = parts.next();
    let status = parts.next().and_then(|code| code.parse().ok()).ok_or(Error::INVALID)?;
    let reason = parts.next().unwrap_or("").into();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    let mut response = Response { status, reason, headers, body: data[split + 4..].to_vec() };
    if response.header("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked")) {
        response.body = unchunk(&response.body).ok_or(Error::INVALID)?;
    }
    Ok(response)
}

/// Puts a chunked body back together: each chunk is a hex length line, the
/// data and a line break; a zero length ends it.
fn unchunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n")?;
        let line = core::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(output);
        }
        output.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
}

/// Downloads `url`, following up to five redirects. Anything but a final
/// 200 is an error, described.
pub fn get(url: &str, progress: &mut dyn FnMut(usize, Option<usize>)) -> Result<Vec<u8>, String> {
    let mut url = String::from(url);
    for _ in 0..6 {
        let parsed = Url::parse(&url)?;
        let response = request(&parsed, progress).map_err(|error| alloc::format!("{}: {error}", parsed.host))?;
        if (300..400).contains(&response.status)
            && let Some(location) = response.header("location")
        {
            url = if location.starts_with('/') {
                alloc::format!("http://{}:{}{location}", parsed.host, parsed.port)
            } else {
                location
            };
            continue;
        }
        if response.status != 200 {
            return Err(alloc::format!("{url}: the server said {} {}", response.status, response.reason));
        }
        return Ok(response.body);
    }
    Err(alloc::format!("{url}: too many redirects"))
}
