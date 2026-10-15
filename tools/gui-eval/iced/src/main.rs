//! GUI T1 选型样例（iced）：工具栏按钮、1000 行播放列表、每 250 ms 更新一次的状态行。
//! 环境变量 `QY_EVAL_STATIC=1` 时不定时刷新（测量完全空闲）。
#![windows_subsystem = "windows"]

use std::time::{Duration, Instant};

use iced::widget::{button, column, container, lazy, row, scrollable, text};
use iced::{Element, Length, Subscription};

#[derive(Debug, Clone)]
enum Message {
    Play,
    Stop,
    Tick,
}

struct App {
    rows: Vec<(String, String, String)>,
    playing: Option<usize>,
    start: Instant,
    tick: bool,
}

impl Default for App {
    fn default() -> Self {
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

impl App {
    fn update(&mut self, m: Message) {
        match m {
            Message::Play => self.playing = Some(0),
            Message::Stop => self.playing = None,
            Message::Tick => {}
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let bar = row![
            button("播放").on_press(Message::Play),
            button("停止").on_press(Message::Stop)
        ]
        .spacing(6);
        // 列表只在“正在播放”变化时重建（`lazy` 按依赖值缓存），状态行刷新不重建 1000 行。
        let rows = &self.rows;
        let list = lazy(self.playing, move |playing| {
            column(rows.iter().enumerate().map(|(i, (name, len, fmt))| {
                row![
                    text(if *playing == Some(i) { "▶" } else { "" }).width(24),
                    text(name.clone()).width(Length::Fill),
                    text(len.clone()).width(60),
                    text(fmt.clone()).width(200),
                ]
                .into()
            }))
        });
        let t = self.start.elapsed().as_secs();
        let status = text(format!(
            "源 44100 Hz / 2 声道 / 16 位 → 设备 16/16 位（独占）  {}:{:02}  欠载 0",
            t / 60,
            t % 60
        ));
        container(column![bar, scrollable(list).height(Length::Fill), status].spacing(4))
            .padding(6)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        if self.tick {
            iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        }
    }
}

fn main() -> iced::Result {
    iced::application(App::default, App::update, App::view)
        .title("QyBit GUI 选型（iced）")
        .subscription(App::subscription)
        .run()
}
