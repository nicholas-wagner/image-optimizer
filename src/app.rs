use eframe::egui;

use crate::analyze::{self, ImageInfo};
use crate::execute::{self, CancelFlag, OptimizeOutcome};
use crate::files::{self, FilePrompt};
use crate::model::{self, Decision, Recommendation, RowChoice};
use crate::settings::Settings;
use crate::tools::{self, ToolPaths};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

enum PickerPurpose {
    Images,
    Output,
}

enum PickerOutcome {
    Stay,
    Close,
    Add(Vec<PathBuf>),
    Output(PathBuf),
}

struct DirPicker {
    purpose: PickerPurpose,
    current: PathBuf,
    entries: Vec<files::DirItem>,
    selected: Vec<PathBuf>,
    error: String,
}

impl DirPicker {
    fn open(purpose: PickerPurpose, dir: PathBuf) -> Self {
        let mut picker = Self {
            purpose,
            current: dir,
            entries: Vec::new(),
            selected: Vec::new(),
            error: String::new(),
        };
        picker.reload();
        picker
    }

    fn reload(&mut self) {
        match files::list_dir(&self.current) {
            Ok(entries) => {
                self.entries = entries;
                self.error.clear();
            }
            Err(err) => {
                self.entries.clear();
                self.error = err;
            }
        }
        self.selected.clear();
    }

    fn enter(&mut self, dir: PathBuf) {
        self.current = dir;
        self.reload();
    }
}

struct PreviewImage {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

struct RowPatch {
    status: String,
    recommendation: Option<Recommendation>,
    output_bytes: Option<u64>,
    output_path: Option<PathBuf>,
    command: String,
    error: Option<String>,
}

enum Msg {
    Row { gen: u64, index: usize, patch: RowPatch },
    Finished { gen: u64 },
    Preview {
        path: PathBuf,
        before: Option<PreviewImage>,
        after: Option<PreviewImage>,
    },
}

struct JobItem {
    index: usize,
    path: PathBuf,
    info: ImageInfo,
    choice: RowChoice,
    recommendation: Option<Recommendation>,
    choose_best: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepKind {
    NeedTools,
    Add,
    Optimize,
    Working,
    Open,
}

fn step_kind(tools_ok: bool, files: usize, pending: usize, busy: bool) -> StepKind {
    if !tools_ok {
        StepKind::NeedTools
    } else if busy {
        StepKind::Working
    } else if files == 0 {
        StepKind::Add
    } else if pending > 0 {
        StepKind::Optimize
    } else {
        StepKind::Open
    }
}

pub struct OptimizerApp {
    settings: Settings,
    rows: Vec<FileRow>,
    selected: Option<usize>,
    checked: HashSet<usize>,
    tools: Result<ToolPaths, String>,
    log: String,
    path_entry: String,
    running: bool,
    job_gen: u64,
    job_done: usize,
    job_total: usize,
    cancel: Option<CancelFlag>,
    rx: Option<Receiver<Msg>>,
    tx: Sender<Msg>,
    show_licenses: bool,
    picker: Option<DirPicker>,
    preview_path: Option<PathBuf>,
    preview_output: Option<PathBuf>,
    before_tex: Option<egui::TextureHandle>,
    after_tex: Option<egui::TextureHandle>,
}

struct FileRow {
    path: PathBuf,
    info: Result<ImageInfo, String>,
    choice: RowChoice,
    recommendation: Option<Recommendation>,
    output_bytes: Option<u64>,
    output_path: Option<PathBuf>,
    status: String,
    command: String,
}

impl OptimizerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = cc
            .storage
            .and_then(|storage| storage.get_string("settings"))
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let tools = tools::locate();
        let log = match &tools {
            Ok(_) => String::new(),
            Err(err) => err.clone(),
        };
        apply_theme(&cc.egui_ctx);
        let mut app = Self {
            settings,
            rows: Vec::new(),
            selected: None,
            checked: HashSet::new(),
            tools,
            log,
            path_entry: String::new(),
            running: false,
            job_gen: 0,
            job_done: 0,
            job_total: 0,
            cancel: None,
            rx: Some(rx),
            tx,
            show_licenses: false,
            picker: None,
            preview_path: None,
            preview_output: None,
            before_tex: None,
            after_tex: None,
        };
        if std::env::var_os("IMAGE_OPTIMIZER_PICKER").is_some() {
            app.open_picker(PickerPurpose::Images);
        }
        if let Ok(path) = std::env::var("IMAGE_OPTIMIZER_ADD") {
            app.add_path(PathBuf::from(path));
        }
        app
    }

    fn open_picker(&mut self, purpose: PickerPurpose) {
        let dir = self
            .picker
            .as_ref()
            .map(|picker| picker.current.clone())
            .unwrap_or_else(files::starting_dir);
        self.picker = Some(DirPicker::open(purpose, dir));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else {
            return;
        };
        let mut finished = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Row { gen, index, patch } => {
                    if gen == self.job_gen {
                        self.job_done += 1;
                        if let Some(row) = self.rows.get_mut(index) {
                            row.status = patch.status;
                            if patch.recommendation.is_some() {
                                row.recommendation = patch.recommendation;
                            }
                            if patch.output_bytes.is_some() {
                                row.output_bytes = patch.output_bytes;
                            }
                            if patch.output_path.is_some() {
                                row.output_path = patch.output_path;
                            }
                            if !patch.command.is_empty() {
                                row.command = patch.command;
                            }
                            if let Some(error) = patch.error {
                                row.status = error;
                            }
                        }
                    }
                }
                Msg::Finished { gen } => {
                    if gen == self.job_gen {
                        finished = true;
                    }
                }
                Msg::Preview { path, before, after } => {
                    if self.preview_path.as_ref() == Some(&path) {
                        self.before_tex = before.map(|img| texture(ctx, "before", &img));
                        self.after_tex = after.map(|img| texture(ctx, "after", &img));
                    }
                }
            }
        }
        if finished {
            self.running = false;
            let cancelled = self
                .cancel
                .as_ref()
                .map(|flag| flag.is_cancelled())
                .unwrap_or(false);
            self.cancel = None;
            if cancelled {
                self.log = "Stopped. You can optimize again.".to_string();
            } else {
                self.log.clear();
            }
        }
        if self.running {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    fn intake_drops(&mut self, ctx: &egui::Context) {
        if self.running {
            return;
        }
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        for path in dropped {
            self.add_path(path);
        }
    }

    fn add_path(&mut self, path: PathBuf) {
        let found = files::collect_images(&path);
        if found.is_empty() && path.is_file() {
            self.push_row(path);
            return;
        }
        for file in found {
            if self.rows.len() >= files::MAX_IMAGES {
                self.log = format!("Stopped at {} files.", files::MAX_IMAGES);
                break;
            }
            self.push_row(file);
        }
    }

    fn push_row(&mut self, path: PathBuf) {
        if self.rows.iter().any(|row| row.path == path) {
            return;
        }
        let info = analyze::analyze(&path);
        let status = match &info {
            Ok(_) => "Ready".to_string(),
            Err(err) => err.clone(),
        };
        self.rows.push(FileRow {
            path,
            info,
            choice: RowChoice::Recommended,
            recommendation: None,
            output_bytes: None,
            output_path: None,
            status,
            command: String::new(),
        });
    }

    fn output_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.settings.output_dir {
            return Some(dir.clone());
        }
        self.rows.iter().find_map(|row| {
            row.info.as_ref().ok()?;
            row.path.parent().map(|parent| parent.join("optimized"))
        })
    }

    fn start_job(&mut self) {
        if self.running {
            return;
        }
        let Ok(tools) = self.tools.clone() else {
            self.log = "The optimizer tools are not available.".to_string();
            return;
        };
        let Some(output_dir) = self.output_dir() else {
            self.log = "Add an image first.".to_string();
            return;
        };
        self.settings.clamp();
        let mut items = Vec::new();
        for (index, row) in self.rows.iter_mut().enumerate() {
            let Ok(info) = row.info.clone() else {
                continue;
            };
            row.output_bytes = None;
            row.output_path = None;
            row.status = "Queued".to_string();
            items.push(JobItem {
                index,
                path: row.path.clone(),
                info,
                choice: row.choice,
                recommendation: row.recommendation.clone(),
                choose_best: self.settings.choose_best_format,
            });
        }
        if items.is_empty() {
            self.log = "No supported images to process.".to_string();
            return;
        }
        self.job_gen += 1;
        let gen = self.job_gen;
        self.job_done = 0;
        self.job_total = items.len();
        self.running = true;
        let cancel = CancelFlag::new();
        self.cancel = Some(cancel.clone());
        let tx = self.tx.clone();
        let settings = self.settings.clone();
        thread::spawn(move || run_job(gen, items, tools, settings, output_dir, cancel, tx));
    }

    fn counts(&self) -> (usize, usize) {
        let files = self.rows.iter().filter(|row| row.info.is_ok()).count();
        let pending = self
            .rows
            .iter()
            .filter(|row| {
                row.info.is_ok() && row.output_path.is_none() && row.choice != RowChoice::Skip
            })
            .count();
        (files, pending)
    }

    fn saved_line(&self) -> Option<String> {
        let mut before = 0u64;
        let mut after = 0u64;
        let mut n = 0u64;
        for row in &self.rows {
            let Some(info) = row.info.as_ref().ok() else {
                continue;
            };
            let Some(output) = row.output_bytes else {
                continue;
            };
            before += info.bytes;
            after += output;
            n += 1;
        }
        if n == 0 {
            return None;
        }
        Some(format!(
            "Saved {:.0}% ({}) across {}.",
            model::saved_percent(before, after),
            model::format_bytes(before.saturating_sub(after)),
            plural(n as usize, "image", "images")
        ))
    }

    fn open_output(&mut self) {
        if let Some(dir) = self.output_dir() {
            let _ = std::fs::create_dir_all(&dir);
            open_dir(&dir);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.heading("Image Optimizer");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Licenses").clicked() {
                    self.show_licenses = true;
                }
            });
        });

        let (files, pending) = self.counts();
        let kind = step_kind(self.tools.is_ok(), files, pending, self.running);
        let hovering = ctx.input(|input| !input.raw.hovered_files.is_empty());

        ui.add_space(8.0);
        let border = if hovering { accent() } else { card_stroke() };
        egui::Frame::NONE
            .fill(card_fill())
            .stroke(egui::Stroke::new(1.5_f32, border))
            .inner_margin(egui::Margin::same(20))
            .corner_radius(egui::CornerRadius::same(10))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(step_title(kind, files))
                            .size(24.0)
                            .strong()
                            .color(ink_text()),
                    );
                    let detail = if hovering {
                        "Release to add these files."
                    } else {
                        step_detail(kind)
                    };
                    if !detail.is_empty() {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(detail)
                                .size(16.0)
                                .color(body_text()),
                        );
                    }
                    if let Some(saved) = self.saved_line() {
                        if matches!(kind, StepKind::Open) {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(saved)
                                    .size(16.0)
                                    .color(body_text()),
                            );
                        }
                    }
                    if !self.log.is_empty() {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&self.log)
                                .size(16.0)
                                .color(egui::Color32::from_rgb(153, 27, 27)),
                        );
                    }
                    ui.add_space(14.0);
                    let label = step_action(kind);
                    if !label.is_empty() && primary_button(ui, label).clicked() {
                        self.act(kind);
                    }
                    ui.add_space(8.0);
                });
            });

        if self.running {
            ui.add_space(8.0);
            let frac = if self.job_total == 0 {
                0.0
            } else {
                self.job_done as f32 / self.job_total as f32
            };
            ui.add(
                egui::ProgressBar::new(frac).text(format!("{} of {}", self.job_done, self.job_total)),
            );
        }

        if files > 0 {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Add more").clicked() && !self.running {
                    self.prompt_images();
                }
                if ui.button("Clear").clicked() && !self.running {
                    self.clear_rows();
                }
            });
            if !self.checked.is_empty() && !self.running {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{} selected", self.checked.len()));
                    for (label, choice) in [
                        ("Best", RowChoice::Recommended),
                        ("JPEG", RowChoice::Jpeg),
                        ("PNG", RowChoice::Png),
                        ("WebP", RowChoice::Webp),
                    ] {
                        if ui.button(label).clicked() {
                            let indexes: Vec<usize> = self.checked.iter().copied().collect();
                            for index in indexes {
                                self.set_choice(index, choice);
                            }
                        }
                    }
                });
            }
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .show(ui, |ui| {
                    self.table(ui);
                });
            self.preview_ui(ui, ctx);
        }

        ui.add_space(8.0);
        egui::CollapsingHeader::new("Options")
            .default_open(false)
            .show(ui, |ui| {
                self.settings_ui(ui);
            });

        if self.show_licenses {
            egui::Window::new("Licenses")
                .open(&mut self.show_licenses)
                .default_width(520.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        ui.label(crate::licenses::TEXT);
                    });
                });
        }
    }

    fn act(&mut self, kind: StepKind) -> bool {
        match kind {
            StepKind::Add => self.prompt_images(),
            StepKind::Optimize => self.start_job(),
            StepKind::Working => {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
            }
            StepKind::Open => self.open_output(),
            StepKind::NeedTools => {}
        }
        true
    }

    fn clear_rows(&mut self) {
        self.rows.clear();
        self.selected = None;
        self.checked.clear();
        self.before_tex = None;
        self.after_tex = None;
        self.preview_path = None;
        self.preview_output = None;
        self.log.clear();
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(
            &mut self.settings.choose_best_format,
            "Best uses a smaller format when it is clearly better",
        );
        ui.add_enabled(
            !self.settings.jpeg_lossless,
            egui::Slider::new(&mut self.settings.jpeg_quality, 0..=100).text("JPEG max quality"),
        );
        ui.checkbox(&mut self.settings.jpeg_lossless, "Lossless JPEG");
        ui.add_enabled(
            !self.settings.webp_lossless,
            egui::Slider::new(&mut self.settings.webp_quality, 0..=100).text("WebP quality"),
        );
        ui.checkbox(&mut self.settings.webp_lossless, "Lossless WebP");
        ui.checkbox(&mut self.settings.strip_metadata, "Strip metadata");
        ui.checkbox(
            &mut self.settings.png_brute,
            "PNG maximum compression (slow)",
        );
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.settings.resize_enabled, "Resize");
            ui.add_enabled(
                self.settings.resize_enabled,
                egui::DragValue::new(&mut self.settings.resize_max_edge)
                    .range(1..=20000)
                    .suffix(" px longest side"),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Output:");
            let text = self
                .settings
                .output_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "optimized folder next to the first image".to_string());
            ui.label(text);
            if ui.button("Change").clicked() {
                self.prompt_output();
            }
            if self.settings.output_dir.is_some() && ui.button("Reset").clicked() {
                self.settings.output_dir = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Path:");
            let entry = ui.add(
                egui::TextEdit::singleline(&mut self.path_entry).desired_width(320.0),
            );
            if (entry.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui.button("Add").clicked()
            {
                let path = PathBuf::from(self.path_entry.trim());
                if !path.as_os_str().is_empty() {
                    self.add_path(path);
                    self.path_entry.clear();
                }
            }
        });
        ui.label("JPEG max quality only recompresses files saved above that value.");
    }

    fn set_choice(&mut self, index: usize, choice: RowChoice) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        if row.choice == choice {
            return;
        }
        row.choice = choice;
        row.output_bytes = None;
        row.output_path = None;
        row.recommendation = None;
        row.status = "Ready".to_string();
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("files")
            .striped(true)
            .num_columns(5)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.strong("");
                ui.strong("File");
                ui.strong("Size");
                ui.strong("Format");
                ui.strong("Result");
                ui.end_row();

                for index in 0..self.rows.len() {
                    let mut on = self.checked.contains(&index);
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            self.checked.insert(index);
                        } else {
                            self.checked.remove(&index);
                        }
                        self.selected = Some(index);
                    }
                    let name = self.rows[index]
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| self.rows[index].path.display().to_string());
                    ui.label(name);
                    let bytes = self.rows[index]
                        .info
                        .as_ref()
                        .map(|info| model::format_bytes(info.bytes))
                        .unwrap_or_else(|_| "—".to_string());
                    ui.label(bytes);

                    let mut choice = self.rows[index].choice;
                    let previous = choice;
                    egui::ComboBox::from_id_salt(index)
                        .selected_text(choice.label())
                        .show_ui(ui, |ui| {
                            for option in RowChoice::ALL {
                                ui.selectable_value(&mut choice, option, option.label());
                            }
                        });
                    if choice != previous {
                        self.set_choice(index, choice);
                    }

                    ui.label(row_result(&self.rows[index]));
                    ui.end_row();
                }
            });
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(index) = self.selected else {
            return;
        };
        let Some(row) = self.rows.get(index) else {
            return;
        };
        if row.info.is_err() || row.output_path.is_none() {
            return;
        }
        let path = row.path.clone();
        let output = row.output_path.clone();
        if self.preview_path.as_ref() != Some(&path) || self.preview_output != output {
            self.preview_path = Some(path.clone());
            self.preview_output = output.clone();
            self.before_tex = None;
            self.after_tex = None;
            let tx = self.tx.clone();
            let preview_output = output.clone();
            thread::spawn(move || {
                let before = load_preview(&path);
                let after = preview_output.as_deref().and_then(load_preview);
                let _ = tx.send(Msg::Preview { path, before, after });
            });
            ctx.request_repaint();
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label("Original");
                if let Some(tex) = &self.before_tex {
                    ui.image(tex);
                } else {
                    ui.label("Loading…");
                }
            });
            ui.vertical(|ui| {
                ui.label("Output");
                if let Some(tex) = &self.after_tex {
                    ui.image(tex);
                } else if output.is_some() {
                    ui.label("Loading…");
                } else {
                    ui.label("Optimize to compare.");
                }
            });
        });
        if let Some(rec) = &self.rows[index].recommendation {
            ui.label(rec.summary());
        }
    }
}

impl eframe::App for OptimizerApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.clamp();
        if let Ok(json) = serde_json::to_string(&self.settings) {
            storage.set_string("settings", json);
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        self.intake_drops(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.ui(ui, ctx);
            });
        });
        self.show_picker(ctx);
    }
}

impl OptimizerApp {
    fn prompt_images(&mut self) {
        match files::file_prompt() {
            FilePrompt::InApp => self.open_picker(PickerPurpose::Images),
            FilePrompt::Native => self.add_native(NativePick::Files),
        }
    }

    fn prompt_output(&mut self) {
        match files::file_prompt() {
            FilePrompt::InApp => self.open_picker(PickerPurpose::Output),
            FilePrompt::Native => self.add_native(NativePick::Output),
        }
    }

    #[cfg(windows)]
    fn add_native(&mut self, kind: NativePick) {
        let picked = std::panic::catch_unwind(|| match kind {
            NativePick::Files => rfd::FileDialog::new()
                .add_filter("Images", &["jpg", "jpeg", "png", "webp"])
                .pick_files()
                .map(NativeResult::Files),
            NativePick::Output => rfd::FileDialog::new()
                .pick_folder()
                .map(NativeResult::Output),
        });
        match picked {
            Ok(Some(NativeResult::Files(paths))) => {
                for path in paths {
                    self.add_path(path);
                }
            }
            Ok(Some(NativeResult::Output(path))) => {
                self.settings.output_dir = Some(path);
            }
            Ok(None) => {}
            Err(_) => {
                self.log = "The file dialog failed. Type a path instead.".to_string();
            }
        }
    }

    #[cfg(not(windows))]
    fn add_native(&mut self, _kind: NativePick) {
        self.open_picker(PickerPurpose::Images);
    }

    fn show_picker(&mut self, ctx: &egui::Context) {
        let Some(mut picker) = self.picker.take() else {
            return;
        };
        let mut outcome = PickerOutcome::Stay;
        let title = match picker.purpose {
            PickerPurpose::Images => "Add images",
            PickerPurpose::Output => "Output folder",
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                outcome = picker_ui(ui, &mut picker);
            });
        let close = !matches!(outcome, PickerOutcome::Stay);
        if !close {
            self.picker = Some(picker);
        }
        match outcome {
            PickerOutcome::Add(paths) => {
                let count_before = self.rows.len();
                for path in paths {
                    self.add_path(path);
                }
                let added = self.rows.len() - count_before;
                self.log = format!("Added {added} image(s).");
            }
            PickerOutcome::Output(path) => {
                self.log = format!("Output folder: {}", path.display());
                self.settings.output_dir = Some(path);
            }
            PickerOutcome::Stay | PickerOutcome::Close => {}
        }
    }
}

#[derive(Clone, Copy)]
enum NativePick {
    Files,
    Output,
}

#[cfg(windows)]
enum NativeResult {
    Files(Vec<PathBuf>),
    Output(PathBuf),
}

fn picker_ui(ui: &mut egui::Ui, picker: &mut DirPicker) -> PickerOutcome {
    ui.label(picker.current.display().to_string());
    ui.horizontal_wrapped(|ui| {
        if ui.button("Up").clicked() {
            if let Some(parent) = files::parent_dir(&picker.current) {
                picker.enter(parent);
            }
        }
        for (label, path) in files::shortcuts() {
            if ui.button(label).clicked() {
                picker.enter(path);
            }
        }
    });
    if !picker.error.is_empty() {
        ui.colored_label(egui::Color32::RED, &picker.error);
    }
    let mut enter: Option<PathBuf> = None;
    let mut toggle: Option<PathBuf> = None;
    egui::ScrollArea::vertical()
        .max_height(320.0)
        .show(ui, |ui| {
            if picker.entries.is_empty() && picker.error.is_empty() {
                ui.label("No images in this folder.");
            }
            for item in &picker.entries {
                let label = if item.is_dir {
                    format!("{}/", item.name)
                } else {
                    item.name.clone()
                };
                let on = picker.selected.iter().any(|path| path == &item.path);
                if ui.selectable_label(on, label).clicked() {
                    if item.is_dir {
                        enter = Some(item.path.clone());
                    } else {
                        toggle = Some(item.path.clone());
                    }
                }
            }
        });
    if let Some(dir) = enter {
        picker.enter(dir);
    }
    if let Some(path) = toggle {
        if let Some(index) = picker.selected.iter().position(|item| item == &path) {
            picker.selected.remove(index);
        } else {
            picker.selected.push(path);
        }
    }
    ui.separator();
    let mut outcome = PickerOutcome::Stay;
    ui.horizontal(|ui| {
        match picker.purpose {
            PickerPurpose::Images => {
                if ui.button("Add images in this folder").clicked() {
                    outcome = PickerOutcome::Add(vec![picker.current.clone()]);
                }
                if ui
                    .add_enabled(!picker.selected.is_empty(), egui::Button::new("Add selected"))
                    .clicked()
                {
                    outcome = PickerOutcome::Add(picker.selected.clone());
                }
            }
            PickerPurpose::Output => {
                if ui.button("Use this folder").clicked() {
                    outcome = PickerOutcome::Output(picker.current.clone());
                }
            }
        }
        if ui.button("Close").clicked() {
            outcome = PickerOutcome::Close;
        }
    });
    outcome
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn step_title(kind: StepKind, files: usize) -> String {
    match kind {
        StepKind::NeedTools => "The optimizer did not start".to_string(),
        StepKind::Add => "Add the images you want to shrink".to_string(),
        StepKind::Optimize => format!("Ready to shrink {}", plural(files, "image", "images")),
        StepKind::Working => "Optimizing your images".to_string(),
        StepKind::Open => "Your images are ready".to_string(),
    }
}

fn step_detail(kind: StepKind) -> &'static str {
    match kind {
        StepKind::NeedTools => "This copy of the app is missing its tools.",
        StepKind::Add => "Drop a folder here, or use the button.",
        StepKind::Optimize => "Best is the default. Set JPEG, PNG, or WebP on a row to keep that format.",
        StepKind::Working => "",
        StepKind::Open => "",
    }
}

fn step_action(kind: StepKind) -> &'static str {
    match kind {
        StepKind::NeedTools => "",
        StepKind::Add => "Add images",
        StepKind::Optimize => "Optimize",
        StepKind::Working => "Cancel",
        StepKind::Open => "Open folder",
    }
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .size(18.0)
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(accent())
        .min_size(egui::vec2(240.0, 42.0)),
    )
}

fn accent() -> egui::Color32 {
    // Dark enough that white button text clears 4.5:1.
    egui::Color32::from_rgb(194, 65, 12)
}

fn ink_text() -> egui::Color32 {
    egui::Color32::from_rgb(17, 24, 39)
}

fn body_text() -> egui::Color32 {
    egui::Color32::from_rgb(44, 58, 78)
}

fn card_fill() -> egui::Color32 {
    egui::Color32::WHITE
}

fn card_stroke() -> egui::Color32 {
    egui::Color32::from_rgb(186, 199, 216)
}

fn page_fill() -> egui::Color32 {
    egui::Color32::from_rgb(241, 245, 249)
}

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.window_fill = page_fill();
    visuals.panel_fill = page_fill();
    visuals.faint_bg_color = egui::Color32::from_rgb(226, 232, 240);
    visuals.extreme_bg_color = card_fill();
    visuals.code_bg_color = egui::Color32::from_rgb(226, 232, 240);
    visuals.hyperlink_color = egui::Color32::from_rgb(29, 78, 216);
    visuals.selection.bg_fill = egui::Color32::from_rgb(219, 234, 254);
    visuals.override_text_color = Some(ink_text());
    visuals.window_stroke = egui::Stroke::new(1.0_f32, card_stroke());
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.fg_stroke.color = ink_text();
        widget.bg_stroke.color = card_stroke();
    }
    visuals.widgets.noninteractive.bg_fill = page_fill();
    visuals.widgets.inactive.bg_fill = card_fill();
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(226, 232, 240);
    visuals.widgets.active.bg_fill = accent();
    visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
    visuals.widgets.open.bg_fill = egui::Color32::from_rgb(226, 232, 240);
    ctx.set_visuals(visuals);
}

fn row_result(row: &FileRow) -> String {
    if row.info.is_err() {
        return row.status.clone();
    }
    if let Some(bytes) = row.output_bytes {
        let saved = row
            .info
            .as_ref()
            .ok()
            .map(|info| format!(" · saved {:.0}%", model::saved_percent(info.bytes, bytes)))
            .unwrap_or_default();
        return format!("{}{saved}", model::format_bytes(bytes));
    }
    if row.status != "Ready" && !row.status.is_empty() {
        return row.status.clone();
    }
    "—".to_string()
}

fn run_job(
    gen: u64,
    items: Vec<JobItem>,
    tools: ToolPaths,
    settings: Settings,
    output_dir: PathBuf,
    cancel: CancelFlag,
    tx: Sender<Msg>,
) {
    let total = items.len();
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let items = std::sync::Arc::new(items);
    let workers = worker_count(total);
    let mut handles = Vec::new();
    for _ in 0..workers {
        let next = next.clone();
        let items = items.clone();
        let tools = tools.clone();
        let settings = settings.clone();
        let output_dir = output_dir.clone();
        let cancel = cancel.clone();
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let cursor = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if cursor >= total {
                    break;
                }
                let item = &items[cursor];
                let patch = optimize_item(&tools, &settings, item, &output_dir, &cancel);
                let _ = tx.send(Msg::Row {
                    gen,
                    index: item.index,
                    patch,
                });
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    let _ = tx.send(Msg::Finished { gen });
}

fn worker_count(jobs: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (cores / 2).clamp(1, 4).min(jobs.max(1))
}

fn optimize_item(
    tools: &ToolPaths,
    settings: &Settings,
    item: &JobItem,
    output_dir: &Path,
    cancel: &CancelFlag,
) -> RowPatch {
    if item.choose_best && item.choice == RowChoice::Recommended && item.recommendation.is_none() {
        let rec = match execute::recommend_file(tools, settings, &item.path, &item.info, cancel) {
            Ok(rec) => rec,
            Err(err) => return error_patch(err),
        };
        let encode = match rec.decision {
            Decision::Keep => {
                return RowPatch {
                    status: "Already small enough".to_string(),
                    recommendation: Some(rec),
                    output_bytes: None,
                    output_path: None,
                    command: String::new(),
                    error: None,
                };
            }
            Decision::Use(encode) => encode,
        };
        let mut patch = match execute::optimize_file(
            tools,
            settings,
            &item.path,
            &item.info,
            encode,
            output_dir,
            cancel,
        ) {
            Ok(outcome) => done_patch(outcome),
            Err(err) => return error_patch(err),
        };
        patch.status = encode.label().to_string();
        patch.recommendation = Some(rec);
        return patch;
    }
    let encode = execute::effective_encode(
        item.choice,
        item.recommendation.as_ref(),
        item.info.format,
        item.info.has_alpha,
        settings.webp_lossless,
    );
    let encode = match encode {
        Ok(Some(encode)) => encode,
        Ok(None) => {
            let status = if item.choice == RowChoice::Skip {
                "Skipped"
            } else {
                "Kept original"
            };
            return RowPatch {
                status: status.to_string(),
                recommendation: None,
                output_bytes: None,
                output_path: None,
                command: String::new(),
                error: None,
            };
        }
        Err(err) => return error_patch(err),
    };
    match execute::optimize_file(
        tools,
        settings,
        &item.path,
        &item.info,
        encode,
        output_dir,
        cancel,
    ) {
        Ok(outcome) => done_patch(outcome),
        Err(err) => error_patch(err),
    }
}

fn done_patch(outcome: OptimizeOutcome) -> RowPatch {
    let saved_note = if outcome.note.is_empty() {
        "Done".to_string()
    } else {
        format!("Done ({})", outcome.note)
    };
    RowPatch {
        status: saved_note,
        recommendation: None,
        output_bytes: Some(outcome.output_bytes),
        output_path: Some(outcome.output_path),
        command: outcome.commands.join(" && "),
        error: None,
    }
}

fn error_patch(err: String) -> RowPatch {
    RowPatch {
        status: err.clone(),
        recommendation: None,
        output_bytes: None,
        output_path: None,
        command: String::new(),
        error: Some(err),
    }
}

fn texture(ctx: &egui::Context, name: &str, image: &PreviewImage) -> egui::TextureHandle {
    let color = egui::ColorImage::from_rgba_unmultiplied([image.width, image.height], &image.rgba);
    ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
}

fn load_preview(path: &Path) -> Option<PreviewImage> {
    let image = image::open(path).ok()?;
    let (width, height) = model::fit(image.width(), image.height(), 360);
    let image = if width != image.width() || height != image.height() {
        image.resize(width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Some(PreviewImage {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

fn open_dir(path: &Path) {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    let _ = cmd.arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_step_is_one_action() {
        assert_eq!(step_kind(true, 0, 0, false), StepKind::Add);
        assert_eq!(step_action(StepKind::Add), "Add images");

        assert_eq!(step_kind(true, 3, 3, false), StepKind::Optimize);
        assert_eq!(step_action(StepKind::Optimize), "Optimize");
        assert_eq!(step_title(StepKind::Optimize, 3), "Ready to shrink 3 images");
        assert_eq!(step_title(StepKind::Optimize, 1), "Ready to shrink 1 image");

        assert_eq!(step_kind(true, 3, 1, true), StepKind::Working);
        assert_eq!(step_action(StepKind::Working), "Cancel");

        assert_eq!(step_kind(true, 3, 0, false), StepKind::Open);
        assert_eq!(step_action(StepKind::Open), "Open folder");

        assert_eq!(step_kind(false, 0, 0, false), StepKind::NeedTools);
        assert_eq!(step_action(StepKind::NeedTools), "");
    }
}
