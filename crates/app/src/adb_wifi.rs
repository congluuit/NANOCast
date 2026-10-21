//! Android 11+ "Wireless debugging": pair once with the 6-digit code the TV shows, then
//! talk ADB over TLS 1.3 on the port it announces. Same wire format as AOSP
//! `adb/pairing_connection` + `pairing_auth`: inside TLS, SPAKE2 (BoringSSL's variant) on
//! the code plus the TLS exported keying material, then each side sends its `PeerInfo`
//! (our `adbkey.pub` line, the TV's id) sealed with AES-128-GCM. After that the TV trusts
//! the public key of our TLS certificate, made from the same RSA key as plain ADB.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use ring::rand::SecureRandom;
use ring::{aead, digest, hkdf};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

use crate::adb::{Fail, Key};

/// Shown while the TV's "Pair device with pairing code" dialog is open.
pub const PAIRING_SERVICE: &str = "_adb-tls-pairing._tcp.local.";
/// Announced while Wireless debugging is on: the ADB-over-TLS port.
pub const CONNECT_SERVICE: &str = "_adb-tls-connect._tcp.local.";

pub type Tls = StreamOwned<ClientConnection, TcpStream>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// `PairingPacketHeader`: version, type, big-endian payload size.
const HEADER_VERSION: u8 = 1;
const SPAKE2_MSG: u8 = 0;
const PEER_INFO: u8 = 1;
const MAX_PAYLOAD: usize = 2 * PEER_INFO_SIZE;
/// `PeerInfo`: one type byte, then a NUL-terminated string, 8192 bytes in all.
const PEER_INFO_SIZE: usize = 8192;
const ADB_RSA_PUB_KEY: u8 = 0;
/// 64 bytes of `SSL_export_keying_material("adb-label\0")` are appended to the code.
const EXPORTED_KEY_LABEL: &[u8] = b"adb-label\0";

/// Pairs with the TV showing `code` in its "Pair device with pairing code" dialog at
/// `addr`. Afterwards the TV accepts our key over TLS. Returns the TV's id.
pub fn pair(addr: SocketAddr, key: &Key, name: &str, code: &str) -> Result<String, Fail> {
    let sock = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| Fail::NoAdb(format!("{addr}: {e}")))?;
    let mut tls = tls_client(sock, key, addr.ip())?;
    let mut ekm = [0u8; 64];
    tls.conn.export_keying_material(&mut ekm, EXPORTED_KEY_LABEL, None).map_err(|e| Fail::Other(format!("TLS: {e}")))?;
    let password = [code.trim().as_bytes(), &ekm].concat();
    let spake = Spake2::new(true, &password, &random64());

    write_packet(&mut tls, SPAKE2_MSG, &spake.msg)?;
    let theirs = read_packet(&mut tls, SPAKE2_MSG)?;
    let mut cipher = Cipher::new(&spake.finish(&theirs).ok_or_else(|| Fail::Other("pairing: bad SPAKE2 message".into()))?);

    let mut info = vec![0u8; PEER_INFO_SIZE];
    info[0] = ADB_RSA_PUB_KEY;
    let public = key.android_public(name);
    info[1..1 + public.len()].copy_from_slice(&public);
    write_packet(&mut tls, PEER_INFO, &cipher.seal(info))?;
    // A wrong code: the TV cannot open ours and hangs up, or we cannot open theirs.
    let sealed = read_packet(&mut tls, PEER_INFO).map_err(|_| Fail::WrongCode)?;
    let info = cipher.open(sealed).filter(|i| i.len() == PEER_INFO_SIZE).ok_or(Fail::WrongCode)?;
    Ok(c_string(&info[1..]))
}

/// The port `ip` announces `service` on over mDNS, waiting up to `wait`.
pub fn discover(service: &str, ip: Ipv4Addr, wait: Duration, cancel: &AtomicBool) -> Option<u16> {
    let mdns = mdns_sd::ServiceDaemon::new().map_err(|e| log::warn!("mDNS: {e}")).ok()?;
    let rx = mdns.browse(service).map_err(|e| log::warn!("mDNS browse {service}: {e}")).ok()?;
    let deadline = Instant::now() + wait;
    let mut port = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) = rx.recv_timeout(left.min(Duration::from_millis(250))) {
            if info.get_addresses_v4().contains(&ip) && info.get_port() != 0 {
                log::info!("{} on {ip}:{}", info.fullname, info.get_port());
                port = Some(info.get_port());
                break;
            }
        }
    }
    let _ = mdns.stop_browse(service);
    let _ = mdns.shutdown();
    port
}

/// The TLS 1.3 client handshake on `sock`, showing our key's certificate. Like `adb`, any
/// server certificate is accepted: pairing binds the session through the exported keying
/// material, and on the ADB port the TV checks us, not the other way round.
pub fn tls_client(sock: TcpStream, key: &Key, peer: IpAddr) -> Result<Tls, Fail> {
    sock.set_read_timeout(Some(IO_TIMEOUT))?;
    sock.set_write_timeout(Some(IO_TIMEOUT))?;
    let _ = sock.set_nodelay(true);
    let tls_err = |e: rustls::Error| Fail::Other(format!("TLS: {e}"));
    let (cert, private) = certificate(key)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
        .with_client_auth_cert(vec![CertificateDer::from(cert)], PrivateKeyDer::Pkcs8(private.into()))
        .map_err(tls_err)?;
    let conn = ClientConnection::new(Arc::new(config), ServerName::IpAddress(peer.into())).map_err(tls_err)?;
    let mut tls = StreamOwned::new(conn, sock);
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock).map_err(|e| Fail::Other(format!("TLS handshake: {e}")))?;
    }
    Ok(tls)
}

fn write_packet(tls: &mut Tls, kind: u8, payload: &[u8]) -> Result<(), Fail> {
    let mut msg = vec![HEADER_VERSION, kind];
    msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload);
    tls.write_all(&msg)?;
    tls.flush()?;
    Ok(())
}

fn read_packet(tls: &mut Tls, kind: u8) -> Result<Vec<u8>, Fail> {
    let mut head = [0u8; 6];
    tls.read_exact(&mut head).map_err(|e| Fail::Other(format!("pairing: {e}")))?;
    let len = u32::from_be_bytes([head[2], head[3], head[4], head[5]]) as usize;
    if head[0] != HEADER_VERSION || head[1] != kind || len == 0 || len > MAX_PAYLOAD {
        return Err(Fail::Other(format!("pairing: unexpected packet {:02x?}", head)));
    }
    let mut payload = vec![0u8; len];
    tls.read_exact(&mut payload).map_err(|e| Fail::Other(format!("pairing: {e}")))?;
    Ok(payload)
}

fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn random64() -> [u8; 64] {
    let mut r = [0u8; 64];
    ring::rand::SystemRandom::new().fill(&mut r).expect("system random");
    r
}

// ---------- SPAKE2 (BoringSSL `spake25519.c`) ----------

const CLIENT_NAME: &[u8] = b"adb pair client\0";
const SERVER_NAME: &[u8] = b"adb pair server\0";
/// BoringSSL's M and N: the first SHA-256 chain values from "edwards25519 point generation
/// seed (M)" / "(N)" that decode to a point.
const SPAKE_M: [u8; 32] = hex32("5ada7e4bf6ddd9adb6626d32131c6b5c51a1e347a3478f53cfcf441b88eed12e");
const SPAKE_N: [u8; 32] = hex32("10e3df0ae37d8e7a99b5fe74b44672103dbddcbd06af680d71329a11693bc778");

/// One side of BoringSSL's SPAKE2 over Ed25519. Alice is the pairing client.
struct Spake2 {
    alice: bool,
    /// x; the ephemeral key is 8·x, like BoringSSL's left-shifted one.
    private: Scalar,
    password_scalar: Scalar,
    password_hash: [u8; 64],
    msg: [u8; 32],
}

impl Spake2 {
    fn new(alice: bool, password: &[u8], random: &[u8; 64]) -> Self {
        let private = Scalar::from_bytes_mod_order_wide(random);
        let password_hash: [u8; 64] = digest::digest(&digest::SHA512, password).as_ref().try_into().unwrap();
        let password_scalar = Scalar::from_bytes_mod_order_wide(&password_hash);
        let mask = spake_mask(&password_scalar, if alice { &SPAKE_M } else { &SPAKE_N });
        let msg = (EdwardsPoint::mul_base(&private).mul_by_cofactor() + mask).compress().to_bytes();
        Self { alice, private, password_scalar, password_hash, msg }
    }

    /// The 64-byte key from the peer's message; `None` if it is not a point.
    fn finish(&self, theirs: &[u8]) -> Option<[u8; 64]> {
        let q = CompressedEdwardsY::from_slice(theirs).ok()?.decompress()?;
        let their_mask = spake_mask(&self.password_scalar, if self.alice { &SPAKE_N } else { &SPAKE_M });
        let shared = (self.private * (q - their_mask)).mul_by_cofactor().compress();
        let (alice_msg, bob_msg) = if self.alice { (&self.msg[..], theirs) } else { (theirs, &self.msg[..]) };
        let mut h = digest::Context::new(&digest::SHA512);
        for part in [CLIENT_NAME, SERVER_NAME, alice_msg, bob_msg, shared.as_bytes(), &self.password_hash] {
            h.update(&(part.len() as u64).to_le_bytes());
            h.update(part);
        }
        Some(h.finish().as_ref().try_into().unwrap())
    }
}

/// s·P as BoringSSL computes it: its password scalar is s plus multiples of the group
/// order that make it a multiple of 8, which keeps only the prime-order part of P.
fn spake_mask(s: &Scalar, point: &[u8; 32]) -> EdwardsPoint {
    let p = CompressedEdwardsY(*point).decompress().expect("SPAKE2 constant point");
    (s * Scalar::from(8u8).invert()) * p.mul_by_cofactor()
}

const fn hex32(s: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("not hex"),
        }
    }
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

/// AES-128-GCM with the HKDF-SHA256 key from SPAKE2; the nonce is a little-endian message
/// counter per direction (AOSP `aes_128_gcm.cpp`).
struct Cipher {
    key: aead::LessSafeKey,
    sent: u64,
    got: u64,
}

impl Cipher {
    fn new(material: &[u8; 64]) -> Self {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &[]).extract(material);
        let okm = prk.expand(&[b"adb pairing_auth aes-128-gcm key"], &aead::AES_128_GCM).expect("HKDF length");
        Self { key: aead::LessSafeKey::new(aead::UnboundKey::from(okm)), sent: 0, got: 0 }
    }

    fn nonce(n: u64) -> aead::Nonce {
        let mut b = [0u8; 12];
        b[..8].copy_from_slice(&n.to_le_bytes());
        aead::Nonce::assume_unique_for_key(b)
    }

    fn seal(&mut self, mut data: Vec<u8>) -> Vec<u8> {
        self.key.seal_in_place_append_tag(Self::nonce(self.sent), aead::Aad::empty(), &mut data).expect("AES-GCM seal");
        self.sent += 1;
        data
    }

    fn open(&mut self, mut data: Vec<u8>) -> Option<Vec<u8>> {
        let n = self.key.open_in_place(Self::nonce(self.got), aead::Aad::empty(), &mut data).ok()?.len();
        self.got += 1;
        data.truncate(n);
        Some(data)
    }
}

// ---------- certificate ----------

/// A self-signed X.509 v3 certificate for our RSA key ("CN=NANOCast"), and the key as
/// PKCS#8, both DER. adbd only compares the certificate's public key with the paired ones.
fn certificate(key: &Key) -> Result<(Vec<u8>, Vec<u8>), Fail> {
    let fail = |e: &dyn std::fmt::Display| Fail::Other(format!("TLS certificate: {e}"));
    let private = key.0.to_pkcs8_der().map_err(|e| fail(&e))?.as_bytes().to_vec();
    let spki = key.0.to_public_key().to_public_key_der().map_err(|e| fail(&e))?.into_vec();
    // sha256WithRSAEncryption, NULL parameters.
    let algorithm = der(0x30, &[der(0x06, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]), vec![0x05, 0x00]].concat());
    let name = der(0x30, &der(0x31, &der(0x30, &[der(0x06, &[0x55, 0x04, 0x03]), der(0x0c, b"NANOCast")].concat())));
    let validity = der(0x30, &[der(0x17, b"200101000000Z"), der(0x17, b"491231235959Z")].concat());
    let version = der(0xa0, &der(0x02, &[2]));
    let tbs = der(0x30, &[version, der(0x02, &[1]), algorithm.clone(), name.clone(), validity, name, spki].concat());
    let pair = ring::signature::RsaKeyPair::from_pkcs8(&private).map_err(|e| fail(&e))?;
    let mut sig = vec![0u8; pair.public().modulus_len()];
    pair.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), &tbs, &mut sig).map_err(|e| fail(&e))?;
    let bits = [&[0u8][..], &sig].concat();
    Ok((der(0x30, &[tbs, algorithm, der(0x03, &bits)].concat()), private))
}

/// One DER element (lengths up to 64 KiB).
fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let n = body.len();
    let mut out = vec![tag];
    match n {
        0..0x80 => out.push(n as u8),
        0x80..0x100 => out.extend_from_slice(&[0x81, n as u8]),
        _ => out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend_from_slice(body);
    out
}

/// Accepts any server certificate but still checks the handshake signature against it.
#[derive(Debug)]
struct AnyServer(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyServer {
    fn verify_server_cert(&self, _: &CertificateDer, _: &[CertificateDer], _: &ServerName, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The TV side, for tests: a TLS server that only lets in `known` keys (as adbd does after
/// pairing), and a pairing server.
#[cfg(test)]
pub mod fake {
    use super::*;
    use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
    use rustls::{DistinguishedName, ServerConfig, ServerConnection};

    pub type TlsServer = StreamOwned<ServerConnection, TcpStream>;

    #[derive(Debug)]
    struct KnownClients {
        provider: Arc<CryptoProvider>,
        /// SubjectPublicKeyInfo DER of the keys that may connect; empty = any.
        known: Vec<Vec<u8>>,
    }

    impl ClientCertVerifier for KnownClients {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }

        fn verify_client_cert(&self, cert: &CertificateDer, _: &[CertificateDer], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
            let cert: &[u8] = cert;
            if self.known.is_empty() || self.known.iter().any(|k| cert.windows(k.len()).any(|w| w == k.as_slice())) {
                Ok(ClientCertVerified::assertion())
            } else {
                Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
            }
        }

        fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
        }

        fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider.signature_verification_algorithms.supported_schemes()
        }
    }

    pub fn spki(key: &Key) -> Vec<u8> {
        key.0.to_public_key().to_public_key_der().unwrap().into_vec()
    }

    /// The server side of the TLS handshake on `sock` with `tv_key`'s certificate.
    pub fn tls_server(sock: TcpStream, tv_key: &Key, known: Vec<Vec<u8>>) -> Result<TlsServer, String> {
        let (cert, private) = certificate(tv_key).map_err(|e| e.to_string())?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(Arc::new(KnownClients { provider, known }))
            .with_single_cert(vec![CertificateDer::from(cert)], PrivateKeyDer::Pkcs8(private.into()))
            .unwrap();
        let mut tls = StreamOwned::new(ServerConnection::new(Arc::new(config)).unwrap(), sock);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock).map_err(|e| e.to_string())?;
        }
        Ok(tls)
    }

    /// Answers one pairing attempt with `code`; returns the client's `PeerInfo` string, or
    /// `None` when the client used another code.
    pub fn serve_pairing(sock: TcpStream, tv_key: &Key, code: &str, guid: &str) -> Option<String> {
        let mut tls = tls_server(sock, tv_key, Vec::new()).unwrap();
        let mut ekm = [0u8; 64];
        tls.conn.export_keying_material(&mut ekm, EXPORTED_KEY_LABEL, None).unwrap();
        let spake = Spake2::new(false, &[code.as_bytes(), &ekm].concat(), &random64());
        let packet = |tls: &mut TlsServer, kind: u8, payload: &[u8]| {
            let mut msg = vec![HEADER_VERSION, kind];
            msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            msg.extend_from_slice(payload);
            tls.write_all(&msg).unwrap();
            tls.flush().unwrap();
        };
        let take = |tls: &mut TlsServer, kind: u8| {
            let mut head = [0u8; 6];
            tls.read_exact(&mut head).unwrap();
            assert_eq!(&head[..2], &[HEADER_VERSION, kind]);
            let mut payload = vec![0u8; u32::from_be_bytes([head[2], head[3], head[4], head[5]]) as usize];
            tls.read_exact(&mut payload).unwrap();
            payload
        };
        packet(&mut tls, SPAKE2_MSG, &spake.msg);
        let theirs = take(&mut tls, SPAKE2_MSG);
        let mut cipher = Cipher::new(&spake.finish(&theirs).unwrap());
        let mut info = vec![0u8; PEER_INFO_SIZE];
        info[0] = 1; // ADB_DEVICE_GUID
        info[1..1 + guid.len()].copy_from_slice(guid.as_bytes());
        packet(&mut tls, PEER_INFO, &cipher.seal(info));
        let client = cipher.open(take(&mut tls, PEER_INFO))?;
        assert_eq!(client.len(), PEER_INFO_SIZE);
        assert_eq!(client[0], ADB_RSA_PUB_KEY);
        Some(c_string(&client[1..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The M and N constants are what BoringSSL's generator script finds.
    #[test]
    fn spake2_points_from_seeds() {
        for (seed, expected) in [(&b"edwards25519 point generation seed (M)"[..], SPAKE_M), (&b"edwards25519 point generation seed (N)"[..], SPAKE_N)] {
            let mut v: [u8; 32] = digest::digest(&digest::SHA256, seed).as_ref().try_into().unwrap();
            while CompressedEdwardsY(v).decompress().is_none() {
                v = digest::digest(&digest::SHA256, &v).as_ref().try_into().unwrap();
            }
            assert_eq!(v, expected);
            // Not in the prime-order subgroup: why BoringSSL clears the cofactor of its mask.
            assert!(!CompressedEdwardsY(v).decompress().unwrap().is_torsion_free());
        }
    }

    #[test]
    fn spake2_agrees_only_on_the_same_password() {
        let a = Spake2::new(true, b"123456xyz", &random64());
        let b = Spake2::new(false, b"123456xyz", &random64());
        let c = Spake2::new(false, b"654321xyz", &random64());
        assert_eq!(a.finish(&b.msg), b.finish(&a.msg));
        assert_ne!(a.finish(&c.msg), c.finish(&a.msg));
    }

    #[test]
    fn cipher_counts_messages() {
        let k = [9u8; 64];
        let (mut tx, mut rx) = (Cipher::new(&k), Cipher::new(&k));
        let one = tx.seal(b"one".to_vec());
        let two = tx.seal(b"two".to_vec());
        assert_eq!(one.len(), 3 + 16);
        assert!(rx.open(two.clone()).is_none());
        let mut rx = Cipher::new(&k);
        assert_eq!(rx.open(one).unwrap(), b"one");
        assert_eq!(rx.open(two).unwrap(), b"two");
    }

    #[test]
    fn der_lengths() {
        assert_eq!(der(0x04, &[1, 2]), vec![0x04, 2, 1, 2]);
        assert_eq!(der(0x04, &[0; 200])[..3], [0x04, 0x81, 200]);
        assert_eq!(der(0x04, &[0; 300])[..4], [0x04, 0x82, 0x01, 0x2c]);
    }

    fn key() -> &'static Key {
        crate::adb::tests::test_key()
    }

    #[test]
    fn pair_with_code() {
        let tv_key = crate::adb::tests::other_key();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let tv = std::thread::spawn(move || serve_pairing_once(&listener, tv_key, "482916"));
        assert_eq!(pair(addr, key(), "me@pc", "482916").unwrap(), "adb-FAKE1234-abcdef");
        let public = tv.join().unwrap().expect("same code");
        assert_eq!(public.as_bytes(), &key().android_public("me@pc")[..public.len()]);
    }

    #[test]
    fn pair_with_wrong_code() {
        let tv_key = crate::adb::tests::other_key();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let tv = std::thread::spawn(move || serve_pairing_once(&listener, tv_key, "482916"));
        assert!(matches!(pair(addr, key(), "me@pc", "111111"), Err(Fail::WrongCode)));
        assert!(tv.join().unwrap().is_none());
    }

    fn serve_pairing_once(listener: &std::net::TcpListener, tv_key: &Key, code: &str) -> Option<String> {
        let (s, _) = listener.accept().unwrap();
        fake::serve_pairing(s, tv_key, code, "adb-FAKE1234-abcdef")
    }

    /// `adb pair` from the Android SDK against our pairing server: run with
    /// `--ignored --nocapture`, then type the printed command.
    #[test]
    #[ignore = "manual: needs adb from the Android SDK"]
    fn real_adb_pairs_with_fake_tv() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        println!("adb pair {} 123456", listener.local_addr().unwrap());
        let got = serve_pairing_once(&listener, crate::adb::tests::other_key(), "123456");
        println!("adb sent: {got:?}");
        assert!(got.is_some());
    }

    /// Against a real TV / emulator with Wireless debugging on: `NANOCAST_PAIR_AT` and
    /// `NANOCAST_PAIR_CODE` from "Pair device with pairing code" (optional when already
    /// paired), `NANOCAST_CONNECT_AT` from the Wireless debugging screen.
    #[test]
    #[ignore = "manual: needs an Android 11+ device with Wireless debugging"]
    fn real_tv_pair_and_connect() {
        let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        let key = Key::load_or_create(&std::env::temp_dir().join("nanocast-test-adbkey.pem")).unwrap();
        if let (Some(at), Some(code)) = (var("NANOCAST_PAIR_AT"), var("NANOCAST_PAIR_CODE")) {
            println!("paired: {}", pair(at.parse().unwrap(), &key, "nanocast@test", &code).unwrap());
        }
        let at = var("NANOCAST_CONNECT_AT").expect("NANOCAST_CONNECT_AT").parse().unwrap();
        let cancel = AtomicBool::new(false);
        let mut dev = crate::adb::Device::connect(at, &key, "nanocast@test", Duration::from_secs(5), &cancel, || panic!("no Allow dialog over TLS")).unwrap();
        println!("banner: {}", dev.banner);
        let out = dev.shell("getprop ro.build.version.release; echo ok", Duration::from_secs(10)).unwrap();
        println!("shell: {out}");
        assert!(out.contains("ok"));
    }
}
