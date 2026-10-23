use drifter::download::{Control, Downloader, Phase, Progress, validate_url};
use eframe::egui::{self, Color32, FontId, RichText, Stroke};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

const BACKGROUND: Color32 = Color32::from_rgb(21, 21, 23);
const SURFACE: Color32 = Color32::from_rgb(30, 30, 33);
const TEXT: Color32 = Color32::from_rgb(237, 234, 230);
const MUTED: Color32 = Color32::from_rgb(151, 148, 148);
const ACCENT: Color32 = Color32::from_rgb(238, 167, 106);

pub fn default_folder() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

struct Download {
    url: String,
    folder: PathBuf,
    requested: usize,
    progress: Arc<Progress>,
    control: Control,
    task: tokio::task::JoinHandle<()>,
    last_sample: Instant,
    last_bytes: u64,
    speed: f64,
}

impl Download {
    fn active(&self) -> bool {
        matches!(
            self.progress.details.lock().unwrap().0,
            Phase::Connecting | Phase::Downloading
        )
    }
    fn sample(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();
        if elapsed >= 0.5 {
            let bytes = self.progress.bytes.load(Ordering::Relaxed);
            let measured = bytes.saturating_sub(self.last_bytes) as f64 / elapsed;
            self.speed = if self.control.is_paused() {
                0.0
            } else {
                measured
            };
            self.last_bytes = bytes;
            self.last_sample = now;
        }
    }
}

pub struct DrifterApp {
    context: egui::Context,
    runtime: tokio::runtime::Runtime,
    downloader: Downloader,
    url: String,
    folder: String,
    connections: usize,
    downloads: Vec<Download>,
    error: String,
    filter: usize,
}

impl DrifterApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        runtime: tokio::runtime::Runtime,
        downloader: Downloader,
    ) -> Self {
        let ctx = &cc.egui_ctx;
        let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BACKGROUND;
        style.visuals.window_fill = SURFACE;
        style.visuals.override_text_color = Some(TEXT);
        style.visuals.extreme_bg_color = BACKGROUND;
        style.visuals.faint_bg_color = SURFACE;
        style.visuals.selection.bg_fill = Color32::from_rgb(96, 65, 43);
        style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(43, 42, 45);
        style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(43, 42, 45);
        style.visuals.widgets.inactive.bg_stroke = Stroke::NONE;
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(61, 55, 52);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(81, 61, 47);
        style.spacing.item_spacing = egui::vec2(12.0, 12.0);
        style.spacing.button_padding = egui::vec2(16.0, 10.0);
        style.spacing.interact_size.y = 36.0;
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(14.0));
        ctx.set_style_of(egui::Theme::Dark, style);
        ctx.set_theme(egui::Theme::Dark);
        if let Some(state) = &cc.wgpu_render_state {
            eprintln!("Drifter renderer: {:?}", state.adapter.get_info());
        }
        Self {
            context: ctx.clone(),
            runtime,
            downloader,
            url: String::new(),
            folder: default_folder().display().to_string(),
            connections: 8,
            downloads: Vec::new(),
            error: String::new(),
            filter: 0,
        }
    }

    fn launch(&mut self, url: String, folder: PathBuf, connections: usize) {
        let progress = Arc::new(Progress::default());
        let control = Control::default();
        let engine = self.downloader.clone();
        let (worker_url, worker_folder, worker_progress, worker_control) = (
            url.clone(),
            folder.clone(),
            progress.clone(),
            control.clone(),
        );
        let context = self.context.clone();
        let task = self.runtime.spawn(async move {
            engine
                .download(
                    &worker_url,
                    &worker_folder,
                    connections,
                    worker_progress,
                    worker_control,
                )
                .await;
            context.request_repaint();
        });
        self.downloads.insert(
            0,
            Download {
                url,
                folder,
                requested: connections,
                progress,
                control,
                task,
                last_sample: Instant::now(),
                last_bytes: 0,
                speed: 0.0,
            },
        );
    }

    fn add(&mut self) {
        self.error.clear();
        if let Err(error) = validate_url(&self.url) {
            self.error = error.to_string();
            return;
        }
        if self.folder.trim().is_empty() {
            self.error = "Choose a download folder".into();
            return;
        }
        if self.downloads.iter().filter(|job| job.active()).count() >= 4 {
            self.error =
                "Four downloads are already active. Finish or cancel a transfer first.".into();
            return;
        }
        let url = self.url.trim().to_string();
        self.launch(url, PathBuf::from(self.folder.trim()), self.connections);
        self.url.clear();
        self.filter = 0;
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            download_mark(ui, 35.0, false, ACCENT);
            ui.vertical(|ui| {
                ui.label(RichText::new("drifter").size(30.0).strong());
                ui.label(RichText::new("A faster way down.").color(MUTED));
            });
        });
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("New download").size(19.0).strong());
            ui.add_space(6.0);
            let input = ui.add_sized(
                [ui.available_width(), 44.0],
                egui::TextEdit::singleline(&mut self.url)
                    .hint_text("Paste a file link…")
                    .margin(egui::vec2(12.0, 12.0)),
            );
            let submit = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Save to").color(MUTED));
                let width = (ui.available_width() - 115.0).max(80.0);
                ui.add_sized(
                    [width, 36.0],
                    egui::TextEdit::singleline(&mut self.folder).margin(egui::vec2(10.0, 9.0)),
                );
                if ui.button("Browse").clicked()
                    && let Some(folder) = rfd::FileDialog::new()
                        .set_directory(self.folder.trim())
                        .pick_folder()
                {
                    self.folder = folder.display().to_string();
                }
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Connections").color(MUTED));
                ui.add(
                    egui::Slider::new(&mut self.connections, 1..=32)
                        .clamping(egui::SliderClamping::Always),
                );
                for count in [1, 4, 8, 16, 32] {
                    if ui
                        .selectable_label(self.connections == count, count.to_string())
                        .clicked()
                    {
                        self.connections = count;
                    }
                }
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Per new download · server limits apply")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button = egui::Button::new(
                        RichText::new("Start download").color(BACKGROUND).strong(),
                    )
                    .fill(ACCENT)
                    .min_size(egui::vec2(180.0, 42.0))
                    .corner_radius(10);
                    if ui.add(button).clicked() || submit {
                        self.add();
                    }
                });
            });
            if !self.error.is_empty() {
                ui.colored_label(Color32::from_rgb(242, 151, 138), &self.error);
            }
        });
    }

    fn downloads(&mut self, ui: &mut egui::Ui) {
        let active = self.downloads.iter().filter(|job| job.active()).count();
        let completed = self
            .downloads
            .iter()
            .filter(|job| matches!(job.progress.details.lock().unwrap().0, Phase::Complete(_)))
            .count();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Transfers").size(20.0).strong());
            for (index, name) in [
                format!("All  {}", self.downloads.len()),
                format!("Active  {active}"),
                format!("Done  {completed}"),
            ]
            .iter()
            .enumerate()
            {
                if ui.selectable_label(self.filter == index, name).clicked() {
                    self.filter = index;
                }
            }
            if self.downloads.iter().any(|job| !job.active())
                && ui.button("Clear finished").clicked()
            {
                self.downloads.retain(|job| job.active());
            }
        });
        ui.add_space(8.0);
        let mut retry = None;
        let mut visible = 0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, job) in self.downloads.iter_mut().enumerate() {
                    job.sample();
                    let (phase, name, note) = job.progress.details.lock().unwrap().clone();
                    if self.filter == 1 && !job.active()
                        || self.filter == 2 && !matches!(phase, Phase::Complete(_))
                    {
                        continue;
                    }
                    visible += 1;
                    ui.push_id(index, |ui| {
                        card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let paused = job.control.is_paused();
                            let bytes = job.progress.bytes.load(Ordering::Relaxed);
                            let total = job.progress.total.load(Ordering::Relaxed);
                            let count = job.progress.connections.load(Ordering::Relaxed);
                            ui.horizontal(|ui| {
                                let color = if matches!(phase, Phase::Failed(_)) {
                                    Color32::from_rgb(242, 151, 138)
                                } else {
                                    ACCENT
                                };
                                download_mark(ui, 25.0, matches!(phase, Phase::Complete(_)), color);
                                ui.add(
                                    egui::Label::new(RichText::new(&name).size(17.0).strong())
                                        .truncate(),
                                );
                            });
                            ui.add(
                                egui::Label::new(
                                    RichText::new(display_url(&job.url)).size(12.0).color(MUTED),
                                )
                                .truncate(),
                            );
                            let fraction = if matches!(phase, Phase::Complete(_)) {
                                1.0
                            } else if total > 0 {
                                (bytes as f64 / total as f64).clamp(0.0, 1.0) as f32
                            } else {
                                0.0
                            };
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_height(5.0)
                                    .fill(ACCENT)
                                    .animate(matches!(phase, Phase::Connecting) && !paused),
                            );
                            ui.horizontal_wrapped(|ui| {
                                let status = match &phase {
                                    Phase::Connecting => {
                                        if paused { "Paused" } else { "Connecting…" }.to_string()
                                    }
                                    Phase::Downloading if paused => "Paused".into(),
                                    Phase::Downloading => format!("{}/s", human_bytes(job.speed)),
                                    Phase::Complete(_) => "Complete".into(),
                                    Phase::Failed(_) => "Failed".into(),
                                    Phase::Cancelled => "Cancelled".into(),
                                };
                                ui.label(RichText::new(status).color(ACCENT));
                                ui.label(
                                    RichText::new(if total > 0 {
                                        format!(
                                            "{} / {}",
                                            human_bytes(bytes as f64),
                                            human_bytes(total as f64)
                                        )
                                    } else {
                                        human_bytes(bytes as f64)
                                    })
                                    .color(MUTED)
                                    .size(12.0),
                                );
                                if matches!(phase, Phase::Downloading) {
                                    ui.label(
                                        RichText::new(format!(
                                            "{count} / {} connections",
                                            job.requested
                                        ))
                                        .size(12.0)
                                        .color(MUTED),
                                    );
                                    if job.speed > 0.0 && total > bytes && !paused {
                                        ui.label(
                                            RichText::new(eta((total - bytes) as f64 / job.speed))
                                                .size(12.0)
                                                .color(MUTED),
                                        );
                                    }
                                }
                                if job.active() {
                                    if ui
                                        .small_button(if paused { "Resume" } else { "Pause" })
                                        .clicked()
                                    {
                                        job.control.pause(!paused);
                                    }
                                    if ui.small_button("Cancel").clicked() {
                                        job.control.cancel.cancel();
                                    }
                                } else if let Phase::Complete(path) = &phase {
                                    if ui.small_button("Show file").clicked() {
                                        reveal(path);
                                    }
                                } else if ui.small_button("Retry").clicked() {
                                    retry = Some((job.url.clone(), job.folder.clone(), index));
                                }
                            });
                            match &phase {
                                Phase::Failed(error) => {
                                    ui.colored_label(Color32::from_rgb(242, 151, 138), error);
                                }
                                Phase::Downloading if !note.is_empty() => {
                                    ui.label(RichText::new(note).size(11.0).color(MUTED));
                                }
                                _ => {}
                            }
                        });
                    });
                    ui.add_space(10.0);
                }
                if visible == 0 {
                    ui.add_space(28.0);
                    ui.vertical_centered(|ui| {
                        download_mark(ui, 44.0, false, ACCENT);
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(if self.downloads.is_empty() {
                                "Your next download starts here."
                            } else {
                                "No transfers in this view."
                            })
                            .size(18.0),
                        );
                        ui.label(
                            RichText::new("Direct links · parallel connections · a quieter wait")
                                .size(13.0)
                                .color(MUTED),
                        );
                    });
                }
            });
        if let Some((url, folder, index)) = retry {
            if active >= 4 {
                self.error =
                    "Four downloads are already active. Finish or cancel one first.".into();
            } else {
                self.downloads.remove(index);
                self.launch(url, folder, self.connections);
            }
        }
    }
}

impl eframe::App for DrifterApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BACKGROUND.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(
                if ui.available_width() < 750.0 { 22 } else { 40 },
                28,
            ))
            .show(ui, |ui| {
                self.header(ui);
                ui.add_space(24.0);
                self.composer(ui);
                ui.add_space(24.0);
                self.downloads(ui);
            });
        if self.downloads.iter().any(|job| job.active()) {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl Drop for DrifterApp {
    fn drop(&mut self) {
        for job in &self.downloads {
            job.control.cancel.cancel();
        }
        self.runtime.block_on(async {
            for job in &mut self.downloads {
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut job.task).await;
            }
        });
    }
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .corner_radius(16)
        .inner_margin(22)
}

fn human_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{bytes:.0} B")
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1} KiB", bytes / 1024.0)
    } else if bytes < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MiB", bytes / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", bytes / (1024.0 * 1024.0 * 1024.0))
    }
}

fn eta(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.0}s left")
    } else if seconds < 3600.0 {
        format!("{:.0}m left", seconds / 60.0)
    } else {
        format!("{:.1}h left", seconds / 3600.0)
    }
}

fn display_url(input: &str) -> String {
    if let Ok(mut url) = url::Url::parse(input) {
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    } else {
        input.into()
    }
}

fn reveal(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if let Some(parent) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
    }
}

fn download_mark(ui: &mut egui::Ui, size: f32, complete: bool, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let center = rect.center();
    let stroke = Stroke::new((size / 13.0).max(1.8), color);
    if complete {
        ui.painter().line_segment(
            [
                center + egui::vec2(-size * 0.27, 0.0),
                center + egui::vec2(-size * 0.06, size * 0.23),
            ],
            stroke,
        );
        ui.painter().line_segment(
            [
                center + egui::vec2(-size * 0.06, size * 0.23),
                center + egui::vec2(size * 0.32, -size * 0.24),
            ],
            stroke,
        );
    } else {
        ui.painter().arrow(
            center - egui::vec2(size * 0.26, size * 0.26),
            egui::vec2(size * 0.52, size * 0.52),
            stroke,
        );
    }
}
