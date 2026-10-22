use eframe::egui;

use crate::analyze::{self, ImageInfo};
use crate::execute::{self, CancelFlag, OptimizeOutcome};
use crate::files::{self, FilePrompt};
use crate::model::{self, Decision, Encode, Recommendation, RowChoice};
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
    /// JPEG, PNG, and WebP sizes from this run. Absent means "leave the row's sizes alone."
    measured: Option<Vec<(Encode, u64)>>,
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
        error: String,
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
    advanced: bool,
    coach_dismissed: bool,
    cancel: Option<CancelFlag>,
    rx: Option<Receiver<Msg>>,
    tx: Sender<Msg>,
    show_licenses: bool,
    picker: Option<DirPicker>,
    preview_path: Option<PathBuf>,
    preview_output: Option<PathBuf>,
    preview_error: String,
    before_tex: Option<egui::TextureHandle>,
    after_tex: Option<egui::TextureHandle>,
    /// 0 is all original, 1 is all result.
    compare_split: f32,
    compare_actual: bool,
    compare_large: bool,
}

struct FileRow {
    path: PathBuf,
    info: Result<ImageInfo, String>,
    choice: RowChoice,
    recommendation: Option<Recommendation>,
    /// Sizes kept when the format chip changes, so the WebP line can show before the next run.
    measured: Option<Vec<(Encode, u64)>>,
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
        let advanced = stored_flag(cc.storage, "advanced");
        let coach_dismissed = stored_flag(cc.storage, "coach");
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
            advanced,
            coach_dismissed,
            cancel: None,
            rx: Some(rx),
            tx,
            show_licenses: false,
            picker: None,
            preview_path: None,
            preview_output: None,
            preview_error: String::new(),
            before_tex: None,
            after_tex: None,
            compare_split: 0.5,
            compare_actual: false,
            compare_large: false,
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
                            if patch.measured.is_some() {
                                row.measured = patch.measured;
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
                Msg::Preview {
                    path,
                    before,
                    after,
                    error,
                } => {
                    if self.preview_path.as_ref() == Some(&path) {
                        self.before_tex = before.map(|img| texture(ctx, "before", &img));
                        self.after_tex = after.map(|img| texture(ctx, "after", &img));
                        self.preview_error = error;
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
            measured: None,
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
                row.measured = None;
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
        self.log.clear();
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
        let pct = model::saved_percent(before, after);
        if pct >= 1.0 {
            Some(format!(
                "Saved {pct:.0}% ({}) across {}.",
                model::format_bytes(before.saturating_sub(after)),
                plural(n as usize, "image", "images")
            ))
        } else {
            Some(format!(
                "Finished {}.",
                plural(n as usize, "image", "images")
            ))
        }
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
                if ui
                    .add(egui::Button::new(egui::RichText::new("Licenses").size(14.0)))
                    .on_hover_text("Notices for the tools packed inside this app.")
                    .clicked()
                {
                    self.show_licenses = true;
                }
                ui.add_space(8.0);
                if mode_switch(ui, &mut self.advanced) && self.advanced {
                    self.force_options_open = true;
                }
            });
        });
        ui.label(
            egui::RichText::new("Smaller copies. Your original files stay put.")
                .size(14.0)
                .color(muted_text()),
        );

        if !self.advanced && !self.coach_dismissed && self.tools.is_ok() && self.rows.is_empty()
        {
            ui.add_space(10.0);
            self.coach_banner(ui);
        }

        let files = self.counts().0;
        let mut kind = self.step_now();
        let hovering = ctx.input(|input| !input.raw.hovered_files.is_empty());

        ui.add_space(12.0);
        let border = if hovering { accent() } else { card_stroke() };
        egui::Frame::NONE
            .fill(card_fill())
            .stroke(egui::Stroke::new(if hovering { 2.0_f32 } else { 1.0_f32 }, border))
            .inner_margin(egui::Margin::same(18))
            .corner_radius(egui::CornerRadius::same(12))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                if self.tools.is_ok() {
                    ui.add_enabled_ui(!self.running, |ui| {
                        self.preset_row(ui);
                    });
                    kind = self.step_now();
                    ui.add_space(if self.rows.is_empty() { 14.0 } else { 8.0 });
                }
                let wrote = self.rows.iter().any(|row| row.output_path.is_some());
                let all_skipped = files > 0
                    && self
                        .rows
                        .iter()
                        .filter(|row| row.info.is_ok())
                        .all(|row| row.choice == RowChoice::Skip);
                let compact = !self.rows.is_empty() && !hovering;
                ui.vertical_centered(|ui| {
                    let kicker = if hovering {
                        "Drop files"
                    } else {
                        step_kicker(kind, wrote)
                    };
                    ui.label(
                        egui::RichText::new(kicker)
                            .size(13.0)
                            .strong()
                            .color(accent()),
                    );
                    ui.add_space(2.0);
                    let title = if hovering {
                        "Drop to add these images".to_string()
                    } else if matches!(kind, StepKind::Open) {
                        finished_title(wrote, all_skipped, files)
                    } else {
                        step_title(kind, files)
                    };
                    ui.label(
                        egui::RichText::new(title)
                            .size(if compact { 20.0 } else { 26.0 })
                            .strong()
                            .color(ink_text()),
                    );
                    // Once files are listed, the footer and the list carry the detail.
                    // Still explain a finished run that wrote nothing.
                    let explain_empty = matches!(kind, StepKind::Open) && !wrote;
                    if !compact || explain_empty {
                        let detail = if hovering {
                            "Release to add them to the list."
                        } else if matches!(kind, StepKind::Open) && !wrote {
                            if all_skipped {
                                if self.advanced {
                                    "Every file is set to skip. Choose Best above the list to include them."
                                } else {
                                    "Every file is set to skip. Switch to Advanced and choose Best to include them."
                                }
                            } else {
                                "Nothing was smaller than the original, so no copies were written. Shrink for the web can still make them smaller."
                            }
                        } else {
                            step_detail(kind)
                        };
                        if !detail.is_empty() {
                            ui.add_space(6.0);
                            wrapped_label(ui, detail, 16.0, body_text());
                        }
                        ui.add_space(4.0);
                    }
                });
            });

        if !self.rows.is_empty() {
            ui.add_space(12.0);
            let list_caption = if files == self.rows.len() {
                plural(files, "image", "images")
            } else {
                format!(
                    "{} · {} not usable",
                    plural(files, "image", "images"),
                    self.rows.len() - files
                )
            };
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(list_caption)
                        .strong()
                        .size(16.0)
                        .color(ink_text()),
                );
                if ui.button("Add more").clicked() && !self.running {
                    self.prompt_images();
                }
                if ui
                    .button("Remove all")
                    .on_hover_text("Clears the list. Does not delete any files.")
                    .clicked()
                    && !self.running
                {
                    self.clear_rows();
                }
                if self.advanced && !self.running {
                    if ui.button("Check all").clicked() {
                        self.checked = (0..self.rows.len()).collect();
                    }
                    if ui.button("Check none").clicked() {
                        self.checked.clear();
                    }
                }
            });
            ui.add_space(8.0);
            // A horizontal row that asks for the leftover viewport height collapses
            // once the hero has filled the scroll area. This row grows with its contents.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                ui.set_min_height(180.0);
                let total = ui.available_width().max(520.0);
                let preview_w = (total * 0.42).clamp(220.0, 420.0);
                let list_w = (total - preview_w - 12.0).max(220.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(list_w, 180.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        panel_frame().show(ui, |ui| {
                            ui.set_min_width(list_w - 2.0);
                            if self.advanced {
                                ui.add_enabled_ui(!self.running, |ui| {
                                    self.format_bar(ui);
                                });
                                ui.add_space(6.0);
                            } else {
                                hint(ui, "Click a file to compare before and after.");
                                ui.add_space(4.0);
                            }
                            // Fill the card so the bar sits in the right gutter, not on the Result column.
                            egui::ScrollArea::vertical()
                                .id_salt("file_list")
                                .max_height(360.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    self.table(ui);
                                });
                        });
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(preview_w, 180.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        panel_frame().show(ui, |ui| {
                            ui.set_min_width(preview_w - 2.0);
                            self.preview_ui(ui, ctx);
                        });
                    },
                );
            });
        }

        ui.add_space(12.0);
        if self.advanced {
            let mut options = egui::CollapsingHeader::new("Advanced options").default_open(true);
            if self.force_options_open {
                options = options.open(Some(true));
                self.force_options_open = false;
            }
            options.show(ui, |ui| {
                ui.add_enabled_ui(!self.running, |ui| {
                    self.settings_ui(ui);
                });
            });
        } else if self.tools.is_ok() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("Need quality, size, or format controls?")
                        .color(muted_text()),
                );
                if ui.button("Open Advanced").clicked() {
                    self.advanced = true;
                    self.force_options_open = true;
                }
            });
        }

        if self.show_licenses {
            egui::Window::new("Licenses")
                .open(&mut self.show_licenses)
                .default_width(640.0)
                .default_height(520.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().max_height(460.0).show(ui, |ui| {
                        ui.label(crate::licenses::TEXT);
                    });
                });
        }
        ui.add_space(8.0);
    }

    fn coach_banner(&mut self, ui: &mut egui::Ui) {
        egui::Frame::NONE
            .fill(egui::Color32::from_rgb(255, 247, 237))
            .stroke(egui::Stroke::new(
                1.0_f32,
                egui::Color32::from_rgb(253, 186, 116),
            ))
            .inner_margin(egui::Margin::same(12))
            .corner_radius(egui::CornerRadius::same(10))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Pick a style, add your images, then press the button at the bottom.");
                    if ui.button("Got it").clicked() {
                        self.coach_dismissed = true;
                    }
                });
            });
    }

    fn preset_row(&mut self, ui: &mut egui::Ui) {
        let presets = visible_presets(self.advanced, self.settings.preset);
        let current = self.settings.preset;
        let advanced = self.advanced;
        let mut picked = None;
        let previous_gap = ui.spacing().item_spacing.x;
        ui.spacing_mut().item_spacing.x = 10.0;
        ui.columns(presets.len(), |cols| {
            for (col, preset) in cols.iter_mut().zip(presets) {
                if preset_card(col, preset, current == preset, advanced).clicked() {
                    picked = Some(preset);
                }
            }
        });
        ui.spacing_mut().item_spacing.x = previous_gap;
        if let Some(preset) = picked {
            self.pick_preset(preset);
        }
    }

    fn pick_preset(&mut self, preset: Preset) {
        if preset == Preset::Custom {
            self.advanced = true;
            self.force_options_open = true;
            return;
        }
        self.settings.apply_preset(preset);
    }

    fn action_bar(&mut self, ui: &mut egui::Ui) {
        let files = self.counts().0;
        let kind = self.step_now();
        let wrote = self.rows.iter().any(|row| row.output_path.is_some());
        let all_skipped = files > 0
            && self
                .rows
                .iter()
                .filter(|row| row.info.is_ok())
                .all(|row| row.choice == RowChoice::Skip);
        let try_web = matches!(kind, StepKind::Open) && !wrote && !all_skipped;
        let label = action_label(kind, wrote, all_skipped);
        let stale = !self.running
            && self.results_stale()
            && self.rows.iter().any(has_previous_result);
        let saved = self
            .saved_line()
            .filter(|_| matches!(kind, StepKind::Open) && wrote);
        let status = if !self.log.is_empty() && !self.running {
            self.log.clone()
        } else if self.running {
            progress_text(&self.active_file, self.job_done, self.job_total)
        } else if let Some(saved) = &saved {
            saved.clone()
        } else if stale {
            "Settings changed. Optimize again to apply them.".to_string()
        } else if self.tools.is_err() {
            "This copy of the app is missing its tools.".to_string()
        } else if matches!(kind, StepKind::Open) && !wrote {
            "Originals stay put.".to_string()
        } else if files == 0 {
            "Add images to begin. You can also drop them on this window.".to_string()
        } else {
            let done_dir = if matches!(kind, StepKind::Open) && wrote {
                self.output_dir()
            } else {
                None
            };
            destination_text(done_dir.as_deref(), self.settings.output_dir.as_deref())
        };
        let status_color = if !self.log.is_empty() && !self.running {
            log_color(&self.log)
        } else if saved.is_some() {
            good_text()
        } else if stale {
            warn_text()
        } else {
            body_text()
        };
        let running = self.running;
        let frac = if self.job_total == 0 {
            0.0
        } else {
            self.job_done as f32 / self.job_total as f32
        };
        let mut act = false;
        let mut web = false;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !label.is_empty() && primary_button(ui, label).clicked() {
                if try_web {
                    web = true;
                } else {
                    act = true;
                }
            }
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                if running {
                    ui.add(egui::ProgressBar::new(frac).text(&status));
                } else if saved.is_some() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&status)
                                .size(18.0)
                                .strong()
                                .color(status_color),
                        )
                        .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                } else {
                    wrapped_label(ui, &status, 15.0, status_color);
                }
            });
        });
        if web {
            self.settings.apply_preset(Preset::Web);
            self.start_job();
        } else if act {
            self.act(kind);
        }
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
            format!("Save {} selected as", self.checked.len())
        } else {
            format!(
                "Save {} as",
                self.rows
                    .get(targets[0])
                    .map(|row| file_label(&row.path))
                    .unwrap_or_default()
            )
        };
        let shared = shared_choice(&self.rows, &targets);
        ui.label(egui::RichText::new(title).strong());
        ui.horizontal_wrapped(|ui| {
            for choice in RowChoice::ALL {
                if ui
                    .selectable_label(shared == Some(choice), choice.label())
                    .on_hover_text(choice_tip(choice))
                    .clicked()
                {
                    for index in targets.clone() {
                        self.set_choice(index, choice);
                    }
                }
            }
        });
        hint(
            ui,
            "Check several rows to change them together. Skip leaves a file out.",
        );
    }

    fn remove_row(&mut self, index: usize) {
        if index >= self.rows.len() {
            return;
        }
        self.rows.remove(index);
        let mut next = HashSet::new();
        for checked in &self.checked {
            if *checked == index {
                continue;
            }
            if *checked > index {
                next.insert(checked - 1);
            } else {
                next.insert(*checked);
            }
        }
        self.checked = next;
        self.selected = if self.rows.is_empty() {
            None
        } else {
            Some(index.min(self.rows.len() - 1))
        };
        self.before_tex = None;
        self.after_tex = None;
        self.preview_path = None;
        self.preview_output = None;
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
        hint(
            ui,
            "Changing a control below switches the style to Your settings. Switch back to Simple in the header when you are done. Your settings stay.",
        );

        group_card(ui, "Quality", |ui| {
            let jpeg_on = !self.settings.jpeg_lossless;
            ui.add_enabled(
                jpeg_on,
                egui::Slider::new(&mut self.settings.jpeg_quality, 0..=100).text("JPEG quality"),
            );
            if jpeg_on {
                hint(ui, quality_hint(self.settings.jpeg_quality));
            }
            ui.checkbox(
                &mut self.settings.jpeg_lossless,
                "Keep JPEG quality exact (no softness)",
            );
            if self.settings.jpeg_lossless {
                hint(ui, "The file is only tidied. The picture does not change.");
            } else {
                hint(
                    ui,
                    "Files already saved at this quality or lower are left as they are.",
                );
            }
            ui.add_space(6.0);
            let webp_on = !self.settings.webp_lossless;
            ui.add_enabled(
                webp_on,
                egui::Slider::new(&mut self.settings.webp_quality, 0..=100).text("WebP quality"),
            );
            if webp_on {
                hint(ui, quality_hint(self.settings.webp_quality));
            }
            ui.checkbox(
                &mut self.settings.webp_lossless,
                "Keep WebP quality exact",
            );
            hint(
                ui,
                "Images with transparency always use lossless WebP, so the clear parts stay clear.",
            );
        });

        group_card(ui, "Size", |ui| {
            ui.checkbox(&mut self.settings.resize_enabled, "Shrink the long side");
            ui.horizontal_wrapped(|ui| {
                for edge in [800_u32, 1600, 1920] {
                    let on = self.settings.resize_enabled && self.settings.resize_max_edge == edge;
                    let mut text = egui::RichText::new(format!("{edge} px"));
                    if on {
                        text = text.color(egui::Color32::WHITE);
                    }
                    let mut button = egui::Button::new(text).min_size(egui::vec2(84.0, 30.0));
                    if on {
                        button = button.fill(accent());
                    }
                    let tip = match edge {
                        800 => "Small, for thumbnails and tight pages.",
                        1600 => "The For the web style. Enough for a normal page.",
                        _ => "Sharper, for large layouts.",
                    };
                    if ui.add(button).on_hover_text(tip).clicked() {
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
            hint(
                ui,
                "Phone photos are often 4000 px. 1600 px is what makes them small enough to send.",
            );
        });

        group_card(ui, "Where files go", |ui| {
            let text = self
                .settings
                .output_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "optimized folder next to the first image".to_string());
            ui.label(egui::RichText::new(text).color(ink_text()));
            ui.horizontal_wrapped(|ui| {
                if ui.button("Choose folder").clicked() {
                    self.prompt_output();
                }
                if self.settings.output_dir.is_some() && ui.button("Use the default folder").clicked()
                {
                    self.settings.output_dir = None;
                }
            });
            hint(
                ui,
                "Originals are never replaced. A result with the same name in the output folder is.",
            );
            ui.checkbox(
                &mut self.settings.strip_metadata,
                "Remove camera and location data",
            );
            hint(
                ui,
                "Strips hidden metadata. The picture looks the same.",
            );
        });

        group_card(ui, "Expert", |ui| {
            ui.checkbox(
                &mut self.settings.choose_best_format,
                "Compare formats and keep the smaller one",
            );
            if self.settings.choose_best_format {
                hint(
                    ui,
                    "Photos and large images (long side 1200 px or more) are compared as JPEG quality 90, 4:2:0. Small art stays PNG. WebP is used only when it is at least 8% smaller, and the winner has to save at least 3%.",
                );
            } else {
                hint(ui, "Each file stays in its own format and is only recompressed.");
            }
            ui.checkbox(
                &mut self.settings.png_brute,
                "PNG maximum compression (slow)",
            );
            hint(
                ui,
                "Tries many more PNG strategies. Useful for icons and screenshots. Slow on large photos.",
            );
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Add by path").strong());
            hint(ui, "A file or a folder. Press Enter to add it.");
            ui.horizontal_wrapped(|ui| {
                let entry = ui.add(
                    egui::TextEdit::singleline(&mut self.path_entry)
                        .desired_width(360.0)
                        .hint_text("Paste a path"),
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
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Command for the selected file").strong());
            let command_view = if self.rows.is_empty() {
                None
            } else {
                self.selected
                    .and_then(|index| self.rows.get(index))
                    .map(|row| (row.command.clone(), row.status.clone(), row.choice))
            };
            if self.rows.is_empty() {
                hint(ui, "Add a file, then optimize, to see its command.");
            } else if let Some((command, status, choice)) = command_view {
                if command.is_empty() {
                    if choice == RowChoice::Skip
                        || matches!(
                            status.as_str(),
                            "Already small enough" | "Kept original" | "Skipped"
                        )
                    {
                        hint(ui, "No command. This file was kept as it is.");
                    } else {
                        hint(ui, "Optimize this file to see its command.");
                    }
                } else {
                    if ui.button("Copy command").clicked() {
                        ui.ctx().copy_text(command.clone());
                    }
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgb(226, 232, 240))
                        .inner_margin(egui::Margin::same(8))
                        .corner_radius(egui::CornerRadius::same(6))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(command).monospace().size(13.0))
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        });
                }
            }
        });
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
        // measured stays. A switch from Best to JPEG can show the WebP gap before Optimize.
        row.status = "Ready".to_string();
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let webp_lossless = self.settings.webp_lossless;
        let advanced = self.advanced;
        egui::Grid::new("files")
            .striped(true)
            .num_columns(if advanced { 4 } else { 3 })
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                if advanced {
                    ui.strong("");
                }
                ui.strong("File");
                ui.strong("Size");
                ui.strong("Result");
                ui.end_row();

                for index in 0..self.rows.len() {
                    if advanced {
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
                    let result = result_label(&self.rows[index], webp_lossless, advanced);
                    let color = result_color(&self.rows[index]);
                    if ui
                        .selectable_label(selected, egui::RichText::new(result).color(color))
                        .clicked()
                    {
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
            ui.label(egui::RichText::new("Compare").strong().color(muted_text()));
            ui.add_space(4.0);
            ui.label(egui::RichText::new(file_label(&self.rows[index].path)).strong());
            ui.label(
                egui::RichText::new(&self.rows[index].status)
                    .color(egui::Color32::from_rgb(153, 27, 27)),
            );
            self.remove_button(ui, index);
            return;
        }

        let path = self.rows[index].path.clone();
        let output = self.rows[index].output_path.clone();
        let info = self.rows[index].info.as_ref().ok().cloned();
        let original_bytes = info.as_ref().map(|info| info.bytes);
        let output_bytes = self.rows[index].output_bytes;
        let after_caption = preview_after_caption(&self.rows[index], self.settings.webp_lossless);
        let summary = self.rows[index]
            .recommendation
            .as_ref()
            .map(|rec| rec.summary());
        let webp_line = webp_savings_line(
            &self.rows[index],
            self.results_stale(),
            self.settings.webp_lossless,
        );
        let stale_result = self.results_stale() && output_bytes.is_some();
        let name = file_label(&path);

        if self.preview_path.as_ref() != Some(&path) || self.preview_output != output {
            self.preview_path = Some(path.clone());
            self.preview_output = output.clone();
            self.preview_error.clear();
            self.before_tex = None;
            self.after_tex = None;
            let tx = self.tx.clone();
            let preview_output = output.clone();
            // Windows gives new threads a 1 MB stack. WebP decoding needs more.
            let _ = thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(move || {
                let before = load_preview(&path);
                let after = match preview_output.as_deref() {
                    Some(output) => load_preview(output).map(Some),
                    None => Ok(None),
                };
                let error = match (&before, &after) {
                    (Err(err), _) => format!("Could not preview the original: {err}"),
                    (_, Err(err)) => format!("Could not preview the result: {err}"),
                    _ => String::new(),
                };
                let _ = tx.send(Msg::Preview {
                    path,
                    before: before.ok(),
                    after: after.ok().flatten(),
                    error,
                });
            });
            ctx.request_repaint();
        }

        ui.label(egui::RichText::new("Compare").strong().color(muted_text()));
        ui.add_space(2.0);
        ui.label(egui::RichText::new(name).strong().size(16.0));
        if let Some(info) = &info {
            let mut line = format!(
                "{} · {} × {}",
                info.format.label(),
                info.width,
                info.height
            );
            if info.has_alpha {
                line.push_str(" · has transparency");
            }
            hint(ui, &line);
        }
        if let (Some(before_bytes), Some(after_bytes)) = (original_bytes, output_bytes) {
            let pct = model::saved_percent(before_bytes, after_bytes);
            ui.add_space(4.0);
            if pct >= 1.0 {
                ui.label(
                    egui::RichText::new(format!("{pct:.0}% smaller"))
                        .size(22.0)
                        .strong()
                        .color(good_text()),
                );
            } else if pct <= -1.0 {
                ui.label(
                    egui::RichText::new("Larger than the original")
                        .strong()
                        .color(warn_text()),
                );
            }
        }
        if stale_result {
            ui.label(
                egui::RichText::new("This comparison used the previous settings.").color(warn_text()),
            );
        }
        if let Some(line) = &webp_line {
            ui.add_space(4.0);
            hint(ui, line);
        }
        ui.add_space(8.0);
        let before_line = match original_bytes {
            Some(bytes) => format!("Original {}", model::format_bytes(bytes)),
            None => "Original".to_string(),
        };
        let after_line = if after_caption == "Output" {
            "Result not yet".to_string()
        } else {
            format!("Result {after_caption}")
        };
        hint(ui, &format!("{before_line}  ·  {after_line}"));
        let before = self.before_tex.clone();
        let after = self.after_tex.clone();
        if before.is_some() && after.is_some() {
            let text = egui::RichText::new("Compare").color(egui::Color32::WHITE);
            if ui
                .add(egui::Button::new(text).fill(accent()).min_size(egui::vec2(120.0, 32.0)))
                .on_hover_text("Open the slider across the window. Left of the line is the original.")
                .clicked()
            {
                self.compare_large = true;
            }
        }
        if !self.preview_error.is_empty() {
            ui.label(
                egui::RichText::new(&self.preview_error)
                    .color(egui::Color32::from_rgb(153, 27, 27)),
            );
        }
        match (before, after) {
            (Some(_before), Some(after)) => {
                show_preview_image(ui, &after, ui.available_width().min(280.0));
            }
            (Some(before), None) => {
                if output.is_some() && self.preview_error.is_empty() {
                    ui.label("Loading the result…");
                } else if output.is_none() && self.running {
                    ui.label("Working…");
                } else if output.is_none() && after_caption == "Output" {
                    ui.label("Press Optimize to compare.");
                }
                show_preview_image(ui, &before, ui.available_width());
            }
            (None, Some(after)) => show_preview_image(ui, &after, ui.available_width()),
            (None, None) => {
                if self.preview_error.is_empty() {
                    ui.label("Loading…");
                }
            }
        }
        if let Some(summary) = summary {
            ui.add_space(6.0);
            hint(ui, "Compared");
            ui.add(egui::Label::new(summary).wrap_mode(egui::TextWrapMode::Wrap));
        }
        self.remove_button(ui, index);
    }

    fn show_compare_large(&mut self, ctx: &egui::Context) {
        if !self.compare_large {
            return;
        }
        let (Some(before), Some(after)) = (self.before_tex.clone(), self.after_tex.clone()) else {
            self.close_compare(ctx);
            return;
        };
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.close_compare(ctx);
            return;
        }
        let name = self
            .preview_path
            .as_ref()
            .map(|path| file_label(path))
            .unwrap_or_default();
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("compare_large"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.set_clip_rect(screen);
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_rgb(15, 23, 42));
                let _ = ui.interact(screen, ui.id().with("dim"), egui::Sense::click());
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(screen), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(name)
                                .strong()
                                .size(16.0)
                                .color(egui::Color32::WHITE),
                        );
                        ui.add_space(12.0);
                        if mode_button(ui, "Fit", !self.compare_actual).clicked() {
                            self.compare_actual = false;
                        }
                        if mode_button(ui, "Actual size", self.compare_actual).clicked() {
                            self.compare_actual = true;
                        }
                        let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
                        let full_label = if fullscreen {
                            "Window"
                        } else {
                            "Full screen"
                        };
                        if mode_button(ui, full_label, fullscreen).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
                        }
                        if mode_button(ui, "Close", false).clicked() {
                            self.compare_large = false;
                        }
                    });
                    ui.label(
                        egui::RichText::new(
                            "Left of the line is the original. Drag the line. Esc closes.",
                        )
                        .color(egui::Color32::from_rgb(203, 213, 225)),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.compare_split, 0.0..=1.0).show_value(false),
                    );
                    let split = &mut self.compare_split;
                    let actual = self.compare_actual;
                    let height = ui.available_height().max(120.0);
                    if actual {
                        egui::ScrollArea::both()
                            .id_salt("compare_large_actual")
                            .max_height(height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                comparison_wipe(ui, &before, &after, split, true, false, height);
                            });
                    } else {
                        comparison_wipe(ui, &before, &after, split, false, true, height);
                    }
                });
            });
        if !self.compare_large {
            self.exit_fullscreen(ctx);
        }
    }

    fn close_compare(&mut self, ctx: &egui::Context) {
        self.compare_large = false;
        self.exit_fullscreen(ctx);
    }

    fn exit_fullscreen(&self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().fullscreen.unwrap_or(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }

    fn remove_button(&mut self, ui: &mut egui::Ui, index: usize) {
        ui.add_space(8.0);
        if ui
            .add_enabled(!self.running, egui::Button::new("Remove from list"))
            .on_hover_text("Takes this file off the list. Does not delete it.")
            .clicked()
        {
            self.remove_row(index);
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
        storage.set_string(
            "advanced",
            if self.advanced { "1" } else { "0" }.to_string(),
        );
        storage.set_string(
            "coach",
            if self.coach_dismissed { "1" } else { "0" }.to_string(),
        );
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        self.intake_drops(ctx);
        egui::TopBottomPanel::bottom("action_bar")
            .frame(
                egui::Frame::NONE
                    .fill(card_fill())
                    .stroke(egui::Stroke::new(1.0_f32, card_stroke()))
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ctx, |ui| {
                self.action_bar(ui);
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(page_fill())
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.ui(ui, ctx);
                });
            });
        self.show_picker(ctx);
        self.show_compare_large(ctx);
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
                self.log = if added == 0 {
                    "Those images are already in the list.".to_string()
                } else {
                    format!("Added {}.", plural(added, "image", "images"))
                };
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
    ui.label(
        egui::RichText::new(picker.current.display().to_string())
            .size(13.0)
            .color(muted_text()),
    );
    ui.horizontal_wrapped(|ui| {
        if ui.button("Up one folder").clicked() {
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
        ui.colored_label(egui::Color32::from_rgb(153, 27, 27), &picker.error);
    }
    let images = picker.entries.iter().filter(|item| !item.is_dir).count();
    let folders = picker.entries.iter().filter(|item| item.is_dir).count();
    if picker.error.is_empty() {
        hint(
            ui,
            &format!(
                "{images} images · {folders} folders. Click a folder to open it. Click a file to select it."
            ),
        );
    }
    let mut enter: Option<PathBuf> = None;
    let mut toggle: Option<PathBuf> = None;
    egui::ScrollArea::vertical()
        .max_height(320.0)
        .show(ui, |ui| {
            if picker.entries.is_empty() && picker.error.is_empty() {
                ui.label("This folder is empty.");
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
    ui.add_space(8.0);
    let mut outcome = PickerOutcome::Stay;
    match picker.purpose {
        PickerPurpose::Images => {
            let selected = picker.selected.len();
            if selected > 0 {
                ui.label(format!("{selected} selected"));
            }
            if primary_button(ui, "Add images in this folder").clicked() {
                outcome = PickerOutcome::Add(vec![picker.current.clone()]);
            }
            ui.add_space(4.0);
            if ui
                .add_enabled(
                    !picker.selected.is_empty(),
                    egui::Button::new("Add selected only").min_size(egui::vec2(240.0, 36.0)),
                )
                .clicked()
            {
                outcome = PickerOutcome::Add(picker.selected.clone());
            }
        }
        PickerPurpose::Output => {
            hint(
                ui,
                "Results go here. A file already in this folder with the same name is replaced. Originals are never replaced.",
            );
            if primary_button(ui, "Use this folder").clicked() {
                outcome = PickerOutcome::Output(picker.current.clone());
            }
        }
    }
    ui.add_space(4.0);
    if ui.button("Close").clicked() {
        outcome = PickerOutcome::Close;
    }
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
        StepKind::Add => "Drop your images here".to_string(),
        StepKind::Optimize => format!("Ready to shrink {}", plural(files, "image", "images")),
        StepKind::Working => "Optimizing your images".to_string(),
        StepKind::Open => "Your images are ready".to_string(),
    }
}

fn step_kicker(kind: StepKind, wrote: bool) -> &'static str {
    match kind {
        StepKind::NeedTools => "Setup",
        StepKind::Add => "Start here",
        StepKind::Optimize => "Ready when you are",
        StepKind::Working => "In progress",
        StepKind::Open if wrote => "All done",
        StepKind::Open => "Nothing new",
    }
}

fn step_detail(kind: StepKind) -> &'static str {
    match kind {
        StepKind::NeedTools => "This copy of the app is missing its tools.",
        StepKind::Add => "JPEG, PNG, or WebP. Drop a file or a folder here, or use the button below.",
        StepKind::Optimize => "Originals stay where they are. A copy is written only when it is smaller.",
        StepKind::Working => "You can cancel. Files already finished are kept.",
        StepKind::Open => "",
    }
}

fn action_label(kind: StepKind, wrote: bool, all_skipped: bool) -> &'static str {
    let try_web = matches!(kind, StepKind::Open) && !wrote && !all_skipped;
    if try_web {
        "Shrink for the web"
    } else if matches!(kind, StepKind::Open) && !wrote {
        ""
    } else {
        step_action(kind)
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

fn good_text() -> egui::Color32 {
    egui::Color32::from_rgb(21, 128, 61)
}

fn muted_text() -> egui::Color32 {
    egui::Color32::from_rgb(71, 85, 105)
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
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size.y = 32.0;
    style.spacing.slider_rail_height = 10.0;
    // Solid bar, always visible when the list overflows. The handle color is
    // the inactive widget stroke, set below to slate so it is not a black stripe.
    let mut scroll = egui::style::ScrollStyle::solid();
    scroll.bar_width = 8.0;
    scroll.handle_min_length = 28.0;
    scroll.bar_inner_margin = 4.0;
    scroll.bar_outer_margin = 4.0;
    scroll.foreground_color = true;
    style.spacing.scroll = scroll;
    ctx.set_style(style);

    let track = egui::Color32::from_rgb(203, 213, 225);
    let mut visuals = egui::Visuals::light();
    visuals.window_fill = page_fill();
    visuals.panel_fill = page_fill();
    visuals.faint_bg_color = egui::Color32::from_rgb(226, 232, 240);
    // Scrollbar track and text fields. White here disappears on the cards.
    visuals.extreme_bg_color = track;
    visuals.code_bg_color = egui::Color32::from_rgb(226, 232, 240);
    visuals.hyperlink_color = egui::Color32::from_rgb(29, 78, 216);
    visuals.selection.bg_fill = egui::Color32::from_rgb(147, 197, 253);
    visuals.slider_trailing_fill = true;
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
    // Strong labels use the active text color. Keep it dark so section titles
    // stay visible. Buttons that sit on the accent color set their own white text.
    // Slider rails use this same fill. White on a white card hides the rail.
    visuals.widgets.inactive.bg_fill = track;
    // Scrollbar handles use this color. Ink here paints a black bar over the list.
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(100, 116, 139));
    // The light theme's idle checkbox stroke has width 0. A white fill on a
    // white card then disappears until the box is checked.
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.5, muted_text());
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(226, 232, 240);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.5, ink_text());
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(71, 85, 105));
    visuals.widgets.active.bg_fill = accent();
    // Do not paint this white. Strong labels take their color from here.
    visuals.widgets.active.fg_stroke = egui::Stroke::new(2.0, ink_text());
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

fn stored_flag(storage: Option<&dyn eframe::Storage>, key: &str) -> bool {
    storage
        .and_then(|storage| storage.get_string(key))
        .as_deref()
        == Some("1")
}

fn mode_switch(ui: &mut egui::Ui, advanced: &mut bool) -> bool {
    let before = *advanced;
    // Fixed width so this pair stays Simple, then Advanced, and does not push Licenses off.
    ui.allocate_ui_with_layout(
        egui::vec2(196.0, 32.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if mode_button(ui, "Simple", !*advanced)
                .on_hover_text("A few clear choices. Extra controls stay hidden.")
                .clicked()
            {
                *advanced = false;
            }
            if mode_button(ui, "Advanced", *advanced)
                .on_hover_text("Quality, size, output folder, and per-file format.")
                .clicked()
            {
                *advanced = true;
            }
        },
    );
    *advanced != before
}

fn mode_button(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    let mut text = egui::RichText::new(label).size(14.0);
    if on {
        text = text.strong().color(egui::Color32::WHITE);
    }
    let mut button = egui::Button::new(text).min_size(egui::vec2(96.0, 32.0));
    if on {
        button = button.fill(accent());
    }
    ui.add(button)
}

fn visible_presets(advanced: bool, current: Preset) -> Vec<Preset> {
    if advanced || current == Preset::Custom {
        Preset::ALL.to_vec()
    } else {
        vec![Preset::Web, Preset::Compress, Preset::Lossless]
    }
}

fn preset_plain(preset: Preset) -> (&'static str, &'static str) {
    match preset {
        Preset::Web => (
            "For the web",
            "Shrinks the long side to 1600 px for sites and email.",
        ),
        Preset::Compress => (
            "Smaller file",
            "JPEG for photos and large images. PNG for small art. WebP when it is smaller.",
        ),
        Preset::Lossless => (
            "Perfect quality",
            "Looks identical. The file gets a little smaller.",
        ),
        Preset::Custom => ("Your settings", "Uses the quality and size you set."),
    }
}

fn preset_card(ui: &mut egui::Ui, preset: Preset, selected: bool, show_badge: bool) -> egui::Response {
    let (title, detail) = preset_plain(preset);
    let width = ui.available_width();
    let height = if show_badge { 138.0 } else { 112.0 };
    let fill = if selected {
        egui::Color32::from_rgb(255, 247, 237)
    } else {
        card_fill()
    };
    let response = ui.add(
        egui::Button::new("")
            .min_size(egui::vec2(width, height))
            .fill(fill)
            .stroke(egui::Stroke::new(
                if selected { 2.0_f32 } else { 1.0_f32 },
                if selected { accent() } else { card_stroke() },
            ))
            .corner_radius(egui::CornerRadius::same(10)),
    );
    if ui.is_enabled() {
        response.clone().on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    let rect = response.rect.shrink2(egui::vec2(12.0, 10.0));
    let painter = ui.painter_at(rect);
    let title_color = if selected { accent() } else { ink_text() };
    let mut y = rect.top();
    let title_galley = painter.layout_no_wrap(
        title.to_string(),
        egui::FontId::proportional(16.0),
        title_color,
    );
    let title_h = title_galley.size().y;
    painter.galley(egui::pos2(rect.left(), y), title_galley, title_color);
    y += title_h + 2.0;
    if show_badge {
        let badge = painter.layout_no_wrap(
            preset.label().to_string(),
            egui::FontId::proportional(12.0),
            muted_text(),
        );
        let badge_h = badge.size().y;
        painter.galley(egui::pos2(rect.left(), y), badge, muted_text());
        y += badge_h + 4.0;
    } else {
        y += 4.0;
    }
    let detail_galley = painter.layout(
        detail.to_string(),
        egui::FontId::proportional(13.0),
        body_text(),
        rect.width(),
    );
    painter.galley(egui::pos2(rect.left(), y), detail_galley, body_text());
    if selected {
        let mark = painter.layout_no_wrap(
            "In use".to_string(),
            egui::FontId::proportional(12.0),
            accent(),
        );
        let mark_y = rect.bottom() - mark.size().y;
        painter.galley(egui::pos2(rect.left(), mark_y), mark, accent());
    }
    response.on_hover_text(preset.detail())
}

fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(card_fill())
        .stroke(egui::Stroke::new(1.0_f32, card_stroke()))
        .inner_margin(egui::Margin::same(12))
        .corner_radius(egui::CornerRadius::same(10))
}

fn group_card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    egui::Frame::NONE
        .fill(card_fill())
        .stroke(egui::Stroke::new(1.0_f32, card_stroke()))
        .inner_margin(egui::Margin::same(12))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(
                egui::RichText::new(title)
                    .strong()
                    .size(16.0)
                    .color(ink_text()),
            );
            ui.add_space(6.0);
            add(ui);
        });
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(egui::RichText::new(text).size(13.0).color(muted_text()))
            .wrap_mode(egui::TextWrapMode::Wrap),
    );
}

fn quality_hint(quality: u8) -> &'static str {
    match quality {
        0..=50 => "Small files, visible softness.",
        51..=75 => "Smaller files, slight softness.",
        90 => "The usual save for photos and large images: 4:2:0 color.",
        76..=89 => "Smaller files, slight softness. Still 4:2:0 color.",
        91..=95 => "Very sharp, and the files stay large.",
        _ => "Almost identical to the original.",
    }
}

/// Gray compare-panel line. Same 8% bar Best uses, measured against the JPEG or PNG.
fn webp_savings_line(row: &FileRow, stale: bool, webp_lossless: bool) -> Option<String> {
    if stale || row.info.is_err() {
        return None;
    }
    let compat = match row.choice {
        RowChoice::Jpeg => Encode::Jpeg,
        RowChoice::Png => Encode::Png,
        RowChoice::Recommended => match row.recommendation.as_ref().map(|rec| rec.decision) {
            Some(Decision::Use(encode @ (Encode::Jpeg | Encode::Png))) => encode,
            _ => return None,
        },
        RowChoice::Webp | RowChoice::Skip => return None,
    };
    let sizes = row.measured.as_deref()?;
    let compat_bytes = sizes
        .iter()
        .find_map(|(encode, bytes)| (*encode == compat).then_some(*bytes))?;
    let has_alpha = row
        .info
        .as_ref()
        .ok()
        .map(|info| info.has_alpha)
        .unwrap_or(false);
    let mode = model::webp_chip_mode(has_alpha, webp_lossless);
    let webp_bytes = sizes.iter().find_map(|(encode, bytes)| match encode {
        Encode::Webp(found) if *found == mode => Some(*bytes),
        _ => None,
    })?;
    if compat_bytes == 0 || webp_bytes >= compat_bytes {
        return None;
    }
    let saved = (compat_bytes - webp_bytes) as f64 / compat_bytes as f64;
    if saved + 1e-9 < model::WEBP_BEAT_BY {
        return None;
    }
    let diff = model::format_bytes(compat_bytes - webp_bytes);
    let pct = saved * 100.0;
    if has_alpha && matches!(compat, Encode::Jpeg) {
        Some(format!(
            "WebP keeps the transparency and is {diff} smaller ({pct:.0}%)."
        ))
    } else {
        Some(format!("WebP would be {diff} smaller ({pct:.0}%)."))
    }
}

fn choice_tip(choice: RowChoice) -> &'static str {
    match choice {
        RowChoice::Recommended => {
            "Photos and large images are compared as JPEG. Small art is compared as PNG. WebP wins when it is at least 8% smaller."
        }
        RowChoice::Jpeg => "Write a JPEG at the quality below, with 4:2:0 color.",
        RowChoice::Png => "Write a PNG. The usual save for small art.",
        RowChoice::Webp => "Always write a WebP.",
        RowChoice::Skip => "Leave this file out. Nothing is written.",
    }
}

fn result_label(row: &FileRow, webp_lossless: bool, advanced: bool) -> String {
    if row.info.is_err() {
        return row.status.clone();
    }
    if !advanced {
        match row.status.as_str() {
            "Ready" | "" if row.output_bytes.is_none() => {
                return match row.choice {
                    RowChoice::Recommended => "Ready".to_string(),
                    RowChoice::Skip => "Will skip".to_string(),
                    RowChoice::Jpeg => "Will save as JPEG".to_string(),
                    RowChoice::Png => "Will save as PNG".to_string(),
                    RowChoice::Webp => "Will save as WebP".to_string(),
                };
            }
            "Queued" => return "Waiting…".to_string(),
            "Working" => return "Working…".to_string(),
            "Kept original" => return "Kept the original".to_string(),
            "Skipped" => return "Skipped".to_string(),
            "Already small enough" => return "Already small enough".to_string(),
            _ => {}
        }
    }
    row_result(row, webp_lossless)
}

fn result_color(row: &FileRow) -> egui::Color32 {
    if row.info.is_err() {
        return egui::Color32::from_rgb(153, 27, 27);
    }
    if row.choice == RowChoice::Skip {
        return muted_text();
    }
    if let (Ok(info), Some(output)) = (&row.info, row.output_bytes) {
        if output < info.bytes {
            return good_text();
        }
        return body_text();
    }
    match row.status.as_str() {
        "Working" | "Queued" => accent(),
        "Already small enough" | "Kept original" | "Skipped" | "Ready" | "" => {
            if row.choice == RowChoice::Skip {
                muted_text()
            } else {
                body_text()
            }
        }
        _ => {
            if row.status.starts_with("Done") || row.output_bytes.is_some() {
                good_text()
            } else {
                egui::Color32::from_rgb(153, 27, 27)
            }
        }
    }
}

fn log_color(log: &str) -> egui::Color32 {
    let lower = log.to_ascii_lowercase();
    if lower.starts_with("stopped at") {
        warn_text()
    } else if lower.starts_with("added")
        || lower.starts_with("output folder")
        || lower.starts_with("stopped")
        || lower.starts_with("those images")
    {
        body_text()
    } else {
        egui::Color32::from_rgb(153, 27, 27)
    }
}

fn wrapped_label(ui: &mut egui::Ui, text: &str, size: f32, color: egui::Color32) {
    ui.add(
        egui::Label::new(egui::RichText::new(text).size(size).color(color))
            .wrap_mode(egui::TextWrapMode::Wrap),
    );
}

fn show_preview_image(ui: &mut egui::Ui, tex: &egui::TextureHandle, max_width: f32) {
    let size = tex.size_vec2();
    let max = max_width.min(420.0);
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
        let measured = rec.sizes.clone();
        let encode = match rec.decision {
            Decision::Keep => {
                return RowPatch {
                    status: "Already small enough".to_string(),
                    recommendation: Some(rec),
                    measured: Some(measured),
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
        patch.measured = Some(measured);
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
                measured: None,
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
        Ok(outcome) => {
            let mut patch = done_patch(outcome);
            if matches!(encode, Encode::Jpeg | Encode::Png) {
                let mode = model::webp_chip_mode(item.info.has_alpha, settings.webp_lossless);
                if let Ok(webp_bytes) = execute::measure_webp(
                    tools,
                    settings,
                    &item.path,
                    &item.info,
                    mode,
                    cancel,
                ) {
                    let compat_bytes = patch.output_bytes.unwrap_or(0);
                    patch.measured = Some(vec![
                        (encode, compat_bytes),
                        (Encode::Webp(mode), webp_bytes),
                    ]);
                }
            }
            patch
        }
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
        measured: None,
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
        measured: None,
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

const PREVIEW_EDGE: u32 = 4096;

fn load_preview(path: &Path) -> Result<PreviewImage, String> {
    let mut last = "could not read the image".to_string();
    for attempt in 0..8 {
        match image::open(path) {
            Ok(image) => {
                let (width, height) = model::fit(image.width(), image.height(), PREVIEW_EDGE);
                let image = if width != image.width() || height != image.height() {
                    image.resize(width, height, image::imageops::FilterType::Triangle)
                } else {
                    image
                };
                let rgba = image.to_rgba8();
                return Ok(PreviewImage {
                    width: rgba.width() as usize,
                    height: rgba.height() as usize,
                    rgba: rgba.into_raw(),
                });
            }
            Err(err) => {
                last = err.to_string();
                let locked = last.to_ascii_lowercase().contains("os error");
                if attempt + 1 < 8 && locked {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                break;
            }
        }
    }
    Err(last)
}

fn comparison_wipe(
    ui: &mut egui::Ui,
    before: &egui::TextureHandle,
    after: &egui::TextureHandle,
    split: &mut f32,
    actual: bool,
    allow_upscale: bool,
    max_height: f32,
) {
    let native = before.size_vec2();
    if native.x <= 0.0 || native.y <= 0.0 {
        return;
    }
    let scale = if actual {
        1.0
    } else {
        let fit = (ui.available_width() / native.x).min(max_height / native.y);
        if allow_upscale {
            fit
        } else {
            fit.min(1.0)
        }
    };
    let display = native * scale;
    let pad = ((ui.available_width() - display.x) * 0.5).max(0.0);
    ui.add_space(0.0);
    ui.horizontal(|ui| {
        ui.add_space(pad);
        comparison_wipe_at(ui, before, after, split, display);
    });
}

fn comparison_wipe_at(
    ui: &mut egui::Ui,
    before: &egui::TextureHandle,
    after: &egui::TextureHandle,
    split: &mut f32,
    display: egui::Vec2,
) {
    let (rect, response) = ui.allocate_exact_size(display, egui::Sense::click_and_drag());
    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            *split = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        }
    }
    let split_clamped = (*split).clamp(0.0, 1.0);
    let painter = ui.painter().with_clip_rect(rect);
    painter.image(
        before.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    let cut = rect.left() + rect.width() * split_clamped;
    let after_rect = egui::Rect::from_min_max(egui::pos2(cut, rect.top()), rect.max);
    painter.image(
        after.id(),
        after_rect,
        egui::Rect::from_min_max(egui::pos2(split_clamped, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    painter.line_segment(
        [
            egui::pos2(cut, rect.top()),
            egui::pos2(cut, rect.bottom()),
        ],
        egui::Stroke::new(5.0, egui::Color32::from_black_alpha(180)),
    );
    painter.line_segment(
        [
            egui::pos2(cut, rect.top()),
            egui::pos2(cut, rect.bottom()),
        ],
        egui::Stroke::new(2.0, egui::Color32::WHITE),
    );
    painter.circle_filled(egui::pos2(cut, rect.center().y), 11.0, egui::Color32::WHITE);
    painter.circle_stroke(
        egui::pos2(cut, rect.center().y),
        11.0,
        egui::Stroke::new(2.0, ink_text()),
    );
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
        assert_eq!(step_title(StepKind::Add, 0), "Drop your images here");
        assert_eq!(step_kicker(StepKind::Add, false), "Start here");
        assert_eq!(step_kicker(StepKind::Open, true), "All done");
        assert_eq!(step_kicker(StepKind::Open, false), "Nothing new");
        assert_eq!(action_label(StepKind::Add, false, false), "Add images");
        assert_eq!(action_label(StepKind::Open, true, false), "Open folder");
        assert_eq!(action_label(StepKind::Open, false, false), "Shrink for the web");
        assert_eq!(action_label(StepKind::Open, false, true), "");

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
            measured: None,
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

    #[test]
    fn simple_mode_hides_custom_until_the_settings_leave_a_style() {
        assert_eq!(
            visible_presets(false, Preset::Compress),
            vec![Preset::Web, Preset::Compress, Preset::Lossless]
        );
        assert_eq!(visible_presets(false, Preset::Custom), Preset::ALL.to_vec());
        assert_eq!(visible_presets(true, Preset::Web), Preset::ALL.to_vec());
    }

    #[test]
    fn simple_results_avoid_format_jargon_before_a_run() {
        let ready = sample_row("Ready", false, RowChoice::Recommended);
        assert_eq!(result_label(&ready, false, false), "Ready");
        assert_eq!(result_label(&ready, false, true), "Best");
        assert_eq!(
            result_label(
                &sample_row("Queued", false, RowChoice::Recommended),
                false,
                false
            ),
            "Waiting…"
        );
        assert_eq!(
            result_label(&sample_row("Working", false, RowChoice::Jpeg), false, false),
            "Working…"
        );
        assert_eq!(
            result_label(&sample_row("Ready", false, RowChoice::Skip), false, false),
            "Will skip"
        );
    }

    #[test]
    fn webp_line_names_a_real_gap_and_stays_quiet_under_eight_percent() {
        use crate::model::WebpMode;
        let mut jpeg = sample_row("Done", true, RowChoice::Jpeg);
        jpeg.measured = Some(vec![
            (Encode::Jpeg, 1000),
            (Encode::Webp(WebpMode::Lossy), 780),
        ]);
        assert_eq!(
            webp_savings_line(&jpeg, false, false).as_deref(),
            Some("WebP would be 220 B smaller (22%).")
        );
        // Switching off Best clears the decision and the written file. The sizes stay.
        jpeg.recommendation = None;
        jpeg.output_bytes = None;
        assert_eq!(
            webp_savings_line(&jpeg, false, false).as_deref(),
            Some("WebP would be 220 B smaller (22%).")
        );

        jpeg.measured = Some(vec![
            (Encode::Jpeg, 1000),
            (Encode::Webp(WebpMode::Lossy), 920),
        ]);
        assert_eq!(
            webp_savings_line(&jpeg, false, false).as_deref(),
            Some("WebP would be 80 B smaller (8%).")
        );
        jpeg.measured = Some(vec![
            (Encode::Jpeg, 1000),
            (Encode::Webp(WebpMode::Lossy), 921),
        ]);
        assert_eq!(webp_savings_line(&jpeg, false, false), None);
        assert_eq!(webp_savings_line(&jpeg, true, false), None);

        jpeg.choice = RowChoice::Webp;
        jpeg.measured = Some(vec![
            (Encode::Jpeg, 1000),
            (Encode::Webp(WebpMode::Lossy), 780),
        ]);
        assert_eq!(webp_savings_line(&jpeg, false, false), None);

        let mut png = sample_row("Done", true, RowChoice::Png);
        png.info.as_mut().unwrap().has_alpha = true;
        png.measured = Some(vec![
            (Encode::Png, 1000),
            (Encode::Webp(WebpMode::Lossless), 780),
        ]);
        assert_eq!(
            webp_savings_line(&png, false, false).as_deref(),
            Some("WebP would be 220 B smaller (22%).")
        );

        let mut flat = sample_row("Done", true, RowChoice::Jpeg);
        flat.info.as_mut().unwrap().has_alpha = true;
        flat.measured = Some(vec![
            (Encode::Jpeg, 1000),
            (Encode::Webp(WebpMode::Lossless), 780),
        ]);
        assert_eq!(
            webp_savings_line(&flat, false, false).as_deref(),
            Some("WebP keeps the transparency and is 220 B smaller (22%).")
        );
    }

    #[test]
    fn jpeg_choice_measures_webp_without_writing_it() {
        use image::ImageEncoder;
        let Ok(tools) = crate::tools::locate() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-hint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.jpg");
        let image = image::RgbImage::from_fn(48, 48, |x, y| {
            image::Rgb([((x * 13 + y * 7) % 256) as u8, (x * 3) as u8, (y * 5) as u8])
        });
        let file = std::fs::File::create(&src).unwrap();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
            std::io::BufWriter::new(file),
            95,
        );
        encoder
            .write_image(image.as_raw(), 48, 48, image::ExtendedColorType::Rgb8)
            .unwrap();
        let info = analyze::analyze(&src).unwrap();
        let out = dir.join("optimized");
        let item = JobItem {
            index: 0,
            path: src,
            info,
            choice: RowChoice::Jpeg,
            recommendation: None,
            choose_best: true,
        };
        let patch = optimize_item(
            &tools,
            &crate::settings::Settings::default(),
            &item,
            &out,
            &execute::CancelFlag::new(),
        );
        let measured = patch.measured.expect("webp size");
        assert!(measured.iter().any(|(encode, _)| *encode == Encode::Jpeg));
        assert!(measured
            .iter()
            .any(|(encode, _)| matches!(encode, Encode::Webp(_))));
        assert!(patch.output_path.unwrap().extension().unwrap() == "jpg");
        let wrote_webp = std::fs::read_dir(&out).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.path().extension().map(|ext| ext == "webp"))
                .unwrap_or(false)
        });
        assert!(!wrote_webp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_opens_a_webp_written_by_cwebp() {
        let Ok(tools) = crate::tools::locate() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-preview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("card.png");
        let image = image::RgbaImage::from_fn(180, 120, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255])
        });
        image.save(&src).unwrap();
        let webp = dir.join("card.webp");
        let status = std::process::Command::new(&tools.cwebp)
            .args(["-q", "80", src.to_str().unwrap(), "-o", webp.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success());
        let preview = load_preview(&webp).expect("webp preview");
        assert_eq!(preview.width, 180);
        assert_eq!(preview.height, 120);
        assert_eq!(preview.rgba.len(), 180 * 120 * 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quality_hints_follow_the_slider() {
        assert_eq!(quality_hint(40), "Small files, visible softness.");
        assert_eq!(
            quality_hint(90),
            "The usual save for photos and large images: 4:2:0 color."
        );
        assert_eq!(quality_hint(100), "Almost identical to the original.");
    }
}
