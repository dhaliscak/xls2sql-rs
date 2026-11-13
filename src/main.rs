#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod loader;
mod sql;
mod viewer;

use eframe::egui;
use loader::Sheet;
use sql::{FirstColumn, SqlSettings};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Instant;
use viewer::OutputView;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const APP_TITLE: &str = "xls2sql 3.0 (by Daniel Haliscak)";

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 450.0])
            .with_min_inner_size([500.0, 300.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(APP_TITLE, options, Box::new(|_cc| Ok(Box::<App>::default())))
}

/// Result of work done on a background thread.
enum JobResult {
    Loaded {
        result: Result<Vec<Sheet>, String>,
        ms: u128,
    },
    Generated {
        view: OutputView,
        ms: u128,
    },
    Saved {
        file_name: String,
        ms: u128,
        error: Option<String>,
    },
}

struct App {
    database_name: String,
    table_name: String,
    create_table: bool,
    first_column: FirstColumn,
    separator: String,
    trim_white_spaces: bool,
    prefer_nulls: bool,

    file_path: String,
    sheets: Arc<Vec<Sheet>>,
    selected_sheet: Option<usize>,

    output: Option<OutputView>,
    status: String,
    job: Option<Receiver<JobResult>>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            database_name: String::new(),
            table_name: String::new(),
            create_table: true,
            first_column: FirstColumn::None,
            separator: "1000".into(),
            trim_white_spaces: true,
            prefer_nulls: true,
            file_path: String::new(),
            sheets: Arc::new(Vec::new()),
            selected_sheet: None,
            output: None,
            status: String::new(),
            job: None,
        }
    }
}

fn show_message(title: &str, text: &str, level: rfd::MessageLevel) {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(text)
        .set_level(level)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

fn non_empty_or(s: &str, default: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

fn save_parts(path: &Path, parts: &[String]) -> std::io::Result<()> {
    let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    for p in parts {
        w.write_all(p.as_bytes())?;
    }
    w.flush()
}

impl App {
    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn spawn_job(&mut self, work: impl FnOnce() -> JobResult + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        self.job = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
    }

    fn load_file(&mut self, path: PathBuf) {
        if self.busy() {
            return;
        }
        self.file_path = path.display().to_string();
        self.sheets = Arc::new(Vec::new());
        self.selected_sheet = None;
        self.status = "Loading file...".into();
        self.spawn_job(move || {
            let timer = Instant::now();
            let result = loader::load_file(&path);
            JobResult::Loaded {
                result,
                ms: timer.elapsed().as_millis(),
            }
        });
    }

    fn validate_inputs(&self) -> String {
        let mut errors = String::new();
        if self.file_path.trim().is_empty() {
            errors.push_str("File is not selected.\n");
        }
        if self.selected_sheet.is_none() {
            errors.push_str("Workbook is not selected.\n");
        }
        errors
    }

    fn read_settings(&self) -> SqlSettings {
        SqlSettings {
            database_name: non_empty_or(&self.database_name, "databasename"),
            table_name: non_empty_or(&self.table_name, "tablename"),
            create_table: self.create_table,
            separator: self
                .separator
                .parse::<usize>()
                .ok()
                .filter(|&n| n >= 1)
                .unwrap_or(1000),
            prefer_nulls: self.prefer_nulls,
            trim_white_spaces: self.trim_white_spaces,
            first_column: self.first_column,
        }
    }

    fn start_generate(&mut self, save: bool) {
        if self.busy() {
            return;
        }
        let errors = self.validate_inputs();
        if !errors.is_empty() {
            show_message("xls2sql", &errors, rfd::MessageLevel::Warning);
            return;
        }
        let Some(idx) = self.selected_sheet else { return };

        let settings = self.read_settings();
        let sheets = Arc::clone(&self.sheets);
        if !save {
            // Drop the previous result first so old and new output are never alive together.
            self.output = None;
        }
        self.status = if save { "Saving..." } else { "Generating..." }.into();
        self.spawn_job(move || {
            let timer = Instant::now();
            let sheet = &sheets[idx];
            if save {
                let parts = sql::generate_sql_parts(&settings, sheet);
                let file_name = format!("{}.txt", settings.table_name);
                let dir = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
                    .unwrap_or_else(|| PathBuf::from("."));
                let error = save_parts(&dir.join(&file_name), &parts)
                    .err()
                    .map(|e| e.to_string());
                JobResult::Saved {
                    file_name,
                    ms: timer.elapsed().as_millis(),
                    error,
                }
            } else {
                let parts = sql::generate_sql_parts(&settings, sheet);
                JobResult::Generated {
                    view: OutputView::new(parts),
                    ms: timer.elapsed().as_millis(),
                }
            }
        });
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.job else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint();
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                self.status.clear();
                show_message("Error", "Error Occurred", rfd::MessageLevel::Warning);
                return;
            }
        };
        self.job = None;

        match result {
            JobResult::Loaded { result, ms } => {
                match result {
                    Ok(sheets) => {
                        self.selected_sheet = if sheets.len() == 1 { Some(0) } else { None };
                        self.sheets = Arc::new(sheets);
                    }
                    Err(e) => show_message("xls2sql", &e, rfd::MessageLevel::Warning),
                }
                self.status = format!("File Loading Time: {ms}ms");
            }
            JobResult::Generated { view, ms } => {
                self.output = Some(view);
                self.status = format!("Execution Time: {ms}ms");
            }
            JobResult::Saved {
                file_name,
                ms,
                error,
            } => {
                self.status = match error {
                    None => format!("Saved to {file_name} | Execution Time: {ms}ms"),
                    Some(e) => {
                        show_message("xls2sql", &e, rfd::MessageLevel::Error);
                        format!("Save failed: {e}")
                    }
                };
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.len() != 1 || self.busy() {
            return;
        }
        let path = dropped.into_iter().next().unwrap();
        if !loader::is_supported(&path) {
            show_message(
                "Invalid File",
                "Only Excel files (.xls, .xlsx, .csv) are supported.",
                rfd::MessageLevel::Warning,
            );
            return;
        }
        self.load_file(path);
    }

    fn open_file_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Please select an excel to convert")
            .add_filter(
                "Excel files (*.xls *.xlsx *.csv)",
                &loader::SUPPORTED_EXTENSIONS,
            )
            .pick_file()
        {
            self.load_file(path);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job(ctx);
        self.handle_dropped_files(ctx);
        let busy = self.busy();

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if busy {
                    ui.spinner();
                }
                ui.label(&self.status);
            });
        });

        egui::TopBottomPanel::top("settings").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Database");
                ui.add(
                    egui::TextEdit::singleline(&mut self.database_name)
                        .desired_width(200.0)
                        .char_limit(100),
                );
                ui.label("Table");
                ui.add(
                    egui::TextEdit::singleline(&mut self.table_name)
                        .desired_width(200.0)
                        .char_limit(100),
                );
                ui.checkbox(&mut self.create_table, "Generate CREATE TABLE Script");
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("First Column");
                egui::ComboBox::from_id_salt("first_column")
                    .width(170.0)
                    .selected_text(self.first_column.label())
                    .show_ui(ui, |ui| {
                        for fc in FirstColumn::ALL {
                            ui.selectable_value(&mut self.first_column, fc, fc.label());
                        }
                    });
                ui.label("Separator");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.separator).desired_width(100.0))
                    .changed()
                {
                    self.separator.retain(|c| c.is_ascii_digit());
                }
                ui.checkbox(&mut self.trim_white_spaces, "Trim White Spaces");
                ui.checkbox(&mut self.prefer_nulls, "Prefer Nulls");
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Path");
                ui.add(
                    egui::TextEdit::singleline(&mut self.file_path.as_str())
                        .desired_width(371.0)
                        .hint_text("Drop a file here or click Open File"),
                );
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button("Open File").clicked() {
                        self.open_file_dialog();
                    }

                    if self.sheets.len() > 1 {
                        let sel_text = self
                            .selected_sheet
                            .map(|i| self.sheets[i].name.clone())
                            .unwrap_or_default();
                        let sheets = Arc::clone(&self.sheets);
                        egui::ComboBox::from_id_salt("workbook")
                            .width(120.0)
                            .selected_text(sel_text)
                            .show_ui(ui, |ui| {
                                for (i, sh) in sheets.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut self.selected_sheet,
                                        Some(i),
                                        &sh.name,
                                    );
                                }
                            });
                    }

                    if ui.button("Generate").clicked() {
                        self.start_generate(false);
                    }
                    if ui.button("Save").clicked() {
                        self.start_generate(true);
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Output");
                if let Some(view) = &self.output {
                    if ui.small_button("Copy All").clicked() {
                        ui.ctx().copy_text(view.to_text());
                        self.status = format!("Copied {} characters", view.len());
                    }
                }
            });
            if let Some(view) = &mut self.output {
                view.show(ui);
            }
        });
    }
}
