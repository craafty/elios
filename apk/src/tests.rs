//! Host tests, checked against independent implementations: Python's zlib,
//! tarfile and hashlib, and OpenSSL. Tests that need a tool the host lacks
//! are skipped.

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use std::path::PathBuf;
use std::process::Command;

use super::*;

fn temporary(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(alloc::format!("eli-apk-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join(name)
}

/// Runs a Python script; `None` if there's no Python.
fn python(script: &str) -> Option<Vec<u8>> {
    let output = Command::new("python3").arg("-c").arg(script).output().ok()?;
    assert!(output.status.success(), "python failed: {}", String::from_utf8_lossy(&output.stderr));
    Some(output.stdout)
}

/// Data that compresses in interesting ways: runs, text, and noise.
fn sample(len: usize) -> Vec<u8> {
    let mut state = 12345u32;
    (0..len)
        .map(|i| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            match (i / 1000) % 3 {
                0 => b"the quick brown fox jumps over the lazy dog "[i % 44],
                1 => (state >> 16) as u8,
                _ => (i % 7) as u8,
            }
        })
        .collect()
}

#[test]
fn inflate_matches_zlib() {
    let data = sample(300_000);
    let path = temporary("sample");
    std::fs::write(&path, &data).unwrap();
    for level in [0, 1, 6, 9] {
        let script = alloc::format!(
            "import gzip,sys; sys.stdout.buffer.write(gzip.compress(open('{}','rb').read(), {level}))",
            path.display()
        );
        let Some(compressed) = python(&script) else { return };
        let (output, used) = inflate::gunzip(&compressed, 1 << 30).unwrap();
        assert!(output == data, "level {level} decompressed wrongly");
        assert_eq!(used, compressed.len());
    }
    // Several gzip members one after another, as packages are.
    let script = "import gzip,sys; sys.stdout.buffer.write(gzip.compress(b'one') + gzip.compress(b'two'*1000))";
    let compressed = python(script).unwrap();
    let members = gzip_members(&compressed).unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[1].1.len(), 3000);
    // Damage is noticed.
    let mut damaged = compressed.clone();
    damaged[20] ^= 0x10;
    assert!(gzip_members(&damaged).is_err());
}

#[test]
fn hashes_match_hashlib() {
    for len in [0, 1, 55, 56, 63, 64, 65, 119, 120, 1000] {
        let data = sample(len);
        let path = temporary("hash");
        std::fs::write(&path, &data).unwrap();
        let script = alloc::format!(
            "import hashlib,base64; d=open('{}','rb').read(); print(hashlib.sha1(d).hexdigest(), \
             hashlib.sha256(d).hexdigest(), base64.b64encode(d).decode())",
            path.display()
        );
        let Some(output) = python(&script) else { return };
        let output = String::from_utf8(output).unwrap();
        let mut fields = output.split_whitespace();
        assert_eq!(hash::hex(&hash::sha1(&data)), fields.next().unwrap(), "SHA-1 of {len} bytes");
        assert_eq!(hash::hex(&hash::sha256(&data)), fields.next().unwrap(), "SHA-256 of {len} bytes");
        let encoded = fields.next().unwrap_or("");
        assert_eq!(hash::base64_encode(&data), encoded);
        assert_eq!(hash::base64_decode(encoded).unwrap(), data);
    }
}

#[test]
fn tar_reads_long_names_links_and_pax() {
    let path = temporary("archive.tar");
    let script = alloc::format!(
        "import tarfile,io\n\
         t=tarfile.open('{}','w',format=tarfile.PAX_FORMAT)\n\
         def add(name,data=b'',kind=tarfile.REGTYPE,link='',mode=0o644):\n\
         \x20i=tarfile.TarInfo(name); i.type=kind; i.linkname=link; i.mode=mode; i.size=len(data); i.mtime=1700000000\n\
         \x20t.addfile(i, io.BytesIO(data) if data else None)\n\
         add('usr', kind=tarfile.DIRTYPE, mode=0o755)\n\
         add('usr/bin/tool', b'#!/bin/sh\\necho hi\\n', mode=0o755)\n\
         add('usr/share/' + 'x'*150 + '/deep.txt', b'long name')\n\
         add('usr/bin/alias', kind=tarfile.SYMTYPE, link='tool')\n\
         add('usr/bin/hard', kind=tarfile.LNKTYPE, link='usr/bin/tool')\n\
         t.close()",
        path.display()
    );
    if python(&script).is_none() {
        return;
    }
    let archive = std::fs::read(&path).unwrap();
    let entries = tar::entries(&archive).unwrap();
    let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names[0], "usr");
    assert_eq!(entries[0].kind, tar::Kind::Directory);
    assert_eq!(
        (entries[1].data, entries[1].mode, entries[1].modified),
        (&b"#!/bin/sh\necho hi\n"[..], 0o755, 1_700_000_000)
    );
    assert!(names[2].len() > 150 && names[2].ends_with("/deep.txt"), "{}", names[2]);
    assert_eq!((entries[3].kind, entries[3].link.as_str()), (tar::Kind::Symlink, "tool"));
    assert_eq!(entries[4].kind, tar::Kind::HardLink);
}

#[test]
fn rsa_signatures_match_openssl() {
    let key = temporary("key.pem");
    let public = temporary("public.pem");
    let data = temporary("data");
    let signature = temporary("signature");
    for bits in ["2048", "4096"] {
        let made = Command::new("openssl").args(["genrsa", "-out"]).arg(&key).arg(bits).output();
        if !made.is_ok_and(|output| output.status.success()) {
            return;
        }
        Command::new("openssl").args(["rsa", "-pubout", "-in"]).arg(&key).arg("-out").arg(&public).output().unwrap();
        let message = sample(5000);
        std::fs::write(&data, &message).unwrap();
        let key_text = std::fs::read_to_string(&public).unwrap();
        let parsed = PublicKey::from_pem(&key_text).expect("couldn't read the key");
        for (digest, name) in [(rsa::Hash::Sha1, "-sha1"), (rsa::Hash::Sha256, "-sha256")] {
            Command::new("openssl")
                .args(["dgst", name, "-sign"])
                .arg(&key)
                .arg("-out")
                .arg(&signature)
                .arg(&data)
                .output()
                .unwrap();
            let signed = std::fs::read(&signature).unwrap();
            let hash = match digest {
                rsa::Hash::Sha1 => hash::sha1(&message).to_vec(),
                rsa::Hash::Sha256 => hash::sha256(&message).to_vec(),
            };
            assert!(parsed.verify(&signed, digest, &hash), "{bits}-bit {name} signature rejected");
            let mut tampered = message.clone();
            tampered[100] ^= 1;
            let wrong = match digest {
                rsa::Hash::Sha1 => hash::sha1(&tampered).to_vec(),
                rsa::Hash::Sha256 => hash::sha256(&tampered).to_vec(),
            };
            assert!(!parsed.verify(&signed, digest, &wrong), "{bits}-bit {name}: a wrong hash was accepted");
        }
    }
}

#[test]
fn versions_compare_as_alpine_does() {
    use core::cmp::Ordering::{Equal, Greater, Less};
    for (a, b, expected) in [
        ("1.10", "1.9", Greater),
        ("1.2.3-r1", "1.2.3-r0", Greater),
        ("1.2_rc1", "1.2", Less),
        ("1.2_p1", "1.2", Greater),
        ("1.2a", "1.2", Greater),
        ("2.54.0-r0", "2.54.0-r0", Equal),
        ("1.2_alpha", "1.2_beta", Less),
        ("9.1.0000-r0", "9.1.1000-r0", Less),
    ] {
        assert_eq!(compare_versions(a, b), expected, "{a} vs {b}");
    }
}

/// A tiny repository: dependencies by name, by library and with versions.
fn sample_index() -> Index {
    let text = "P:app\nV:1.0-r0\nD:so:libgreet.so.1 base>=2\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:greet\nV:3.1-r0\np:so:libgreet.so.1=3.1 cmd:greet\nD:base\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:base\nV:1.0-r0\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:base\nV:2.5-r1\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n";
    Index::new(index::parse(text, 0))
}

#[test]
fn dependencies_resolve_in_order() {
    let index = sample_index();
    let resolved = index.resolve(&["app".into()]).unwrap();
    let ids: Vec<String> = resolved.iter().map(|package| package.id()).collect();
    assert_eq!(ids, ["base-2.5-r1", "greet-3.1-r0", "app-1.0-r0"]);
    assert_eq!(index.providing_command("greet")[0].name, "greet");
    assert!(index.resolve(&["missing".into()]).unwrap_err().contains("missing"));
    assert_eq!(index::split_id("py3-foo-bar-1.2.3-r0"), Some(("py3-foo-bar", "1.2.3-r0")));
}

/// `install_if`: GTK's image loaders come with libglycin (Firefox aborts
/// without them), and a package wanting two things comes only with both.
#[test]
fn install_if_brings_plugins() {
    let text = "P:gtk\nV:3-r0\nD:so:libglycin-2.so.0\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:libglycin\nV:2.1.5-r0\np:so:libglycin-2.so.0=0\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:glycin-image-rs\nV:2.1.5-r0\nD:libgcc\ni:libglycin~2.1.5\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:glycin-old\nV:1.0-r0\ni:libglycin~1.0\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:libgcc\nV:15-r0\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\n\
                P:gtk-lang\nV:3-r0\ni:gtk=3-r0 lang\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\n";
    let index = Index::new(index::parse(text, 0));
    let resolved = index.resolve(&["gtk".into()]).unwrap();
    let mut names: Vec<&str> = resolved.iter().map(|package| package.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["glycin-image-rs", "gtk", "libgcc", "libglycin"]);
}

/// A signed index made the way Alpine makes them, read and checked.
#[test]
fn signed_indexes_are_checked() {
    let key = temporary("index-key.pem");
    let public = temporary("index-key.rsa.pub");
    let made = Command::new("openssl").args(["genrsa", "-out"]).arg(&key).arg("2048").output();
    if !made.is_ok_and(|output| output.status.success()) {
        return;
    }
    Command::new("openssl").args(["rsa", "-pubout", "-in"]).arg(&key).arg("-out").arg(&public).output().unwrap();
    let index = temporary("APKINDEX.tar.gz");
    let script = alloc::format!(
        "import tarfile,io,gzip,subprocess\n\
         def tar(name, data, end):\n\
         \x20b=io.BytesIO(); t=tarfile.open(fileobj=b,mode='w',format=tarfile.USTAR_FORMAT)\n\
         \x20i=tarfile.TarInfo(name); i.size=len(data); t.addfile(i, io.BytesIO(data)); t.close()\n\
         \x20d=b.getvalue()\n\
         \x20return d if end else d[:-1024]\n\
         body=gzip.compress(tar('APKINDEX', b'P:hello\\nV:1.0-r0\\nC:Q1AAAAAAAAAAAAAAAAAAAAAAAAAAA=\\n', True))\n\
         sig=subprocess.run(['openssl','dgst','-sha1','-sign','{}'], input=body, capture_output=True).stdout\n\
         head=gzip.compress(tar('.SIGN.RSA.test.rsa.pub', sig, False))\n\
         open('{}','wb').write(head+body)",
        key.display(),
        index.display()
    );
    if python(&script).is_none() {
        return;
    }
    let file = std::fs::read(&index).unwrap();
    let keys = [Key {
        name: "test.rsa.pub".into(),
        key: PublicKey::from_pem(&std::fs::read_to_string(&public).unwrap()).unwrap(),
    }];
    let text = read_index(&file, &keys).unwrap();
    assert!(text.starts_with("P:hello\nV:1.0-r0"));
    assert_eq!(read_index(&file, &[]).unwrap_err(), Error::UnknownKey("test.rsa.pub".into()));
    let mut tampered = file.clone();
    let last = tampered.len() - 12;
    tampered[last] ^= 1;
    assert!(read_index(&tampered, &keys).is_err());
}

/// Real Alpine files, when `ELI_ALPINE_FILES` names a directory with an
/// `APKINDEX.tar.gz`, a package, and Alpine's keys in `keys/`.
#[test]
fn real_alpine_files_verify() {
    let Ok(directory) = std::env::var("ELI_ALPINE_FILES") else { return };
    let directory = PathBuf::from(directory);
    let keys: Vec<Key> = std::fs::read_dir(directory.join("keys"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            Key {
                name: entry.file_name().to_string_lossy().into_owned(),
                key: PublicKey::from_pem(&std::fs::read_to_string(entry.path()).unwrap()).unwrap(),
            }
        })
        .collect();
    let text = read_index(&std::fs::read(directory.join("APKINDEX.tar.gz")).unwrap(), &keys).unwrap();
    let index = Index::new(index::parse(&text, 0));
    assert!(index.packages.len() > 1000);
    let musl = index.find("musl").unwrap();
    let file = std::fs::read(directory.join(musl.file_name())).unwrap();
    let contents = read_package(&file, &musl.checksum).unwrap();
    let entries = tar::entries(&contents.data).unwrap();
    assert!(entries.iter().any(|entry| entry.path == "lib/ld-musl-x86_64.so.1"));
    let git = index.resolve(&["git".into()]).unwrap();
    assert_eq!(git.last().unwrap().name, "git");
    assert!(git.iter().any(|package| package.name == "musl"));
}
