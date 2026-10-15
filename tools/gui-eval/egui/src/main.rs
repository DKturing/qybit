//! GUI T1 选型样例（eframe/egui）：工具栏按钮、1000 行播放列表表格、每 250 ms 更新一次的状态行。
//! 环境变量 `QY_EVAL_STATIC=1` 时不定时刷新（测量完全空闲）。
#![windows_subsystem = "windows"]

use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

struct App {
    rows: Vec<(String, String, String)>,
    playing: Option<usize>,
    start: Instant,
    tick: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        load_cjk_font(&cc.egui_ctx);
        Self {
            rows: (0..1000)
                .map(|i| {
                    (
                        format!("第 {i:04} 首 测试曲目.flac"),
                        format!("{}:{:02}", i % 7 + 2, i % 60),
                        "FLAC 44.1 kHz 16 位 2 声道".to_string(),
                    )
                })
                .collect(),
            playing: None,
            start: Instant::now(),
            tick: std::env::var_os("QY_EVAL_STATIC").is_none(),
        }
    }
}

/// egui 自带字体不含汉字：从系统字体目录加载一个中文字体（Windows：微软雅黑）。
fn load_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ];
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".into(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("播放").clicked() {
                    self.playing = Some(0);
                }
                if ui.button("停止").clicked() {
                    self.playing = None;
                }
            });
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            let t = self.start.elapsed().as_secs();
            ui.label(format!(
                "源 44100 Hz / 2 声道 / 16 位 → 设备 16/16 位（独占）  {}:{:02}  欠载 0",
                t / 60,
                t % 60
            ));
        });
        egui::CentralPanel::default().show(ui, |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::exact(24.0))
                .column(Column::remainder())
                .column(Column::exact(60.0))
                .column(Column::exact(200.0))
                .header(20.0, |mut h| {
                    for title in ["", "文件", "时长", "格式"] {
                        h.col(|ui| {
                            ui.strong(title);
                        });
                    }
                })
                .body(|body| {
                    body.rows(18.0, self.rows.len(), |mut row| {
                        let i = row.index();
                        let (name, len, fmt) = &self.rows[i];
                        row.col(|ui| {
                            if self.playing == Some(i) {
                                ui.label("▶");
                            }
                        });
                        row.col(|ui| {
                            ui.label(name);
                        });
                        row.col(|ui| {
                            ui.label(len);
                        });
                        row.col(|ui| {
                            ui.label(fmt);
                        });
                    });
                });
        });
        if self.tick {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "QyBit GUI 选型（egui）",
        eframe::NativeOptions::default(),
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
