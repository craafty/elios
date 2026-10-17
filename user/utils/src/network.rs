//! Network commands: ifconfig, wifi, ping, host, wget and nc.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::net::{self, DatagramSocket, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use eli::{eprintln, fs, http, io, print, println, time};

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

/// The Wi-Fi driver's status and control file.
const WIFI: &str = "/proc/wifi";

/// The `state:` line of /proc/wifi, and the `error:` line if there is one.
fn wifi_state() -> (String, Option<String>) {
    let text = fs::read_to_string(WIFI).unwrap_or_default();
    let field =
        |name: &str| text.lines().find_map(|line| line.strip_prefix(name)).map(|value| value.trim().to_string());
    (field("state:").unwrap_or_default(), field("error:"))
}

/// Whether the last scan (in /proc/wifi) saw a network called `name`.
fn scanned(name: &str) -> bool {
    let text = fs::read_to_string(WIFI).unwrap_or_default();
    let Some((_, networks)) = text.split_once("networks:\n") else { return false };
    // Each line ends with two spaces and the name.
    let ending = alloc::format!("  {name}");
    networks.lines().any(|line| line.ends_with(&ending))
}

/// `wifi connect` arguments: the network (whose name may have spaces,
/// quoted or not), maybe a password, maybe `--user NAME`.
fn connect_request(words: &[String]) -> Result<String, String> {
    let mut user = None;
    let mut rest = Vec::new();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--user" | "-u" => user = Some(words.next().ok_or("--user needs a name")?.clone()),
            _ => rest.push(word.clone()),
        }
    }
    if rest.is_empty() {
        return Err(String::from("which network?"));
    }
    // "wifi connect Purdue Guest": the longest run of words naming a
    // network seen in the last scan is the name; the rest is the password.
    let split = (1..=rest.len()).rev().find(|&count| scanned(&rest[..count].join(" "))).unwrap_or(1);
    let network = rest[..split].join(" ");
    let mut password = rest[split..].join(" ");
    if password.is_empty() {
        let prompt = if user.is_some() { "Password: " } else { "Password (empty for an open network): " };
        password = eli::term::read_password(prompt).unwrap_or_default();
    }
    match &user {
        Some(_) if password.is_empty() => return Err(String::from("enterprise networks need a password")),
        Some(_) => {}
        None if !password.is_empty() && !(8..=63).contains(&password.len()) => {
            return Err(String::from("WPA2 passwords are 8 to 63 characters (enterprise networks: --user NAME)"));
        }
        None => {}
    }
    Ok(alloc::format!("connect\n{network}\n{password}\n{}", user.unwrap_or_default()))
}

/// `wifi [scan | connect NETWORK [PASSWORD] [--user NAME] | disconnect]`:
/// talks to the Wi-Fi driver through /proc/wifi, then waits for it to
/// finish.
pub fn wifi(arguments: &[String]) -> i32 {
    if !fs::exists(WIFI) {
        eprintln!("wifi: no Wi-Fi card");
        return 1;
    }
    let request = match arguments.get(1).map(String::as_str) {
        None | Some("status") => {
            print!("{}", fs::read_to_string(WIFI).unwrap_or_default());
            return 0;
        }
        Some("scan") => String::from("scan"),
        Some("disconnect") => String::from("disconnect"),
        Some("connect") if arguments.len() >= 3 => match connect_request(&arguments[2..]) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("wifi: {error}");
                return 1;
            }
        },
        _ => {
            eprintln!("usage: wifi [scan | connect NETWORK [PASSWORD] [--user NAME] | disconnect]");
            eprintln!("  enterprise networks (eduroam, campus Wi-Fi): wifi connect NETWORK --user NAME");
            return 2;
        }
    };
    let written = fs::File::open_read_write(WIFI).and_then(|file| file.write_all(request.as_bytes()));
    if let Err(error) = written {
        eprintln!("wifi: {error}");
        return 1;
    }
    // Joining takes a scan, signing in, the handshake and DHCP: allow a
    // minute.
    let deadline = time::monotonic_nanos() + 60_000_000_000;
    let mut last = String::new();
    loop {
        time::sleep_ms(200);
        let (state, error) = wifi_state();
        if state != last && state != "working" {
            println!("{state}");
            last = state.clone();
        }
        if let Some(error) = error {
            eprintln!("wifi: {error}");
            return 1;
        }
        if state == "idle" || state == "connected" {
            break;
        }
        if time::monotonic_nanos() > deadline {
            eprintln!("wifi: still {state}; giving up waiting");
            return 1;
        }
    }
    if arguments[1] == "scan" {
        let text = fs::read_to_string(WIFI).unwrap_or_default();
        if let Some((_, networks)) = text.split_once("networks:\n") {
            print!("{networks}");
        }
    }
    if arguments[1] == "connect" {
        wait_for_address("wlan0");
    }
    0
}

/// Waits a while for DHCP to give `interface` an address and prints it
/// (the kernel's own messages about it no longer reach the screen).
fn wait_for_address(interface: &str) {
    let Ok(socket) = DatagramSocket::udp() else { return };
    let deadline = time::monotonic_nanos() + 15_000_000_000;
    while time::monotonic_nanos() < deadline {
        let address = interface_query(&socket, interface, SIOCGIFADDR).map(address_of);
        if let Some(address) = address.filter(|address| address.0 != [0; 4]) {
            println!("address {address}");
            return;
        }
        time::sleep_ms(250);
    }
    println!("no address yet (DHCP is still asking; see ifconfig)");
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
    let Some(url) = url else {
        eprintln!("usage: wget [-q] [-O file] url");
        return 2;
    };
    if !quiet {
        eprintln!("downloading {url}");
    }
    let body = match http::get(&url, &mut |_, _| {}) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("wget: {error}");
            return 1;
        }
    };
    let name = output.unwrap_or_else(|| {
        let path = url.split('?').next().unwrap_or("");
        let last = path.trim_start_matches("http://").split_once('/').map_or("", |(_, path)| path);
        let last = last.rsplit('/').next().unwrap_or("");
        if last.is_empty() { "index.html".into() } else { last.into() }
    });
    let result = if name == "-" { io::write_all(io::STDOUT, &body) } else { fs::write(&name, &body) };
    if let Err(error) = result {
        eprintln!("wget: {name}: {error}");
        return 1;
    }
    if !quiet && name != "-" {
        eprintln!("saved {} bytes to {name}", body.len());
    }
    0
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
