//! GUI T1 选型样例（Slint）：工具栏按钮、1000 行播放列表表格、每 250 ms 更新一次的状态行。
//! 环境变量 `QY_EVAL_STATIC=1` 时不定时刷新（测量完全空闲）。
#![windows_subsystem = "windows"]

use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ModelRc, StandardListViewItem, VecModel};

slint::slint! {
    import { Button, StandardTableView, VerticalBox, HorizontalBox } from "std-widgets.slint";
    export component Main inherits Window {
        title: "QyBit GUI 选型（Slint）";
        preferred-width: 800px;
        preferred-height: 600px;
        in property <[[StandardListViewItem]]> rows;
        in property <string> status;
        callback play();
        callback stop();
        VerticalBox {
            HorizontalBox {
                alignment: start;
                Button { text: "播放"; clicked => { root.play(); } }
                Button { text: "停止"; clicked => { root.stop(); } }
            }
            StandardTableView {
                columns: [{ title: "" , width: 24px }, { title: "文件" }, { title: "时长" }, { title: "格式" }];
                rows: root.rows;
            }
            Text { text: root.status; }
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = Main::new()?;
    let rows: Vec<ModelRc<StandardListViewItem>> = (0..1000)
        .map(|i| {
            let cells: Vec<StandardListViewItem> = [
                String::new(),
                format!("第 {i:04} 首 测试曲目.flac"),
                format!("{}:{:02}", i % 7 + 2, i % 60),
                "FLAC 44.1 kHz 16 位 2 声道".to_string(),
            ]
            .iter()
            .map(|s| StandardListViewItem::from(s.as_str()))
            .collect();
            ModelRc::from(Rc::new(VecModel::from(cells)))
        })
        .collect();
    let model = Rc::new(VecModel::from(rows));
    ui.set_rows(ModelRc::from(model.clone()));
    let m = model.clone();
    ui.on_play(move || {
        use slint::Model;
        if let Some(row) = m.row_data(0) {
            row.set_row_data(0, StandardListViewItem::from("▶"));
        }
    });
    let m = model.clone();
    ui.on_stop(move || {
        use slint::Model;
        if let Some(row) = m.row_data(0) {
            row.set_row_data(0, StandardListViewItem::from(""));
        }
    });
    let start = Instant::now();
    let timer = slint::Timer::default();
    if std::env::var_os("QY_EVAL_STATIC").is_none() {
        let weak = ui.as_weak();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                let t = start.elapsed().as_secs();
                if let Some(ui) = weak.upgrade() {
                    ui.set_status(
                    format!(
                        "源 44100 Hz / 2 声道 / 16 位 → 设备 16/16 位（独占）  {}:{:02}  欠载 0",
                        t / 60,
                        t % 60
                    )
                    .into(),
                );
                }
            },
        );
    }
    ui.run()
}
