//! Real offline localhost TLS verification through the production S3/attohttpc stack.
use cx_backend::{BackendError, RemoteBackend, S3Backend, S3Options};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

fn roundtrip(trust_server: bool, matching_hostname: bool) -> Result<Option<Vec<u8>>, BackendError> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let unrelated_key = KeyPair::generate().unwrap();
    let mut unrelated_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    unrelated_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let unrelated = unrelated_params.self_signed(&unrelated_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf_params = CertificateParams::new(vec![
        if matching_hostname {
            "localhost"
        } else {
            "wrong.example"
        }
        .into(),
    ])
    .unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut tls = rustls::StreamOwned::new(connection, socket);
        let mut request = Vec::new();
        let mut byte = [0];
        loop {
            match tls.read(&mut byte) {
                Ok(1) => request.push(byte[0]),
                _ => return, // expected when the client rejects CA or hostname
            }
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
        tls.flush().unwrap();
    });
    let path = std::env::temp_dir().join(format!("cx-tls-{port}-{}.pem", std::process::id()));
    // The trusted CA is SECOND: successful handshake proves bundle handling.
    let mut bundle = unrelated.pem();
    if trust_server {
        bundle.push_str(&ca.pem());
    }
    std::fs::write(&path, bundle).unwrap();
    let mut backend = S3Backend::new(S3Options {
        name: "local-tls".into(),
        endpoint: Some(format!("https://localhost:{port}")),
        region: "us-east-1".into(),
        bucket: "test-bucket".into(),
        prefix: String::new(),
        access_key_id: "dummy-id".into(),
        secret_access_key: "dummy-secret".into(),
        session_token: None,
        path_style: true,
        ca_file: Some(path.clone()),
        insecure: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let result = backend.get("test-object");
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
    result
}

#[test]
fn real_https_accepts_ca_bundle_second_certificate() {
    assert_eq!(
        roundtrip(true, true).unwrap().as_deref(),
        Some(b"ok".as_slice())
    );
}

#[test]
fn real_https_rejects_untrusted_ca() {
    assert!(matches!(
        roundtrip(false, true),
        Err(BackendError::Transport(_))
    ));
}

#[test]
fn real_https_rejects_wrong_hostname() {
    assert!(matches!(
        roundtrip(true, false),
        Err(BackendError::Transport(_))
    ));
}
