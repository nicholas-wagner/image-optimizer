use eframe::egui;

use crate::analyze::{self, ImageInfo};
use crate::execute::{self, CancelFlag, OptimizeOutcome};
use crate::files::{self, FilePrompt};
use crate::model::{self, Decision, Recommendation, RowChoice};
use crate::settings::{Preset, Settings};
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
    WorkingOn { gen: u64, index: usize, name: String },
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
    job_settings: Option<Settings>,
    active_file: String,
    force_options_open: bool,
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
        let mut settings = cc
            .storage
            .and_then(|storage| storage.get_string("settings"))
            .and_then(|json| serde_json::from_str::<Settings>(&json).ok())
            .unwrap_or_default();
        settings.sync_preset();
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
            job_settings: None,
            active_file: String::new(),
            force_options_open: false,
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
                Msg::WorkingOn { gen, index, name } => {
                    if gen == self.job_gen {
                        self.active_file = name;
                        if let Some(row) = self.rows.get_mut(index) {
                            if row.output_path.is_none() && row.info.is_ok() {
                                row.status = "Working".to_string();
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
            self.active_file.clear();
            let cancelled = self
                .cancel
                .as_ref()
                .map(|flag| flag.is_cancelled())
                .unwrap_or(false);
            self.cancel = None;
            if cancelled {
                for row in &mut self.rows {
                    if row.status == "Working" {
                        row.status = "Ready".to_string();
                    }
                }
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
        if self.selected.is_none() {
            self.selected = Some(self.rows.len() - 1);
        }
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
        self.settings.sync_preset();
        let stale = self.results_stale();
        let mut items = Vec::new();
        for (index, row) in self.rows.iter_mut().enumerate() {
            let Ok(info) = row.info.clone() else {
                continue;
            };
            if stale {
                row.recommendation = None;
            }
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
        self.job_settings = Some(self.settings.clone());
        self.active_file.clear();
        self.running = true;
        let cancel = CancelFlag::new();
        self.cancel = Some(cancel.clone());
        let tx = self.tx.clone();
        let settings = self.settings.clone();
        thread::spawn(move || run_job(gen, items, tools, settings, output_dir, cancel, tx));
    }

    fn counts(&self) -> (usize, usize) {
        let files = self.rows.iter().filter(|row| row.info.is_ok()).count();
        let stale = self.results_stale();
        let pending = self.rows.iter().filter(|row| needs_run(row, stale)).count();
        (files, pending)
    }

    fn results_stale(&self) -> bool {
        self.job_settings
            .as_ref()
            .is_some_and(|applied| !applied.same_job(&self.settings))
    }

    fn step_now(&self) -> StepKind {
        let (files, pending) = self.counts();
        step_kind(self.tools.is_ok(), files, pending, self.running)
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

        let files = self.counts().0;
        let mut kind = self.step_now();
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
                    ui.add_space(4.0);
                    if self.tools.is_ok() {
                        self.preset_row(ui);
                        kind = self.step_now();
                        ui.add_space(10.0);
                    }
                    let wrote = self.rows.iter().any(|row| row.output_path.is_some());
                    let all_skipped = files > 0
                        && self
                            .rows
                            .iter()
                            .filter(|row| row.info.is_ok())
                            .all(|row| row.choice == RowChoice::Skip);
                    let title = if matches!(kind, StepKind::Open) {
                        finished_title(wrote, all_skipped, files)
                    } else {
                        step_title(kind, files)
                    };
                    ui.label(
                        egui::RichText::new(title)
                            .size(24.0)
                            .strong()
                            .color(ink_text()),
                    );
                    let detail = if hovering {
                        "Release to add these files."
                    } else if matches!(kind, StepKind::Open) && !wrote {
                        if all_skipped {
                            "Skipped files stay as they are."
                        } else {
                            "Best kept the originals. Web can still shrink them."
                        }
                    } else {
                        step_detail(kind)
                    };
                    if !detail.is_empty() {
                        ui.add_space(6.0);
                        wrapped_label(ui, detail, 16.0, body_text());
                    }
                    if self.tools.is_ok() {
                        ui.add_space(4.0);
                        let done_dir = if matches!(kind, StepKind::Open) && wrote {
                            self.output_dir()
                        } else {
                            None
                        };
                        let line = if matches!(kind, StepKind::Open) && !wrote {
                            "Originals stay put.".to_string()
                        } else {
                            destination_text(done_dir.as_deref(), self.settings.output_dir.as_deref())
                        };
                        wrapped_label(ui, &line, 16.0, body_text());
                    }
                    if !self.running && self.results_stale() && self.rows.iter().any(has_previous_result)
                    {
                        ui.add_space(4.0);
                        wrapped_label(
                            ui,
                            "These results used the previous settings. Optimize again to apply the new ones.",
                            16.0,
                            warn_text(),
                        );
                    }
                    if let Some(saved) = self.saved_line() {
                        if matches!(kind, StepKind::Open) && wrote {
                            ui.add_space(4.0);
                            wrapped_label(ui, &saved, 16.0, body_text());
                        }
                    }
                    if !self.log.is_empty() {
                        ui.add_space(4.0);
                        wrapped_label(ui, &self.log, 16.0, egui::Color32::from_rgb(153, 27, 27));
                    }
                    ui.add_space(14.0);
                    let label = if matches!(kind, StepKind::Open) && !wrote {
                        ""
                    } else {
                        step_action(kind)
                    };
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
                egui::ProgressBar::new(frac).text(progress_text(
                    &self.active_file,
                    self.job_done,
                    self.job_total,
                )),
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
            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                let total = ui.available_width();
                let preview_w = (total * 0.42).clamp(240.0, 420.0);
                let list_w = (total - preview_w - 12.0).max(260.0);
                ui.vertical(|ui| {
                    ui.set_min_width(list_w);
                    ui.set_max_width(list_w);
                    ui.add_enabled_ui(!self.running, |ui| {
                        self.format_bar(ui);
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("file_list")
                        .max_height(420.0)
                        .show(ui, |ui| {
                            self.table(ui);
                        });
                });
                ui.vertical(|ui| {
                    ui.set_min_width(preview_w);
                    ui.set_max_width(preview_w);
                    self.preview_ui(ui, ctx);
                });
            });
        }

        ui.add_space(8.0);
        let mut options = egui::CollapsingHeader::new("Options").default_open(false);
        if self.force_options_open {
            options = options.open(Some(true));
            self.force_options_open = false;
        }
        options.show(ui, |ui| {
            ui.add_enabled_ui(!self.running, |ui| {
                self.settings_ui(ui);
            });
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

    fn preset_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for preset in Preset::ALL {
                let on = self.settings.preset == preset;
                let mut text = egui::RichText::new(preset.label()).size(16.0);
                if on {
                    text = text.strong().color(egui::Color32::WHITE);
                }
                let mut button = egui::Button::new(text).min_size(egui::vec2(112.0, 36.0));
                if on {
                    button = button.fill(accent());
                }
                if ui.add_enabled(!self.running, button).clicked() {
                    self.pick_preset(preset);
                }
            }
        });
        ui.add_space(4.0);
        wrapped_label(ui, self.settings.preset.detail(), 15.0, body_text());
    }

    fn pick_preset(&mut self, preset: Preset) {
        if preset == Preset::Custom {
            self.force_options_open = true;
            return;
        }
        self.settings.apply_preset(preset);
    }

    fn format_targets(&self) -> Vec<usize> {
        if !self.checked.is_empty() {
            let mut indexes: Vec<usize> = self.checked.iter().copied().collect();
            indexes.sort_unstable();
            indexes
        } else if let Some(index) = self.selected {
            vec![index]
        } else {
            Vec::new()
        }
    }

    fn format_bar(&mut self, ui: &mut egui::Ui) {
        let targets = self.format_targets();
        if targets.is_empty() {
            return;
        }
        let title = if !self.checked.is_empty() {
            format!("Format for {} selected", self.checked.len())
        } else {
            format!(
                "Format for {}",
                self.rows
                    .get(targets[0])
                    .map(|row| file_label(&row.path))
                    .unwrap_or_default()
            )
        };
        let shared = shared_choice(&self.rows, &targets);
        ui.horizontal_wrapped(|ui| {
            ui.label(title);
            for choice in RowChoice::ALL {
                if ui.selectable_label(shared == Some(choice), choice.label()).clicked() {
                    for index in targets.clone() {
                        self.set_choice(index, choice);
                    }
                }
            }
        });
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
        option_group(ui, "Quality", true);
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
        ui.label("JPEG max quality only recompresses files saved above that value.");

        option_group(ui, "Size", false);
        ui.checkbox(&mut self.settings.resize_enabled, "Resize");
        ui.horizontal_wrapped(|ui| {
            for edge in [800_u32, 1600, 1920] {
                let on = self.settings.resize_enabled && self.settings.resize_max_edge == edge;
                let mut text = egui::RichText::new(format!("{edge} px"));
                if on {
                    text = text.color(egui::Color32::WHITE);
                }
                let mut button = egui::Button::new(text).min_size(egui::vec2(84.0, 28.0));
                if on {
                    button = button.fill(accent());
                }
                if ui.add(button).clicked() {
                    self.settings.resize_enabled = true;
                    self.settings.resize_max_edge = edge;
                }
            }
        });
        ui.add_enabled(
            self.settings.resize_enabled,
            egui::DragValue::new(&mut self.settings.resize_max_edge)
                .range(1..=20000)
                .suffix(" px longest side"),
        );
        ui.label("1600 px is the Web preset. That is the step that shrinks phone photos.");

        option_group(ui, "Files", false);
        ui.horizontal_wrapped(|ui| {
            ui.label("Output folder:");
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
        ui.checkbox(
            &mut self.settings.strip_metadata,
            "Strip metadata (removes camera and location data)",
        );

        option_group(ui, "Extra", false);
        ui.checkbox(
            &mut self.settings.choose_best_format,
            "Best uses a smaller format when it is clearly better",
        );
        ui.checkbox(
            &mut self.settings.png_brute,
            "PNG maximum compression (slow)",
        );
        ui.horizontal_wrapped(|ui| {
            ui.label("Or path:");
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
        ui.label("Command for the selected file");
        let command_view = if self.rows.is_empty() {
            None
        } else {
            self.selected.and_then(|index| self.rows.get(index)).map(|row| {
                (row.command.clone(), row.status.clone(), row.choice)
            })
        };
        if self.rows.is_empty() {
            ui.label("Add a file, then optimize, to see its command.");
        } else if let Some((command, status, choice)) = command_view {
            if command.is_empty() {
                if choice == RowChoice::Skip
                    || matches!(
                        status.as_str(),
                        "Already small enough" | "Kept original" | "Skipped"
                    )
                {
                    ui.label("No command. This file was kept as it is.");
                } else {
                    ui.label("Optimize this file to see its command.");
                }
            } else {
                ui.add(
                    egui::Label::new(egui::RichText::new(command).monospace())
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            }
        }
        self.settings.sync_preset();
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
        let webp_lossless = self.settings.webp_lossless;
        egui::Grid::new("files")
            .striped(true)
            .num_columns(4)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.strong("");
                ui.strong("File");
                ui.strong("Size");
                ui.strong("Result");
                ui.end_row();

                for index in 0..self.rows.len() {
                    let mut on = self.checked.contains(&index);
                    let response = ui
                        .checkbox(&mut on, "")
                        .on_hover_text("Check files to change them together");
                    if response.changed() {
                        if on {
                            self.checked.insert(index);
                        } else {
                            self.checked.remove(&index);
                        }
                        self.selected = Some(index);
                    }
                    let selected = self.selected == Some(index);
                    let name = file_label(&self.rows[index].path);
                    if ui.selectable_label(selected, name).clicked() {
                        self.selected = Some(index);
                    }
                    let bytes = self.rows[index]
                        .info
                        .as_ref()
                        .map(|info| model::format_bytes(info.bytes))
                        .unwrap_or_else(|_| "—".to_string());
                    if ui.selectable_label(selected, bytes).clicked() {
                        self.selected = Some(index);
                    }
                    let result = row_result(&self.rows[index], webp_lossless);
                    if ui.selectable_label(selected, result).clicked() {
                        self.selected = Some(index);
                    }
                    ui.end_row();
                }
            });
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(index) = self.selected else {
            ui.label("Click a file to compare.");
            return;
        };
        if self.rows.get(index).is_none() {
            return;
        }
        if self.rows[index].info.is_err() {
            ui.label(egui::RichText::new(file_label(&self.rows[index].path)).strong());
            ui.label(&self.rows[index].status);
            return;
        }

        let path = self.rows[index].path.clone();
        let output = self.rows[index].output_path.clone();
        let original_bytes = self.rows[index].info.as_ref().ok().map(|info| info.bytes);
        let output_bytes = self.rows[index].output_bytes;
        let after_caption = preview_after_caption(&self.rows[index], self.settings.webp_lossless);
        let summary = self.rows[index]
            .recommendation
            .as_ref()
            .map(|rec| rec.summary());
        let stale_result = self.results_stale() && output_bytes.is_some();
        let name = file_label(&path);

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

        ui.label(egui::RichText::new(name).strong());
        if stale_result {
            ui.label(egui::RichText::new("Previous settings").color(warn_text()));
        }
        ui.add_space(6.0);
        let col = (ui.available_width() / 2.0 - 8.0).max(80.0);
        let before = self.before_tex.clone();
        let after = self.after_tex.clone();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(col);
                let caption = match original_bytes {
                    Some(bytes) => format!("Original · {}", model::format_bytes(bytes)),
                    None => "Original".to_string(),
                };
                ui.label(caption);
                if let Some(tex) = &before {
                    show_preview_image(ui, tex, col);
                } else {
                    ui.label("Loading…");
                }
            });
            ui.vertical(|ui| {
                ui.set_width(col);
                ui.label(&after_caption);
                if let Some(tex) = &after {
                    show_preview_image(ui, tex, col);
                } else if output.is_some() {
                    ui.label("Loading…");
                } else if self.running {
                    ui.label("Working…");
                } else if after_caption == "Output" {
                    ui.label("Optimize to compare.");
                } else if !matches!(
                    after_caption.as_str(),
                    "Already small enough" | "Kept original" | "Skipped" | "Skip"
                ) {
                    ui.label("No output file.");
                }
            });
        });
        if let Some(summary) = summary {
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(summary).wrap_mode(egui::TextWrapMode::Wrap),
            );
        }
    }
}

impl eframe::App for OptimizerApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.clamp();
        self.settings.sync_preset();
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
        StepKind::Add => "JPEG, PNG, or WebP. Drop a folder here, or use the button.",
        StepKind::Optimize => "Format buttons under the list override one file.",
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

fn warn_text() -> egui::Color32 {
    egui::Color32::from_rgb(154, 52, 18)
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

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn needs_run(row: &FileRow, stale: bool) -> bool {
    if row.info.is_err() || row.choice == RowChoice::Skip {
        return false;
    }
    if stale {
        return true;
    }
    !settled(row)
}

fn settled(row: &FileRow) -> bool {
    row.output_path.is_some()
        || matches!(
            row.status.as_str(),
            "Already small enough" | "Kept original" | "Skipped"
        )
}

fn has_previous_result(row: &FileRow) -> bool {
    row.output_bytes.is_some()
        || matches!(
            row.status.as_str(),
            "Already small enough" | "Kept original"
        )
}

fn finished_title(wrote: bool, all_skipped: bool, files: usize) -> String {
    if !wrote && all_skipped {
        "Nothing new to write".to_string()
    } else if !wrote {
        "These files are already small enough".to_string()
    } else {
        step_title(StepKind::Open, files)
    }
}

fn destination_text(done_dir: Option<&Path>, chosen_dir: Option<&Path>) -> String {
    if let Some(dir) = done_dir {
        return format!("Originals stay put. Results are in {}.", dir.display());
    }
    if let Some(dir) = chosen_dir {
        return format!("Originals stay put. Results go in {}.", dir.display());
    }
    "Originals stay put. Results go in optimized/ next to the first image.".to_string()
}

fn progress_text(name: &str, done: usize, total: usize) -> String {
    if name.is_empty() {
        format!("{done} of {total}")
    } else {
        format!("{name} · {done} of {total}")
    }
}

fn format_label(row: &FileRow, webp_lossless: bool) -> String {
    if let Some(rec) = &row.recommendation {
        return rec.decision.label().to_string();
    }
    match row.choice {
        RowChoice::Recommended => "Best".to_string(),
        RowChoice::Jpeg => "JPEG".to_string(),
        RowChoice::Png => "PNG".to_string(),
        RowChoice::Webp => {
            let alpha = row.info.as_ref().map(|info| info.has_alpha).unwrap_or(false);
            if alpha || webp_lossless {
                "WebP lossless".to_string()
            } else {
                "WebP".to_string()
            }
        }
        RowChoice::Skip => "Skip".to_string(),
    }
}

fn row_result(row: &FileRow, webp_lossless: bool) -> String {
    if row.info.is_err() {
        return row.status.clone();
    }
    if row.choice == RowChoice::Skip {
        return "Skip".to_string();
    }
    if let Some(bytes) = row.output_bytes {
        let saved = row
            .info
            .as_ref()
            .ok()
            .map(|info| format!(" · saved {:.0}%", model::saved_percent(info.bytes, bytes)))
            .unwrap_or_default();
        let mut text = format!(
            "{} · {}{saved}",
            format_label(row, webp_lossless),
            model::format_bytes(bytes)
        );
        if let Some(note) = row.status.strip_prefix("Done (") {
            if let Some(note) = note.strip_suffix(')') {
                if !note.is_empty() {
                    text.push_str(" · ");
                    text.push_str(note);
                }
            }
        }
        return text;
    }
    if matches!(row.status.as_str(), "Ready" | "") {
        return format_label(row, webp_lossless);
    }
    row.status.clone()
}

fn preview_after_caption(row: &FileRow, webp_lossless: bool) -> String {
    if row.output_bytes.is_some()
        || (row.output_path.is_none()
            && !matches!(row.status.as_str(), "Ready" | "Queued" | "Working" | ""))
    {
        return row_result(row, webp_lossless);
    }
    "Output".to_string()
}

fn shared_choice(rows: &[FileRow], indexes: &[usize]) -> Option<RowChoice> {
    let mut indexes = indexes.iter();
    let first = rows.get(*indexes.next()?)?.choice;
    if indexes.all(|index| rows.get(*index).map(|row| row.choice) == Some(first)) {
        Some(first)
    } else {
        None
    }
}

fn option_group(ui: &mut egui::Ui, title: &str, first: bool) {
    if !first {
        ui.add_space(6.0);
        ui.separator();
    }
    ui.strong(title);
}

fn wrapped_label(ui: &mut egui::Ui, text: &str, size: f32, color: egui::Color32) {
    ui.add(
        egui::Label::new(egui::RichText::new(text).size(size).color(color))
            .wrap_mode(egui::TextWrapMode::Wrap),
    );
}

fn show_preview_image(ui: &mut egui::Ui, tex: &egui::TextureHandle, max_width: f32) {
    let size = tex.size_vec2();
    let max = max_width.min(340.0);
    let scale = if size.x > max && size.x > 0.0 {
        max / size.x
    } else {
        1.0
    };
    ui.add(egui::Image::new((tex.id(), size * scale)));
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
                let name = item
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| item.path.display().to_string());
                let _ = tx.send(Msg::WorkingOn {
                    gen,
                    index: item.index,
                    name,
                });
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

    fn sample_row(status: &str, output: bool, choice: RowChoice) -> FileRow {
        FileRow {
            path: std::path::PathBuf::from("photo.jpg"),
            info: Ok(ImageInfo {
                format: crate::model::SourceFormat::Jpeg,
                width: 100,
                height: 80,
                has_alpha: false,
                bytes: 2000,
            }),
            choice,
            recommendation: None,
            output_bytes: if output { Some(1000) } else { None },
            output_path: if output {
                Some(std::path::PathBuf::from("optimized/photo.jpg"))
            } else {
                None
            },
            status: status.to_string(),
            command: String::new(),
        }
    }

    #[test]
    fn result_column_names_the_chosen_format() {
        use crate::model::{Decision, Encode, WebpMode};
        let mut row = sample_row("WebP", true, RowChoice::Recommended);
        row.recommendation = Some(Recommendation {
            decision: Decision::Use(Encode::Webp(WebpMode::Lossy)),
            sizes: vec![(Encode::Webp(WebpMode::Lossy), 1000)],
            photo: true,
        });
        assert_eq!(row_result(&row, false), "WebP · 1000 B · saved 50%");

        let ready = sample_row("Ready", false, RowChoice::Recommended);
        assert_eq!(row_result(&ready, false), "Best");

        let jpeg = sample_row("Done (resized to 1600x900)", true, RowChoice::Jpeg);
        assert_eq!(
            row_result(&jpeg, false),
            "JPEG · 1000 B · saved 50% · resized to 1600x900"
        );
        assert_eq!(
            row_result(&sample_row("Ready", false, RowChoice::Skip), false),
            "Skip"
        );
    }

    #[test]
    fn a_settings_change_asks_for_another_optimize() {
        let done = sample_row("Done", true, RowChoice::Jpeg);
        assert!(!needs_run(&done, false));
        assert!(needs_run(&done, true));

        let kept = sample_row("Already small enough", false, RowChoice::Recommended);
        assert!(!needs_run(&kept, false));
        assert!(needs_run(&kept, true));

        assert!(needs_run(
            &sample_row("pngcrush failed", false, RowChoice::Png),
            false
        ));
        assert!(!needs_run(
            &sample_row("Skipped", false, RowChoice::Skip),
            true
        ));

        let mut broken = sample_row("not an image", false, RowChoice::Recommended);
        broken.info = Err("not an image".to_string());
        assert!(!needs_run(&broken, true));
    }

    #[test]
    fn the_card_says_where_files_go() {
        assert_eq!(
            destination_text(None, None),
            "Originals stay put. Results go in optimized/ next to the first image."
        );
        assert_eq!(
            destination_text(Some(std::path::Path::new("/tmp/optimized")), None),
            "Originals stay put. Results are in /tmp/optimized."
        );
        assert_eq!(
            destination_text(None, Some(std::path::Path::new("/tmp/custom"))),
            "Originals stay put. Results go in /tmp/custom."
        );
        assert_eq!(finished_title(true, false, 2), "Your images are ready");
        assert_eq!(
            finished_title(false, false, 2),
            "These files are already small enough"
        );
        assert_eq!(finished_title(false, true, 2), "Nothing new to write");
        assert_eq!(progress_text("a.jpg", 1, 4), "a.jpg · 1 of 4");
        assert_eq!(progress_text("", 0, 4), "0 of 4");
    }
}
