# Drifter

A Rust desktop downloader with a native wgpu interface, adjustable parallel connections, and streaming disk writes. Designed with the uimaster UI rules: neutral charcoal surfaces, warm accents, rounded groups, and spacious controls.

## Run

```sh
cargo run --locked --release
```

On macOS, create a standalone, locally signed app:

```sh
./scripts/build-macos.sh
open dist/Drifter.app
```

Paste an HTTP or HTTPS file link, choose a folder, set **1–32 connections**, and start. Connection settings apply to new downloads and retries. Pause/resume keeps the current transfer alive within the running app. Cancel removes its temporary file. **Show file** reveals completed files in the system file manager.

Up to four downloads can run at once. Paused transfers still occupy a slot. Completed files are never overwritten; repeated filenames receive numbered suffixes. The app starts without restoring previous transfers or settings.

Headless use:

```sh
cargo run --locked --release -- --download 'https://example.org/archive.zip' "$HOME/Downloads" 8
```

## Transfer design

- **Rust 2024 / Rust 1.98**, **eframe + egui 0.37**, **wgpu 30**, **Tokio 1.54**, **reqwest 0.13.5**, and rustls. Exact resolved versions are pinned in `Cargo.lock`.
- One pooled HTTP client; network and filesystem work run on Tokio workers. The GUI samples active transfers at 10 Hz and receives a completion repaint, keeping transfer work off the UI thread.
- A one-byte GET probes real range support instead of trusting `Accept-Ranges`. Redirects resolve to the final mirror before workers start.
- Parallel requests use HTTP/1.1 to establish actual concurrent connections rather than multiplexing all ranges over one HTTP/2 socket. Small files use fewer workers to avoid unnecessary requests (roughly 256 KiB per part).
- A strong ETag or Last-Modified validator is required for parallel mode. Workers send `If-Range`, validate `206`, bounds, total length, identity encoding, and any returned validator. Unsupported ranges or absent validators at probe time select a single stream. If a server changes its behavior after probing, the transfer fails safely; retry with one connection.
- Disjoint ranges write through independent file handles into one temporary file, with a 256 KiB write buffer per worker. There is no second assembly pass or whole-file memory buffer.
- Interrupted ranges resume at the last accepted byte, with up to four attempts and bounded exponential backoff. Transient `429`/`5xx` responses are retried. A 15-second connect timeout and 30-second read timeout detect stalled connections. Full-stream failures are reported for manual retry.
- Only validated, fully flushed transfers are published through `persist_noclobber`. Errors and cancellation drain workers and remove the temporary file. A forced process kill or power loss can leave hidden `.drifter-*.part` files, which are not reused.
- Filenames come from Content-Disposition or the final URL, are sanitized, and stay within the chosen destination. TLS certificate verification remains enabled. URL query strings are hidden from transfer rows.

`src/download.rs` owns transfer behavior; `src/ui.rs` owns the desktop interface; `src/main.rs` provides GUI and headless entry points.

## SourceForge and other websites

Standard SourceForge `/projects/.../files/.../download` links and direct mirror links work through HTTP redirects. A live download from the public 7-Zip project was checked against an independent curl download. If a website serves an HTML challenge, login page, or landing page, use its direct file link. Browser-only authentication and JavaScript challenges are not implemented.

More connections help when the server limits throughput per connection and allows range requests. They cannot exceed your network, disk, or server capacity. The application does not claim a universal speed increase or zero bugs. Content checks validate protocol consistency and byte counts; the app does not yet compare downloads against publisher-provided cryptographic hashes. Pause/resume and partial-range recovery are in-session only; there is no restart recovery yet.

## Validation

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo test --locked --test transfers controlled_parallel_benchmark -- --ignored --nocapture
```

The integration suite starts a local Axum server and checks:

- Contiguous, balanced ranges including very large lengths.
- SHA-256 equality for parallel downloads and simultaneous filename collisions.
- Redirects, no-range fallback, absent validators, unknown lengths, and empty files.
- Incorrect ranges, changed validators, extra bytes, short bodies, HTML, and unexpected encoding; failures never publish files.
- Retrying transient server errors and resuming interrupted response bodies from the accepted offset.
- Pause, resume to completion, cancellation while paused, temporary-file cleanup, and invalid connection settings.

On the development Mac, the controlled per-connection-throttled local benchmark transferred the same 2 MiB payload in **3.611 s with 1 connection** and **0.468 s with 8 connections** (**7.72×**). Both outputs matched the fixture's SHA-256. This measures concurrency under an artificial per-connection limit, not internet performance.

Live SourceForge check: `7z2301-extra.7z`, 1,027,828 bytes, SHA-256 `db3a1cbe57a26fac81b65c6a2d23feaecdeede3e4c1fe8fb93a7b91d72d1094c`; independent curl output matched. Native startup reported **Apple M5 / Metal**. The rendered interface was inspected at 1060-point and 640-point widths; native URL entry, clipboard paste, and a completed SourceForge transfer were exercised. Linux and Windows source paths are included but have not been built or runtime-tested on those platforms.

## References

- [eframe 0.37 and the wgpu renderer](https://docs.rs/eframe/0.37.0/eframe/enum.Renderer.html)
- [reqwest client configuration](https://docs.rs/reqwest/0.13.5/reqwest/struct.ClientBuilder.html)
- [HTTP byte-range semantics, RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-range-requests)
- [SourceForge mirror documentation](https://sourceforge.net/p/forge/documentation/Mirrors/)
