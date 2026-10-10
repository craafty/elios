//! Network commands: ifconfig, ping, host, wget and nc.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::net::{self, DatagramSocket, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use eli::{eprintln, fs, io, print, println, time};

/// Interface flags.
const IFF_UP: u16 = 0x1;
const IFF_BROADCAST: u16 = 0x2;
const IFF_LOOPBACK: u16 = 0x8;
const IFF_RUNNING: u16 = 0x40;
const IFF_MULTICAST: u16 = 0x1000;

const SIOCGIFFLAGS: usize = 0x8913;
const SIOCGIFADDR: usize = 0x8915;
const SIOCGIFBRDADDR: usize = 0x8919;
const SIOCGIFNETMASK: usize = 0x891B;
const SIOCGIFMTU: usize = 0x8921;
const SIOCGIFHWADDR: usize = 0x8927;

/// Asks the kernel about interface `name` (a `struct ifreq` ioctl) and
/// returns the 24-byte answer.
fn interface_query(socket: &DatagramSocket, name: &str, request: usize) -> Option<[u8; 24]> {
    let mut ifreq = [0u8; 40];
    ifreq[..name.len().min(15)].copy_from_slice(&name.as_bytes()[..name.len().min(15)]);
    // SAFETY: a struct ifreq, which these requests take.
    unsafe { socket.ioctl(request, ifreq.as_mut_ptr()) }.ok()?;
    Some(ifreq[16..40].try_into().unwrap())
}

fn address_of(answer: [u8; 24]) -> Ipv4Addr {
    Ipv4Addr([answer[4], answer[5], answer[6], answer[7]])
}

/// Interface names and their (received, sent) packet and byte counts, from
/// /proc/net/dev.
fn interface_counters() -> Vec<(String, [u64; 4])> {
    let text = fs::read_to_string("/proc/net/dev").unwrap_or_default();
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, numbers) = line.split_once(':')?;
            let numbers: Vec<u64> = numbers.split_whitespace().filter_map(|n| n.parse().ok()).collect();
            let get = |index: usize| numbers.get(index).copied().unwrap_or(0);
            Some((name.trim().to_string(), [get(1), get(0), get(9), get(8)]))
        })
        .collect()
}

pub fn ifconfig(_: &[String]) -> i32 {
    let socket = match DatagramSocket::udp() {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("ifconfig: {error}");
            return 1;
        }
    };
    for (name, [rx_packets, rx_bytes, tx_packets, tx_bytes]) in interface_counters() {
        let flags = interface_query(&socket, &name, SIOCGIFFLAGS).map_or(0, |a| u16::from_le_bytes([a[0], a[1]]));
        let mut names = Vec::new();
        for (bit, label) in [
            (IFF_UP, "UP"),
            (IFF_BROADCAST, "BROADCAST"),
            (IFF_LOOPBACK, "LOOPBACK"),
            (IFF_RUNNING, "RUNNING"),
            (IFF_MULTICAST, "MULTICAST"),
        ] {
            if flags & bit != 0 {
                names.push(label);
            }
        }
        let mtu =
            interface_query(&socket, &name, SIOCGIFMTU).map_or(0, |a| i32::from_le_bytes([a[0], a[1], a[2], a[3]]));
        println!("\x1b[1m{name}\x1b[0m: flags={flags}<{}>  mtu {mtu}", names.join(","));
        if let Some(address) = interface_query(&socket, &name, SIOCGIFADDR).map(address_of) {
            let netmask = interface_query(&socket, &name, SIOCGIFNETMASK).map(address_of).unwrap_or_default();
            print!("        inet {address}  netmask {netmask}");
            if flags & IFF_BROADCAST != 0
                && let Some(broadcast) = interface_query(&socket, &name, SIOCGIFBRDADDR).map(address_of)
            {
                print!("  broadcast {broadcast}");
            }
            println!();
        } else {
            println!("        (no address yet)");
        }
        if let Some(hardware) = interface_query(&socket, &name, SIOCGIFHWADDR)
            && flags & IFF_LOOPBACK == 0
        {
            let mac = &hardware[2..8];
            println!(
                "        ether {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );
        }
        println!("        RX packets {rx_packets}  bytes {rx_bytes}");
        println!("        TX packets {tx_packets}  bytes {tx_bytes}");
        println!();
    }
    0
}

/// `ping [-c count] host`: ICMP echo requests, one a second.
pub fn ping(arguments: &[String]) -> i32 {
    let mut count = 4u32;
    let mut host = None;
    let mut words = arguments[1..].iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-c" => count = words.next().and_then(|n| n.parse().ok()).unwrap_or(4),
            _ => host = Some(word.as_str()),
        }
    }
    let Some(host) = host else {
        eprintln!("usage: ping [-c count] host");
        return 2;
    };
    let address = match net::resolve(host) {
        Ok(address) => address,
        Err(error) => {
            eprintln!("ping: {host}: {error}");
            return 2;
        }
    };
    let socket = match DatagramSocket::ping() {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("ping: {error}");
            return 2;
        }
    };
    let _ = socket.set_read_timeout(1000);
    println!("PING {host} ({address}): 56 data bytes");
    let mut received = 0;
    let mut times = Vec::new();
    for sequence in 1..=count {
        // An echo request: type 8, code 0, checksum, identifier (the kernel
        // sets it), sequence number, then 56 bytes of data.
        let mut request = alloc::vec![8u8, 0, 0, 0, 0, 0];
        request.extend_from_slice(&(sequence as u16).to_be_bytes());
        request.extend((0..56u8).map(|byte| byte.wrapping_mul(7)));
        let sent = time::monotonic_nanos();
        if let Err(error) = socket.send_to(&request, SocketAddr::new(address, 0)) {
            eprintln!("ping: {error}");
            return 1;
        }
        let mut reply = [0u8; 1500];
        loop {
            match socket.receive_from(&mut reply) {
                Ok((length, from))
                    if length >= 8 && reply[0] == 0 && reply[6..8] == (sequence as u16).to_be_bytes() =>
                {
                    let microseconds = (time::monotonic_nanos() - sent) / 1000;
                    println!(
                        "{} bytes from {}: icmp_seq={sequence} time={}.{:03} ms",
                        length,
                        from.ip,
                        microseconds / 1000,
                        microseconds % 1000
                    );
                    received += 1;
                    times.push(microseconds);
                    break;
                }
                Ok(_) => continue,
                Err(_) => {
                    println!("no reply for icmp_seq={sequence}");
                    break;
                }
            }
        }
        if sequence < count {
            let elapsed_ms = (time::monotonic_nanos() - sent) / 1_000_000;
            time::sleep_ms(1000u64.saturating_sub(elapsed_ms));
        }
    }
    println!("--- {host} ping statistics ---");
    let loss = (count - received) * 100 / count.max(1);
    println!("{count} packets transmitted, {received} received, {loss}% packet loss");
    if let (Some(min), Some(max)) = (times.iter().min(), times.iter().max()) {
        let average = times.iter().sum::<u64>() / times.len() as u64;
        let ms = |us: u64| alloc::format!("{}.{:03}", us / 1000, us % 1000);
        println!("round-trip min/avg/max = {}/{}/{} ms", ms(*min), ms(average), ms(*max));
    }
    if received > 0 { 0 } else { 1 }
}

/// `host name`: looks a name up.
pub fn host(arguments: &[String]) -> i32 {
    let Some(name) = arguments.get(1) else {
        eprintln!("usage: host name");
        return 2;
    };
    match net::resolve(name) {
        Ok(address) => {
            println!("{name} has address {address}");
            0
        }
        Err(error) => {
            eprintln!("host: {name}: {error}");
            1
        }
    }
}

/// A parsed `http://host[:port]/path` URL.
struct Url {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(text: &str) -> Result<Url, String> {
    if text.starts_with("https://") {
        return Err("https isn't supported yet (eliOS has no TLS); try http://".into());
    }
    let rest = text.strip_prefix("http://").unwrap_or(text);
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    let (host, port) = net::split_host_port(authority, 80).ok_or("bad port")?;
    Ok(Url { host, port, path: path.into() })
}

/// `wget [-q] [-O file] url`: downloads over HTTP.
pub fn wget(arguments: &[String]) -> i32 {
    let mut output = None;
    let mut quiet = false;
    let mut url = None;
    let mut words = arguments[1..].iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-O" => output = words.next().cloned(),
            "-q" => quiet = true,
            _ => url = Some(word.clone()),
        }
    }
    let Some(mut url) = url else {
        eprintln!("usage: wget [-q] [-O file] url");
        return 2;
    };
    // Follow up to 5 redirects.
    for _ in 0..6 {
        let parsed = match parse_url(&url) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("wget: {error}");
                return 1;
            }
        };
        let response = match http_get(&parsed, quiet) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("wget: {}: {error}", parsed.host);
                return 1;
            }
        };
        if (300..400).contains(&response.status)
            && let Some(location) = response.header("location")
        {
            url = if location.starts_with('/') {
                alloc::format!("http://{}:{}{location}", parsed.host, parsed.port)
            } else {
                location
            };
            if !quiet {
                eprintln!("redirected to {url}");
            }
            continue;
        }
        if response.status != 200 {
            eprintln!("wget: server says {} {}", response.status, response.reason);
            return 1;
        }
        let name = output.clone().unwrap_or_else(|| {
            let last = parsed.path.rsplit('/').next().unwrap_or("");
            if last.is_empty() { "index.html".into() } else { last.into() }
        });
        let result =
            if name == "-" { io::write_all(io::STDOUT, &response.body) } else { fs::write(&name, &response.body) };
        if let Err(error) = result {
            eprintln!("wget: {name}: {error}");
            return 1;
        }
        if !quiet && name != "-" {
            eprintln!("saved {} bytes to {name}", response.body.len());
        }
        return 0;
    }
    eprintln!("wget: too many redirects");
    1
}

struct Response {
    status: u32,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone())
    }
}

fn http_get(url: &Url, quiet: bool) -> Result<Response, eli::Error> {
    let address = net::resolve(&url.host)?;
    if !quiet {
        eprintln!("connecting to {} ({address}:{})...", url.host, url.port);
    }
    let stream = TcpStream::connect(SocketAddr::new(address, url.port))?;
    // HTTP/1.0: the server closes the connection after the response, and
    // never uses chunked encoding.
    let request = alloc::format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: eliOS-wget\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path,
        url.host
    );
    stream.write_all(request.as_bytes())?;
    let data = stream.read_to_end()?;
    let split = data.windows(4).position(|window| window == b"\r\n\r\n").ok_or(eli::Error::INVALID)?;
    let head = String::from_utf8_lossy(&data[..split]);
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status = parts.next().and_then(|code| code.parse().ok()).ok_or(eli::Error::INVALID)?;
    let reason = parts.next().unwrap_or("").into();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    Ok(Response { status, reason, headers, body: data[split + 4..].to_vec() })
}

/// `nc host port`, or `nc -l port`: connects standard input and output to a
/// TCP connection.
pub fn nc(arguments: &[String]) -> i32 {
    let words: Vec<&str> = arguments[1..].iter().map(String::as_str).collect();
    let stream = match words.as_slice() {
        ["-l", port] | ["-lp", port] | ["-l", "-p", port] => {
            let Ok(port) = port.parse() else { return usage_nc() };
            let listener = match TcpListener::bind(SocketAddr::new(Ipv4Addr::default(), port)) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("nc: port {port}: {error}");
                    return 1;
                }
            };
            match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) => {
                    eprintln!("nc: {error}");
                    return 1;
                }
            }
        }
        [host, port] => {
            let Ok(port) = port.parse() else { return usage_nc() };
            let address = match net::resolve(host) {
                Ok(address) => address,
                Err(error) => {
                    eprintln!("nc: {host}: {error}");
                    return 1;
                }
            };
            match TcpStream::connect(SocketAddr::new(address, port)) {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("nc: {host} port {port}: {error}");
                    return 1;
                }
            }
        }
        _ => return usage_nc(),
    };
    relay(&stream)
}

fn usage_nc() -> i32 {
    eprintln!("usage: nc host port, or nc -l port");
    2
}

/// Copies standard input to the connection and the connection to standard
/// output until the connection ends.
fn relay(stream: &TcpStream) -> i32 {
    let mut buffer = [0u8; 4096];
    let mut input_open = true;
    loop {
        let fds: &[i32] = if input_open { &[stream.fd(), io::STDIN] } else { &[stream.fd()] };
        let Ok(ready) = net::poll_readable(fds, -1) else { return 1 };
        if ready[0] {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => return 0,
                Ok(count) => {
                    let _ = io::write_all(io::STDOUT, &buffer[..count]);
                }
            }
        }
        if input_open && ready.get(1) == Some(&true) {
            match io::read(io::STDIN, &mut buffer) {
                Ok(0) | Err(_) => {
                    input_open = false;
                    let _ = stream.shutdown_write();
                }
                Ok(count) => {
                    if stream.write_all(&buffer[..count]).is_err() {
                        return 1;
                    }
                }
            }
        }
    }
}
