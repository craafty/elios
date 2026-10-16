//! `cargo xtask mirror`: a caching mirror of Alpine's repositories for
//! eliOS in QEMU, so testing big packages (Firefox) doesn't download them
//! again on every boot.
//!
//! It serves plain HTTP on port 8086 of this machine, which QEMU's network
//! reaches as 10.0.2.2. A request for `/alpine/...` is answered from
//! `target/alpine-cache/` or, the first time, fetched from Alpine's CDN and
//! kept. Package lists (APKINDEX) are refreshed after a day. In eliOS:
//!
//! ```text
//! echo "repositories = http://10.0.2.2:8086/alpine/v3.24/main http://10.0.2.2:8086/alpine/v3.24/community" >> /etc/system.conf
//! ```
//!
//! eli checks every package against Alpine's signatures, so the mirror needn't
//! be trusted.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

const PORT: u16 = 8086;
const UPSTREAM: &str = "dl-cdn.alpinelinux.org";

pub fn run(cache: PathBuf) -> Result<(), String> {
    fs::create_dir_all(&cache).map_err(|e| format!("creating {}: {e}", cache.display()))?;
    let listener = TcpListener::bind(("0.0.0.0", PORT)).map_err(|e| format!("listening on port {PORT}: {e}"))?;
    eprintln!("xtask: mirroring http://{UPSTREAM}/alpine on port {PORT}, cache in {}", cache.display());
    for stream in listener.incoming().flatten() {
        let cache = cache.clone();
        thread::spawn(move || {
            if let Err(error) = serve(stream, &cache) {
                eprintln!("xtask: mirror: {error}");
            }
        });
    }
    Ok(())
}

fn serve(mut stream: TcpStream, cache: &Path) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut request = String::new();
    reader.read_line(&mut request).map_err(|e| e.to_string())?;
    // Skip the headers.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line.trim().is_empty() {
            break;
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    let safe = path.starts_with("/alpine/") && !path.contains("..") && !path.contains('?');
    if !safe {
        return respond(&mut stream, 404, b"not found");
    }
    let file = cache.join(&path[1..]);
    let stale = path.ends_with("APKINDEX.tar.gz")
        && fs::metadata(&file)
            .and_then(|m| m.modified())
            .map(|time| SystemTime::now().duration_since(time).unwrap_or_default() > Duration::from_secs(86_400))
            .unwrap_or(true);
    if !file.exists() || stale {
        match fetch(&path) {
            Ok(body) => {
                if let Some(parent) = file.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let partial = file.with_extension("partial");
                fs::write(&partial, &body).and_then(|_| fs::rename(&partial, &file)).map_err(|e| e.to_string())?;
                eprintln!("xtask: mirror: fetched {path} ({} KiB)", body.len() / 1024);
            }
            Err(error) if !file.exists() => {
                eprintln!("xtask: mirror: {path}: {error}");
                return respond(&mut stream, 502, error.as_bytes());
            }
            Err(_) => {}
        }
    }
    let body = fs::read(&file).map_err(|e| e.to_string())?;
    respond(&mut stream, 200, &body)
}

fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Gateway",
    };
    let header = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(header.as_bytes()).and_then(|_| stream.write_all(body)).map_err(|e| e.to_string())
}

/// Fetches a path from Alpine's CDN over plain HTTP.
fn fetch(path: &str) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect((UPSTREAM, 80)).map_err(|e| format!("connecting to {UPSTREAM}: {e}"))?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {UPSTREAM}\r\nConnection: close\r\nUser-Agent: eliOS-xtask\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;
    let end = response.windows(4).position(|w| w == b"\r\n\r\n").ok_or("no HTTP header")?;
    let header = String::from_utf8_lossy(&response[..end]).to_string();
    let status = header.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        return Err(format!("{UPSTREAM} answered {status}"));
    }
    let body = &response[end + 4..];
    if header.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { Ok(body.to_vec()) }
}

/// Undoes HTTP's chunked transfer encoding.
fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n").ok_or("bad chunk")?;
        let size_text = String::from_utf8_lossy(&data[..line_end]);
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16).map_err(|_| "bad chunk")?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        body.extend_from_slice(data.get(..size).ok_or("short chunk")?);
        data = data.get(size + 2..).ok_or("short chunk")?;
    }
}
