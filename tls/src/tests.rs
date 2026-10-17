//! Known-answer tests from the standards; tests/openssl.rs checks the
//! whole client against OpenSSL's server.

use crate::gcm::Gcm;
use crate::mschapv2;
use crate::x25519;

fn hex(text: &str) -> alloc::vec::Vec<u8> {
    (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap()).collect()
}

fn array(text: &str) -> [u8; 32] {
    hex(text).try_into().unwrap()
}

#[test]
fn x25519_matches_rfc_7748() {
    let scalar = array("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
    let u = array("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
    assert_eq!(x25519::x25519(&scalar, &u), array("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"));
    let alice = array("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    assert_eq!(x25519::public_key(&alice), array("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"));
}

#[test]
fn gcm_matches_nist() {
    let gcm = Gcm::new(&[0; 16]);
    assert_eq!(gcm.seal(&[0; 12], &[], &[]), hex("58e2fccefa7e3061367f1d57a4e7455a"));
    let sealed = gcm.seal(&[0; 12], &[], &[0; 16]);
    assert_eq!(sealed, hex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"));
    assert_eq!(gcm.open(&[0; 12], &[], &sealed).unwrap(), [0; 16]);
    let mut forged = sealed.clone();
    forged[0] ^= 1;
    assert!(gcm.open(&[0; 12], &[], &forged).is_none());
}

#[test]
fn md4_and_des_match_their_standards() {
    assert_eq!(mschapv2::md4(b"").to_vec(), hex("31d6cfe0d16ae931b73c59d7e0c089c0"));
    assert_eq!(mschapv2::md4(b"abc").to_vec(), hex("a448017aaf21d8525fc10ae87aa6729d"));
    // The classic worked example (key 133457799BBCDFF1).
    let key = hex("133457799bbcdff1").try_into().unwrap();
    let block = hex("0123456789abcdef").try_into().unwrap();
    assert_eq!(mschapv2::des_encrypt(key, block).to_vec(), hex("85e813540f0ab405"));
}

#[test]
fn mschapv2_matches_rfc_2759() {
    let authenticator: [u8; 16] = hex("5b5d7c7d7b3f2f3e3c2c602132262628").try_into().unwrap();
    let peer: [u8; 16] = hex("21402324255e262a28295f2b3a337c7e").try_into().unwrap();
    assert_eq!(mschapv2::password_hash("clientPass").to_vec(), hex("44ebba8d5312b8d611474411f56989ae"));
    let response = mschapv2::nt_response(&authenticator, &peer, "User", "clientPass");
    assert_eq!(response.to_vec(), hex("82309ecd8d708b5ea08faa3981cd83544233114a3d85d6df"));
    assert_eq!(
        mschapv2::authenticator_response("clientPass", &response, &peer, &authenticator, "User"),
        "S=407A5589115FD0D6209F510FE9C04566932CDA56"
    );
}
