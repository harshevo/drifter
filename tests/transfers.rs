use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, State},
    http::{HeaderMap, Response, StatusCode},
    response::Redirect,
    routing::get,
};
use drifter::download::{
    Control, Downloader, Phase, Progress, parse_content_range, sanitize_filename, split_ranges,
    validate_url,
};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Fixture {
    bytes: Vec<u8>,
    requests: Mutex<Vec<String>>,
    failures: AtomicUsize,
}

struct Server {
    url: String,
    state: Arc<Fixture>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server() -> Server {
    let state = Arc::new(Fixture {
        bytes: (0..(2 * 1024 * 1024 + 123))
            .map(|i| ((i * 31 + i / 7) % 251) as u8)
            .collect(),
        requests: Mutex::new(Vec::new()),
        failures: AtomicUsize::new(0),
    });
    let router = Router::new()
        .route("/redirect", get(|| async { Redirect::temporary("/range") }))
        .fallback(get(serve))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { url, state, task }
}

async fn serve(
    State(state): State<Arc<Fixture>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    let mode = uri.path();
    if mode == "/html" {
        return Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Body::from("<html>challenge</html>"))
            .unwrap();
    }
    if mode == "/zero" {
        return if headers.contains_key("Range") {
            Response::builder()
                .status(416)
                .header("Content-Range", "bytes */0")
                .body(Body::empty())
                .unwrap()
        } else {
            Response::builder()
                .header("Content-Length", "0")
                .body(Body::empty())
                .unwrap()
        };
    }
    let range = headers
        .get("Range")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.requests.lock().unwrap().push(range.clone());
    let mut builder = Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header(
            "Content-Disposition",
            "attachment; filename=\"fixture.bin\"",
        );
    if mode != "/no-validator" {
        builder = builder.header(
            "ETag",
            if mode == "/changed-etag" && range != "bytes=0-0" {
                "\"fixture-v2\""
            } else {
                "\"fixture-v1\""
            },
        );
    }
    if mode == "/encoded" {
        builder = builder.header("Content-Encoding", "gzip");
    }
    if range.is_empty()
        || mode == "/no-range"
        || mode == "/unknown"
        || (mode == "/change" && range != "bytes=0-0")
    {
        if mode == "/slow" {
            return builder
                .status(200)
                .body(slow_body(state.bytes.clone()))
                .unwrap();
        }
        if mode == "/unknown" {
            return builder
                .status(200)
                .body(Body::from_stream(futures_util::stream::iter(
                    state
                        .bytes
                        .chunks(8192)
                        .map(|c| Ok::<_, std::io::Error>(c.to_vec()))
                        .collect::<Vec<_>>(),
                )))
                .unwrap();
        }
        return builder
            .status(200)
            .body(Body::from(state.bytes.clone()))
            .unwrap();
    }
    let (start, end) = range
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
    if mode == "/retry-status"
        && range != "bytes=0-0"
        && state.failures.fetch_add(1, Ordering::SeqCst) == 0
    {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::empty())
            .unwrap();
    }
    if range != "bytes=0-0" {
        assert_eq!(headers.get("If-Range").unwrap(), "\"fixture-v1\"");
    }
    let reported_start = if mode == "/bad-range" && range != "bytes=0-0" {
        start + 1
    } else {
        start
    };
    builder = builder.status(206).header(
        "Content-Range",
        format!("bytes {reported_start}-{end}/{}", state.bytes.len()),
    );
    let data = state.bytes[start..=end].to_vec();
    if mode == "/retry-body"
        && range != "bytes=0-0"
        && state.failures.fetch_add(1, Ordering::SeqCst) == 0
    {
        let stream = futures_util::stream::unfold((data, 0), |(data, step)| async move {
            match step {
                0 => Some((Ok::<_, std::io::Error>(data[..4096].to_vec()), (data, 1))),
                1 => {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Some((Err(std::io::Error::other("interrupted")), (data, 2)))
                }
                _ => None,
            }
        });
        return builder.body(Body::from_stream(stream)).unwrap();
    }
    if mode == "/short" && range != "bytes=0-0" {
        return builder
            .body(Body::from(data[..data.len() / 2].to_vec()))
            .unwrap();
    }
    if mode == "/oversized" && range != "bytes=0-0" {
        let mut data = data;
        data.push(42);
        return builder.body(Body::from(data)).unwrap();
    }
    if mode == "/slow" && range != "bytes=0-0" {
        return builder.body(slow_body(data)).unwrap();
    }
    builder.body(Body::from(data)).unwrap()
}

async fn transfer(
    server: &Server,
    mode: &str,
    count: usize,
    folder: &std::path::Path,
) -> Arc<Progress> {
    let progress = Arc::new(Progress::default());
    tokio::time::timeout(
        Duration::from_secs(15),
        Downloader::new().unwrap().download(
            &format!("{}{mode}", server.url),
            folder,
            count,
            progress.clone(),
            Control::default(),
        ),
    )
    .await
    .unwrap();
    progress
}

fn completed(progress: &Progress) -> std::path::PathBuf {
    match &progress.details.lock().unwrap().0 {
        Phase::Complete(path) => path.clone(),
        phase => panic!("Expected success, got {phase:?}"),
    }
}

#[tokio::test]
async fn parallel_redirect_and_collision_preserve_exact_bytes() {
    let server = server().await;
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("fixture.bin"), b"existing").unwrap();
    let (a, b) = tokio::join!(
        transfer(&server, "/redirect", 8, folder.path()),
        transfer(&server, "/range", 4, folder.path())
    );
    let a = completed(&a);
    let b = completed(&b);
    assert_ne!(a, b);
    for path in [a, b] {
        assert_eq!(
            Sha256::digest(std::fs::read(path).unwrap()),
            Sha256::digest(&server.state.bytes)
        );
    }
    assert_eq!(
        std::fs::read(folder.path().join("fixture.bin")).unwrap(),
        b"existing"
    );
    assert!(server.state.requests.lock().unwrap().len() >= 14);
    assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 3);
}

#[tokio::test]
async fn fallback_single_stream_and_missing_validator() {
    let server = server().await;
    for mode in ["/no-range", "/no-validator", "/range", "/unknown"] {
        let folder = tempfile::tempdir().unwrap();
        let progress = transfer(
            &server,
            mode,
            if mode == "/range" { 1 } else { 8 },
            folder.path(),
        )
        .await;
        assert_eq!(
            std::fs::read(completed(&progress)).unwrap(),
            server.state.bytes
        );
        assert_eq!(
            progress.bytes.load(Ordering::Relaxed),
            server.state.bytes.len() as u64
        );
    }
}

#[tokio::test]
async fn malformed_ranges_changed_files_html_and_encoded_data_never_publish() {
    let server = server().await;
    for mode in [
        "/bad-range",
        "/change",
        "/changed-etag",
        "/html",
        "/encoded",
        "/oversized",
        "/short",
    ] {
        let folder = tempfile::tempdir().unwrap();
        let progress = transfer(&server, mode, 4, folder.path()).await;
        assert!(
            matches!(progress.details.lock().unwrap().0, Phase::Failed(_)),
            "{mode}"
        );
        assert_eq!(
            std::fs::read_dir(folder.path()).unwrap().count(),
            0,
            "Leaked file for {mode}"
        );
    }
}

#[tokio::test]
async fn retry_transient_server_error() {
    let server = server().await;
    let folder = tempfile::tempdir().unwrap();
    let progress = transfer(&server, "/retry-status", 4, folder.path()).await;
    assert_eq!(
        std::fs::read(completed(&progress)).unwrap(),
        server.state.bytes
    );
    assert!(server.state.failures.load(Ordering::SeqCst) > 4);
}

#[tokio::test]
async fn empty_files_are_downloaded() {
    let server = server().await;
    let folder = tempfile::tempdir().unwrap();
    let progress = transfer(&server, "/zero", 8, folder.path()).await;
    assert_eq!(std::fs::metadata(completed(&progress)).unwrap().len(), 0);
}

#[tokio::test]
async fn pause_resume_and_cancel_cleanup() {
    let server = server().await;
    let folder = tempfile::tempdir().unwrap();
    let progress = Arc::new(Progress::default());
    let control = Control::default();
    let task = tokio::spawn({
        let (progress, control, path, url) = (
            progress.clone(),
            control.clone(),
            folder.path().to_path_buf(),
            format!("{}/slow", server.url),
        );
        async move {
            Downloader::new()
                .unwrap()
                .download(&url, &path, 4, progress, control)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while progress.bytes.load(Ordering::Relaxed) < 32768 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    control.pause(true);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let paused_bytes = progress.bytes.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(progress.bytes.load(Ordering::Relaxed), paused_bytes);
    control.pause(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(progress.bytes.load(Ordering::Relaxed) > paused_bytes);
    control.pause(true);
    control.cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(progress.details.lock().unwrap().0, Phase::Cancelled);
    assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 0);
}

#[test]
fn partition_is_contiguous_balanced_and_handles_large_lengths() {
    for total in [0, 1, 31, 32, 33, 1_000_003, u64::MAX] {
        for count in 1..=32 {
            let ranges = split_ranges(total, count);
            let mut next = 0_u128;
            let mut min = u64::MAX;
            let mut max = 0;
            for (start, end) in ranges {
                assert_eq!(start as u128, next);
                assert!(start <= end);
                let size = end - start + 1;
                min = min.min(size);
                max = max.max(size);
                next = end as u128 + 1;
            }
            assert_eq!(next, total as u128);
            if total > 0 {
                assert!(max - min <= 1);
            }
        }
    }
    assert!(split_ranges(100, 0).is_empty());
}

#[test]
fn input_and_header_validation() {
    assert!(
        validate_url("https://sourceforge.net/projects/example/files/file.zip/download").is_ok()
    );
    for bad in [
        "file:///etc/passwd",
        "ftp://example.com/file",
        "https://user:password@example.com",
        "not a url",
    ] {
        assert!(validate_url(bad).is_err());
    }
    assert_eq!(parse_content_range("bytes 0-0/1").unwrap(), (0, 0, 1));
    for bad in [
        "bytes 3-2/10",
        "bytes 0-10/10",
        "bytes */10",
        "items 0-1/2",
        "bytes 0-1/*",
    ] {
        assert!(parse_content_range(bad).is_err());
    }
    for name in [
        "../../evil.zip",
        "CON",
        "NUL.txt",
        "\0hi\n",
        "...",
        &"文".repeat(160),
    ] {
        let safe = sanitize_filename(name);
        assert!(!safe.contains('/') && !safe.contains('\\') && !safe.contains('\0'));
        assert!(!safe.is_empty() && safe.len() <= 201);
        assert_ne!(safe, "CON");
    }
}

#[tokio::test]
async fn interrupted_body_resumes_from_last_written_byte() {
    let server = server().await;
    let folder = tempfile::tempdir().unwrap();
    let progress = transfer(&server, "/retry-body", 4, folder.path()).await;
    assert_eq!(
        std::fs::read(completed(&progress)).unwrap(),
        server.state.bytes
    );
    assert!(
        server
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|range| range.starts_with("bytes=4096-"))
    );
    assert_eq!(
        progress.bytes.load(Ordering::Relaxed),
        server.state.bytes.len() as u64
    );
}

#[tokio::test]
async fn resume_finishes_with_exact_bytes() {
    let server = server().await;
    let folder = tempfile::tempdir().unwrap();
    let progress = Arc::new(Progress::default());
    let control = Control::default();
    control.pause(true);
    let task = tokio::spawn({
        let (progress, control, path, url) = (
            progress.clone(),
            control.clone(),
            folder.path().to_path_buf(),
            format!("{}/slow", server.url),
        );
        async move {
            Downloader::new()
                .unwrap()
                .download(&url, &path, 8, progress, control)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(server.state.requests.lock().unwrap().is_empty());
    control.pause(false);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(completed(&progress)).unwrap(),
        server.state.bytes
    );
}

#[tokio::test]
async fn invalid_connection_counts_leave_no_files() {
    let server = server().await;
    let folder = tempfile::tempdir().unwrap();
    for count in [0, 33, usize::MAX] {
        let progress = transfer(&server, "/range", count, folder.path()).await;
        assert!(matches!(
            progress.details.lock().unwrap().0,
            Phase::Failed(_)
        ));
    }
    assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 0);
}

fn slow_body(data: Vec<u8>) -> Body {
    let stream = futures_util::stream::unfold((data, 0), |(data, offset)| async move {
        if offset == data.len() {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(12)).await;
        let end = (offset + 8192).min(data.len());
        let chunk = data[offset..end].to_vec();
        Some((Ok::<_, std::io::Error>(chunk), (data, end)))
    });
    Body::from_stream(stream)
}

#[tokio::test]
#[ignore = "Controlled per-connection throttling benchmark; run with --ignored --nocapture"]
async fn controlled_parallel_benchmark() {
    let server = server().await;
    let mut times = Vec::new();
    for count in [1, 8] {
        let folder = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let progress = transfer(&server, "/slow", count, folder.path()).await;
        let elapsed = start.elapsed().as_secs_f64();
        assert_eq!(
            Sha256::digest(std::fs::read(completed(&progress)).unwrap()),
            Sha256::digest(&server.state.bytes)
        );
        println!(
            "{count} connection(s): {elapsed:.3}s, {:.2} MiB/s",
            server.state.bytes.len() as f64 / elapsed / 1048576.0
        );
        times.push(elapsed);
    }
    println!(
        "Controlled local speedup: {:.2}x (not an internet benchmark)",
        times[0] / times[1]
    );
}
