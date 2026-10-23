mod ui;

use drifter::download::{Control, Downloader, Phase, Progress};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "Drifter — parallel HTTP downloader\n\n  drifter                              Open desktop app\n  drifter --download URL [DIR] [1..32]  Download without the GUI"
        );
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--download") {
        anyhow::ensure!(
            (2..=4).contains(&args.len()),
            "Usage: drifter --download URL [DIR] [1..32]"
        );
        let folder = args
            .get(2)
            .map(PathBuf::from)
            .unwrap_or_else(ui::default_folder);
        let connections = args
            .get(3)
            .map(|s| s.parse::<usize>())
            .transpose()?
            .unwrap_or(8);
        let runtime = tokio::runtime::Runtime::new()?;
        let progress = Arc::new(Progress::default());
        runtime.block_on(Downloader::new()?.download(
            &args[1],
            &folder,
            connections,
            progress.clone(),
            Control::default(),
        ));
        let phase = progress.details.lock().unwrap().0.clone();
        match phase {
            Phase::Complete(path) => println!(
                "Saved {} bytes to {}",
                progress.bytes.load(Ordering::Relaxed),
                path.display()
            ),
            Phase::Failed(message) => anyhow::bail!(message),
            _ => anyhow::bail!("Download cancelled"),
        }
        return Ok(());
    }
    anyhow::ensure!(args.is_empty(), "Unknown arguments. Use --help for usage.");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let downloader = Downloader::new()?;
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Drifter")
            .with_inner_size([1060.0, 780.0])
            .with_min_inner_size([620.0, 620.0]),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Drifter",
        options,
        Box::new(move |cc| Ok(Box::new(ui::DrifterApp::new(cc, runtime, downloader)))),
    )
    .map_err(|error| anyhow::anyhow!("Could not open wgpu desktop window: {error}"))
}
