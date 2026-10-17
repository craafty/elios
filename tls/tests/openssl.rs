//! The client against OpenSSL's test server (`openssl s_server`), with
//! each cipher suite, and with the server asking for a client certificate
//! (as RADIUS servers may). Skipped if `openssl` isn't installed.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn random(buffer: &mut [u8]) {
    std::fs::File::open("/dev/urandom").unwrap().read_exact(buffer).unwrap();
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A self-signed RSA certificate and key in a temporary directory.
fn certificate() -> Option<PathBuf> {
    let directory = std::env::temp_dir().join(format!("tls-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let status = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=radius.example.edu"])
        .args(["-addext", "subjectAltName=DNS:radius.example.edu"])
        .arg("-keyout")
        .arg(directory.join("key.pem"))
        .arg("-out")
        .arg(directory.join("cert.pem"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then_some(directory)
}

/// Starts `openssl s_server` with `extra` arguments and connects. With
/// `www`, sends a request; returns the client, the server's answer and
/// what the server printed.
fn exchange(directory: &Path, port: u16, www: bool, extra: &[&str]) -> (tls::Client, String, String) {
    let log = directory.join(format!("server-{port}.txt"));
    let child = Command::new("openssl")
        .args(["s_server", "-tls1_2", "-accept", &port.to_string()])
        .args(if www { &["-www"][..] } else { &[] })
        .arg("-cert")
        .arg(directory.join("cert.pem"))
        .arg("-key")
        .arg(directory.join("key.pem"))
        .args(extra)
        .args(["-keymatexport", "client EAP encryption", "-keymatexportlen", "64"])
        // A pipe left open: the plain server stops at the end of its input.
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&log).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server(child);
    let mut stream = (0..100)
        .find_map(|_| {
            TcpStream::connect(("127.0.0.1", port)).ok().or_else(|| {
                std::thread::sleep(Duration::from_millis(50));
                None
            })
        })
        .expect("s_server didn't start");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

    let mut client = tls::Client::new(Some("radius.example.edu"), random);
    stream.write_all(&client.start()).unwrap();
    let mut answer = Vec::new();
    let mut buffer = [0; 4096];
    let mut sent_request = false;
    loop {
        let count = stream.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        let output = client.receive(&buffer[..count]).unwrap_or_else(|error| panic!("{error}"));
        stream.write_all(&output.send).unwrap();
        answer.extend(output.data);
        if client.established() && !www {
            // The plain server prints the session, then waits for input.
            for _ in 0..100 {
                if std::fs::read_to_string(&log).unwrap().contains("Keying material") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            break;
        }
        if client.established() && !sent_request {
            stream.write_all(&client.send(b"GET / HTTP/1.0\r\n\r\n")).unwrap();
            sent_request = true;
        }
        if client.closed() {
            break;
        }
    }
    drop(server);
    (client, String::from_utf8_lossy(&answer).into_owned(), std::fs::read_to_string(log).unwrap())
}

#[test]
fn talks_to_openssl() {
    let Some(directory) = certificate() else {
        eprintln!("openssl isn't installed; skipping");
        return;
    };
    let base = 20000 + (std::process::id() % 20000) as u16;
    let cases: [(&[&str], &str); 3] = [
        (&["-cipher", "ECDHE-RSA-AES128-GCM-SHA256"], "ECDHE-RSA-AES128-GCM-SHA256"),
        (&["-cipher", "AES128-GCM-SHA256"], "RSA-AES128-GCM-SHA256"),
        (&["-cipher", "ECDHE-RSA-AES128-GCM-SHA256", "-verify", "1"], "ECDHE-RSA-AES128-GCM-SHA256"),
    ];
    for (index, (extra, suite)) in cases.into_iter().enumerate() {
        let (client, answer, _) = exchange(&directory, base + index as u16, true, extra);
        assert_eq!(client.suite_name(), suite);
        assert!(answer.contains("200 ok"), "unexpected answer: {answer}");
        // s_server's status page names the cipher it used.
        assert!(answer.contains("Cipher"), "unexpected answer: {answer}");
        let certificate = tls::x509::Certificate::parse(&client.certificates()[0]).unwrap();
        assert_eq!(certificate.subject, "radius.example.edu");
        assert_eq!(certificate.dns_names, ["radius.example.edu"]);

        // PEAP's keys: the same as OpenSSL's exported keying material.
        let (client, _, printed) = exchange(&directory, base + 10 + index as u16, false, extra);
        let exported: String = client.export(b"client EAP encryption", 64).iter().map(|b| format!("{b:02X}")).collect();
        assert!(printed.contains(&exported), "export {exported} not in {printed}");
    }
    let _ = std::fs::remove_dir_all(directory);
}
