//! Offline wire regressions for mutation boundaries and strict LIST pagination.
use cx_backend::s3_testserver::TestS3Server;
use cx_backend::{BackendError, MAX_LIST_OBJECTS, RemoteBackend, S3Backend, S3Options};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Scripted = (
    S3Backend,
    Arc<Mutex<Vec<String>>>,
    std::thread::JoinHandle<()>,
);

fn scripted(replies: Vec<(u16, String)>) -> Scripted {
    scripted_region(replies, "us-east-1")
}

fn scripted_region(replies: Vec<(u16, String)>, region: &str) -> Scripted {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let thread = std::thread::spawn(move || {
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse().ok())
                })
                .unwrap_or(0);
            let mut payload = vec![0; length];
            socket.read_exact(&mut payload).unwrap();
            captured
                .lock()
                .unwrap()
                .push(format!("{headers}{}", String::from_utf8_lossy(&payload)));
            write!(
                socket,
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let backend = S3Backend::new(S3Options {
        name: "regression".into(),
        endpoint: Some(endpoint),
        region: region.into(),
        bucket: "bucket".into(),
        prefix: "space-a".into(),
        access_key_id: "dummy-id".into(),
        secret_access_key: "dummy-secret".into(),
        session_token: None,
        path_style: true,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    (backend, requests, thread)
}

#[test]
fn arbitrary_400_preserves_error_and_does_not_retry_or_overwrite() {
    let body = "<Error><Code>InvalidArgument</Code><Message>bad argument</Message><EC>test-ec</EC><RequestId>test-id</RequestId></Error>".into();
    let (mut backend, requests, thread) = scripted(vec![(400, body)]);
    let err = backend.put("object", b"dummy", true).unwrap_err();
    assert!(matches!(err, BackendError::Status { status: 400, .. }));
    let detail = err.to_string();
    assert!(
        detail.contains("InvalidArgument")
            && detail.contains("test-ec")
            && detail.contains("test-id")
    );
    thread.join().unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn create_bucket_conflicts_are_not_assumed_owned() {
    for code in [
        "BucketAlreadyExists",
        "OperationAborted",
        "BucketAlreadyOwnedByYou",
    ] {
        let (mut backend, _, thread) =
            scripted(vec![(409, format!("<Error><Code>{code}</Code></Error>"))]);
        let result = backend.create_bucket();
        if code == "BucketAlreadyOwnedByYou" {
            assert_eq!(result.unwrap(), false);
        } else {
            assert!(matches!(result, Err(BackendError::Conflict(_))));
        }
        thread.join().unwrap();
    }
}

#[test]
fn explicit_create_bucket_includes_non_default_region() {
    let (mut backend, requests, thread) = scripted_region(vec![(200, String::new())], "eu-west-1");
    assert!(backend.create_bucket().unwrap());
    thread.join().unwrap();
    let requests = requests.lock().unwrap();
    assert!(requests[0].starts_with("PUT /bucket/ HTTP/1.1"));
    assert!(requests[0].contains("<LocationConstraint>eu-west-1</LocationConstraint>"));
    assert!(requests[0].contains("/eu-west-1/s3/aws4_request"));
}

#[test]
fn ensure_root_is_a_bounded_read_only_namespace_probe() {
    let body = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".into();
    let (mut backend, requests, thread) = scripted(vec![(200, body)]);
    assert!(!backend.ensure_root().unwrap());
    thread.join().unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /bucket/?"));
    assert!(requests[0].contains("max-keys=1"));
    assert!(requests[0].contains("prefix=space-a%2F"));
}

#[test]
fn repeated_continuation_token_never_returns_partial_success() {
    let body = "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken> token </NextContinuationToken></ListBucketResult>".to_string();
    let (mut backend, requests, thread) = scripted(vec![(200, body.clone()), (200, body)]);
    assert!(matches!(backend.list(""), Err(BackendError::Malformed(_))));
    thread.join().unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("continuation-token=%20token%20"));
}

#[test]
fn out_of_namespace_list_objects_are_rejected() {
    let body = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>space-ab/object</Key></Contents></ListBucketResult>".into();
    let (mut backend, _, thread) = scripted(vec![(200, body)]);
    assert!(matches!(backend.list(""), Err(BackendError::Malformed(_))));
    thread.join().unwrap();
}

#[test]
fn root_listing_does_not_match_similar_namespace_and_preserves_key_spaces() {
    let server = TestS3Server::start("dummy-id", "dummy-secret", "bucket");
    server.seed("space-a/ key ", b"dummy");
    server.seed("space-ab/private", b"dummy");
    let mut backend = S3Backend::new(S3Options {
        name: "namespace".into(),
        endpoint: Some(server.endpoint()),
        region: "us-east-1".into(),
        bucket: "bucket".into(),
        prefix: "space-a".into(),
        access_key_id: "dummy-id".into(),
        secret_access_key: "dummy-secret".into(),
        session_token: None,
        path_style: true,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let listed = backend.list("").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, " key ");
}

#[test]
fn more_than_object_limit_is_an_error_not_partial_success() {
    let server = TestS3Server::start("dummy-id", "dummy-secret", "bucket");
    for index in 0..=MAX_LIST_OBJECTS {
        server.seed(&format!("space-a/{index:05}"), b"dummy");
    }
    let mut backend = S3Backend::new(S3Options {
        name: "limit".into(),
        endpoint: Some(server.endpoint()),
        region: "us-east-1".into(),
        bucket: "bucket".into(),
        prefix: "space-a".into(),
        access_key_id: "dummy-id".into(),
        secret_access_key: "dummy-secret".into(),
        session_token: None,
        path_style: true,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    assert!(matches!(backend.list(""), Err(BackendError::Malformed(_))));
    assert!(server.signature_failures().is_empty());
}
