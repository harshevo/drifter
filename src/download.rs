use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Response, StatusCode, header};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt, BufWriter},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use url::Url;

const MIN_PART: u64 = 256 * 1024;
const MAX_CONNECTIONS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Downloading,
    Complete(PathBuf),
    Failed(String),
    Cancelled,
}

pub struct Progress {
    pub bytes: AtomicU64,
    pub total: AtomicU64,
    pub connections: AtomicUsize,
    pub details: Mutex<(Phase, String, String)>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            bytes: AtomicU64::new(0),
            total: AtomicU64::new(0),
            connections: AtomicUsize::new(0),
            details: Mutex::new((Phase::Connecting, "Resolving file…".into(), String::new())),
        }
    }
}

#[derive(Clone)]
pub struct Control {
    paused: tokio::sync::watch::Sender<bool>,
    pub cancel: CancellationToken,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            paused: tokio::sync::watch::channel(false).0,
            cancel: CancellationToken::new(),
        }
    }
}

impl Control {
    pub fn pause(&self, value: bool) {
        self.paused.send_replace(value);
    }
    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }
    async fn checkpoint(&self) -> Result<()> {
        let mut paused = self.paused.subscribe();
        loop {
            if self.cancel.is_cancelled() {
                bail!("Cancelled");
            }
            if !*paused.borrow_and_update() {
                return Ok(());
            }
            tokio::select! {
                _ = self.cancel.cancelled() => bail!("Cancelled"),
                result = paused.changed() => { result.context("Pause controller closed")?; }
            }
        }
    }
}

#[derive(Clone)]
pub struct Downloader {
    client: Client,
}

impl Downloader {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!("Drifter/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(30))
                .pool_max_idle_per_host(MAX_CONNECTIONS)
                .tcp_nodelay(true)
                .http1_only()
                .redirect(reqwest::redirect::Policy::limited(15))
                .build()?,
        })
    }

    pub async fn download(
        &self,
        url: &str,
        folder: &Path,
        connections: usize,
        progress: Arc<Progress>,
        control: Control,
    ) {
        let result = self
            .run(url, folder, connections, &progress, &control)
            .await;
        progress.connections.store(0, Ordering::Relaxed);
        let phase = match result {
            Ok(path) => Phase::Complete(path),
            Err(_) if control.cancel.is_cancelled() => Phase::Cancelled,
            Err(error) => Phase::Failed(format!("{error:#}")),
        };
        progress.details.lock().unwrap().0 = phase;
    }

    async fn run(
        &self,
        input: &str,
        folder: &Path,
        connections: usize,
        progress: &Arc<Progress>,
        control: &Control,
    ) -> Result<PathBuf> {
        ensure!(
            (1..=MAX_CONNECTIONS).contains(&connections),
            "Choose between 1 and 32 connections"
        );
        let url = validate_url(input)?;
        tokio::fs::create_dir_all(folder)
            .await
            .context("Cannot create download folder")?;
        control.checkpoint().await?;
        let probe = tokio::select! {
            _ = control.cancel.cancelled() => bail!("Cancelled"),
            result = self.client.get(url).header(header::ACCEPT_ENCODING, "identity").header(header::RANGE, "bytes=0-0").send() => result.context("Could not connect to download server")?,
        };
        let probe = if probe.status() == StatusCode::RANGE_NOT_SATISFIABLE
            && probe
                .headers()
                .get(header::CONTENT_RANGE)
                .is_some_and(|h| h == "bytes */0")
        {
            tokio::select! {
                _ = control.cancel.cancelled() => bail!("Cancelled"),
                result = self.client.get(probe.url().clone()).header(header::ACCEPT_ENCODING, "identity").send() => result?,
            }
        } else {
            probe
        };
        probe
            .error_for_status_ref()
            .context("Server rejected the download")?;
        check_encoding(&probe)?;
        reject_page(&probe)?;
        let final_url = probe.url().clone();
        let filename = response_filename(&probe);
        let validator = probe
            .headers()
            .get(header::ETAG)
            .and_then(|h| h.to_str().ok())
            .filter(|h| !h.starts_with("W/"))
            .map(str::to_owned)
            .or_else(|| {
                probe
                    .headers()
                    .get(header::LAST_MODIFIED)
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_owned)
            });
        let range_size = if probe.status() == StatusCode::PARTIAL_CONTENT {
            let (start, end, total) = content_range(&probe)?;
            ensure!(
                start == 0 && end == 0,
                "Server returned an incorrect probe range"
            );
            Some(total)
        } else {
            ensure!(
                probe.status() == StatusCode::OK,
                "Unexpected server response: {}",
                probe.status()
            );
            None
        };
        let total = range_size.or(probe.content_length()).unwrap_or(0);
        progress.total.store(total, Ordering::Relaxed);
        let count = if range_size.is_some() && validator.is_some() {
            (connections as u64).min(total.div_ceil(MIN_PART)).max(1) as usize
        } else {
            1
        };
        {
            let mut details = progress.details.lock().unwrap();
            details.0 = Phase::Downloading;
            details.1 = filename.clone();
            details.2 = if count > 1 {
                "Parallel transfer"
            } else if connections > 1 && range_size.is_none() {
                "Server uses a single stream"
            } else if connections > 1 && validator.is_none() {
                "Single stream · no file validator"
            } else {
                "Single stream"
            }
            .into();
        }
        progress.connections.store(count, Ordering::Relaxed);
        let temporary = tempfile::Builder::new()
            .prefix(".drifter-")
            .suffix(".part")
            .tempfile_in(folder)
            .context("Cannot create temporary download")?;
        let temp_path = temporary.path().to_path_buf();
        if count > 1 {
            drop(probe);
            let mut file = OpenOptions::new().write(true).open(&temp_path).await?;
            file.set_len(total)
                .await
                .context("Cannot allocate download file")?;
            file.flush().await?;
            drop(file);
            let mut workers = JoinSet::new();
            for (start, end) in split_ranges(total, count) {
                let client = self.client.clone();
                let url = final_url.clone();
                let validator = validator.clone().unwrap();
                let path = temp_path.clone();
                let progress = progress.clone();
                let control = control.clone();
                workers.spawn(async move {
                    download_part(
                        client, url, validator, path, start, end, total, progress, control,
                    )
                    .await
                });
            }
            let transfer = async {
                while let Some(result) = workers.join_next().await {
                    result.context("Download worker stopped")??;
                }
                Ok::<_, anyhow::Error>(())
            };
            let result = tokio::select! {
                _ = control.cancel.cancelled() => Err(anyhow::anyhow!("Cancelled")),
                result = transfer => result,
            };
            if result.is_err() {
                workers.abort_all();
                while workers.join_next().await.is_some() {}
            }
            result?;
        } else {
            let response = if probe.status() == StatusCode::OK {
                probe
            } else {
                drop(probe);
                tokio::select! {
                    _ = control.cancel.cancelled() => bail!("Cancelled"),
                    result = self.client.get(final_url).header(header::ACCEPT_ENCODING, "identity").send() => result?,
                }
            };
            let transfer = stream_single(response, &temp_path, progress, control);
            tokio::select! {
                _ = control.cancel.cancelled() => bail!("Cancelled"),
                result = transfer => result?,
            }
        }
        control.checkpoint().await?;
        let file = OpenOptions::new().write(true).open(&temp_path).await?;
        file.sync_all().await.context("Cannot flush file to disk")?;
        drop(file);
        persist_with_suffix(temporary, folder, &filename, 0)
    }
}

fn persist_with_suffix(
    mut temporary: tempfile::NamedTempFile,
    folder: &Path,
    filename: &str,
    start: usize,
) -> Result<PathBuf> {
    for suffix in start..10_000 {
        let destination = folder.join(numbered_name(filename, suffix));
        match temporary.persist_noclobber(&destination) {
            Ok(_) => return Ok(destination),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                temporary = error.file
            }
            Err(error) => return Err(error.error).context("Cannot save completed file"),
        }
    }
    bail!("Too many files with the same name")
}

async fn stream_single(
    mut response: Response,
    path: &Path,
    progress: &Progress,
    control: &Control,
) -> Result<()> {
    response.error_for_status_ref()?;
    ensure!(
        response.status() == StatusCode::OK,
        "Server did not return a complete file"
    );
    check_encoding(&response)?;
    reject_page(&response)?;
    let expected = response.content_length();
    progress
        .total
        .store(expected.unwrap_or(0), Ordering::Relaxed);
    let mut file = BufWriter::with_capacity(256 * 1024, File::create(path).await?);
    let mut received = 0;
    loop {
        control.checkpoint().await?;
        let Some(chunk) = response
            .chunk()
            .await
            .context("Transfer interrupted; retry this download")?
        else {
            break;
        };
        if let Some(expected) = expected {
            ensure!(
                received + chunk.len() as u64 <= expected,
                "Server sent more data than expected"
            );
        }
        file.write_all(&chunk)
            .await
            .context("Cannot write download to disk")?;
        received += chunk.len() as u64;
        progress
            .bytes
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    if let Some(expected) = expected {
        ensure!(
            received == expected,
            "Incomplete file: expected {expected} bytes, received {received}"
        );
    }
    file.flush().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn download_part(
    client: Client,
    url: Url,
    validator: String,
    path: PathBuf,
    start: u64,
    end: u64,
    total: u64,
    progress: Arc<Progress>,
    control: Control,
) -> Result<()> {
    let mut file = OpenOptions::new().write(true).open(path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut file = BufWriter::with_capacity(256 * 1024, file);
    let mut offset = start;
    for attempt in 0..4 {
        control.checkpoint().await?;
        let result = client
            .get(url.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, format!("bytes={offset}-{end}"))
            .header(header::IF_RANGE, &validator)
            .send()
            .await;
        let mut response = match result {
            Ok(response) => response,
            Err(error) if attempt == 3 => {
                return Err(error).context("Connection failed after 4 attempts");
            }
            Err(_) => {
                retry_delay(attempt, &control).await?;
                continue;
            }
        };
        if response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS
        {
            if attempt == 3 {
                bail!("Server remains unavailable: {}", response.status());
            }
            retry_delay(attempt, &control).await?;
            continue;
        }
        ensure!(
            response.status() == StatusCode::PARTIAL_CONTENT,
            "Server stopped honoring byte ranges ({}); retry with 1 connection",
            response.status()
        );
        check_encoding(&response)?;
        let validator_header = if validator.starts_with('"') {
            header::ETAG
        } else {
            header::LAST_MODIFIED
        };
        if let Some(actual) = response.headers().get(validator_header) {
            ensure!(
                actual.to_str().ok() == Some(validator.as_str()),
                "File changed during download; file was not saved"
            );
        }
        ensure!(
            content_range(&response)? == (offset, end, total),
            "Server returned an incorrect byte range; file was not saved"
        );
        while offset <= end {
            control.checkpoint().await?;
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    ensure!(
                        chunk.len() as u64 <= end - offset + 1,
                        "Server sent data outside the requested range"
                    );
                    file.write_all(&chunk)
                        .await
                        .context("Cannot write download to disk")?;
                    offset += chunk.len() as u64;
                    progress
                        .bytes
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                Ok(None) | Err(_) => break,
            }
        }
        if offset > end {
            ensure!(
                response
                    .chunk()
                    .await
                    .context("Cannot validate range completion")?
                    .is_none(),
                "Server sent extra bytes after range"
            );
            file.flush().await?;
            return Ok(());
        }
        if attempt < 3 {
            retry_delay(attempt, &control).await?;
        }
    }
    bail!("Range {start}-{end} was interrupted after 4 attempts")
}

async fn retry_delay(attempt: u32, control: &Control) -> Result<()> {
    tokio::select! {
        _ = control.cancel.cancelled() => bail!("Cancelled"),
        _ = tokio::time::sleep(Duration::from_millis(400 * (1 << attempt))) => Ok(()),
    }
}

pub fn validate_url(input: &str) -> Result<Url> {
    let url =
        Url::parse(input.trim()).context("Enter a complete http:// or https:// download link")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Only HTTP and HTTPS links are supported"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Links containing passwords are not supported"
    );
    Ok(url)
}

fn check_encoding(response: &Response) -> Result<()> {
    if let Some(encoding) = response.headers().get(header::CONTENT_ENCODING) {
        ensure!(
            encoding == "identity",
            "Server ignored identity encoding; cannot safely combine file bytes"
        );
    }
    Ok(())
}

fn reject_page(response: &Response) -> Result<()> {
    if let Some(kind) = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
    {
        ensure!(
            !kind.to_ascii_lowercase().starts_with("text/html"),
            "This link returned a web page. Copy the direct file or SourceForge mirror download link"
        );
    }
    Ok(())
}

fn content_range(response: &Response) -> Result<(u64, u64, u64)> {
    parse_content_range(
        response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|h| h.to_str().ok())
            .context("Missing Content-Range header")?,
    )
}

pub fn parse_content_range(value: &str) -> Result<(u64, u64, u64)> {
    let rest = value
        .strip_prefix("bytes ")
        .context("Invalid Content-Range unit")?;
    let (range, total) = rest.split_once('/').context("Invalid Content-Range")?;
    let (start, end) = range
        .split_once('-')
        .context("Invalid Content-Range bounds")?;
    let (start, end, total) = (
        start.parse::<u64>()?,
        end.parse::<u64>()?,
        total.parse::<u64>()?,
    );
    ensure!(start <= end && end < total, "Invalid Content-Range bounds");
    Ok((start, end, total))
}

pub fn split_ranges(total: u64, count: usize) -> Vec<(u64, u64)> {
    if total == 0 || count == 0 {
        return Vec::new();
    }
    let count = (count as u64).min(total);
    (0..count)
        .map(|i| {
            let base = total / count;
            let extra = total % count;
            let start = i * base + i.min(extra);
            let size = base + u64::from(i < extra);
            (start, start + size - 1)
        })
        .collect()
}

fn response_filename(response: &Response) -> String {
    if let Some(disposition) = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|h| h.to_str().ok())
    {
        for parameter in disposition.split(';').skip(1) {
            if let Some((key, value)) = parameter.trim().split_once('=')
                && key.eq_ignore_ascii_case("filename*")
                && let Some(value) = value
                    .strip_prefix("UTF-8''")
                    .or_else(|| value.strip_prefix("utf-8''"))
            {
                return sanitize_filename(
                    &percent_encoding::percent_decode_str(value).decode_utf8_lossy(),
                );
            }
        }
        for parameter in disposition.split(';').skip(1) {
            if let Some((key, value)) = parameter.trim().split_once('=')
                && key.eq_ignore_ascii_case("filename")
            {
                return sanitize_filename(value.trim_matches('"'));
            }
        }
    }
    let name = response
        .url()
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty() && *s != "download"))
        .unwrap_or("download");
    sanitize_filename(&percent_encoding::percent_decode_str(name).decode_utf8_lossy())
}

pub fn sanitize_filename(input: &str) -> String {
    let name: String = input
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(160)
        .collect();
    let mut name = name;
    while name.len() > 200 {
        name.pop();
    }
    let name = name.trim_matches([' ', '.']);
    if name.is_empty() {
        return "download".into();
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&stem.as_str())
    {
        format!("_{name}")
    } else {
        name.into()
    }
}

fn numbered_name(filename: &str, suffix: usize) -> String {
    if suffix == 0 {
        return filename.into();
    }
    let path = Path::new(filename);
    match (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|s| s.to_str()),
    ) {
        (Some(stem), Some(extension)) => format!("{stem} ({suffix}).{extension}"),
        _ => format!("{filename} ({suffix})"),
    }
}
