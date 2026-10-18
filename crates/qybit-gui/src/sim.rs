//! GUI T3 界面核对用的模拟设备（`sim` 特性；只在 CI 截图核对时编译，发布产物不含）。
//!
//! 播放走真实核心、自有解码、pack 与渲染循环；只模拟设备与时间（`qy-player` 的假后端，按真实
//! 时间消费，队列不足时补静音并记为欠载）。模拟设备不接受单声道（与常见独占端点相同），用于核对
//! 严格路径的拒绝与单声道复制选项。由环境变量选择：
//!
//! - `QY_GUI_SIM`：`ok`（正常）、`slow`（读取限速为 44.1 kHz 16 位双声道码率的 1/4，产生真实欠载）、
//!   `lost`（第 1 个输出流约 2 秒后设备失效）。
//! - `QY_GUI_SIM_FILES`：逗号分隔的测试曲目，生成在临时目录并按顺序加入播放列表：`mono48`
//!   （48 kHz 16 位单声道 5 秒）、`stereo44`（44.1 kHz 16 位双声道 10 秒）、`hires96`（96 kHz 24 位
//!   双声道 10 秒）、`short`（44.1 kHz 16 位双声道 1 秒）、`float44`（44.1 kHz 32 位浮点双声道 10 秒；
//!   模拟设备只接受整数，用于核对浮点的严格拒绝与明确开启的浮点转整数，F3）。
//!
//! 未启用 `sim` 特性时 [`Sim`] 是不可构造的空类型，`from_env` 恒为 `None`。

#[cfg(not(feature = "sim"))]
pub use disabled::Sim;

#[cfg(not(feature = "sim"))]
mod disabled {
    use std::path::PathBuf;

    use crate::app::Device;
    use qy_ui::Ui;
    use qy_ui::session::Notify;

    pub enum Sim {}

    impl Sim {
        pub fn from_env() -> Option<Self> {
            None
        }
        pub fn title(&self) -> String {
            match *self {}
        }
        pub fn endpoint(&self) -> Device {
            match *self {}
        }
        pub fn files(&self) -> Vec<PathBuf> {
            match *self {}
        }
        pub fn ui(&self, _notify: Notify) -> Ui {
            match *self {}
        }
    }
}

#[cfg(feature = "sim")]
pub use enabled::Sim;

#[cfg(feature = "sim")]
mod enabled {
    use std::path::PathBuf;

    use crate::app::Device;
    use qy_player::signal::{write_test_wav, write_test_wav_float};
    use qy_player::testing::{FakeBackend, FakeDevice, NoMonoNegotiator};
    use qy_ui::Ui;
    use qy_ui::session::Notify;

    pub struct Sim {
        mode: String,
        files: Vec<PathBuf>,
    }

    impl Sim {
        /// 未设置 `QY_GUI_SIM` 时为 `None`（照常使用 WASAPI）。
        pub fn from_env() -> Option<Self> {
            let mode = std::env::var("QY_GUI_SIM").ok()?;
            let dir = std::env::temp_dir().join("qybit-gui-sim");
            let _ = std::fs::create_dir_all(&dir);
            let names = std::env::var("QY_GUI_SIM_FILES").unwrap_or_default();
            let files = names
                .split(',')
                .filter_map(|n| {
                    if n.trim() == "float44" {
                        let path = dir.join("sim-float44.wav");
                        write_test_wav_float(&path, 44_100, 2, 441_000).ok()?;
                        return Some(path);
                    }
                    let (rate, channels, bits, seconds) = match n.trim() {
                        "mono48" => (48_000, 1, 16, 5),
                        "stereo44" => (44_100, 2, 16, 10),
                        "hires96" => (96_000, 2, 24, 10),
                        "short" => (44_100, 2, 16, 1),
                        _ => return None,
                    };
                    let path = dir.join(format!("sim-{}.wav", n.trim()));
                    write_test_wav(&path, rate, channels, bits, u64::from(rate) * seconds).ok()?;
                    Some(path)
                })
                .collect();
            Some(Self { mode, files })
        }

        pub fn files(&self) -> Vec<PathBuf> {
            self.files.clone()
        }

        pub fn title(&self) -> String {
            format!(
                "QyBit（模拟设备：{}；只供界面核对，没有声音输出）",
                self.mode
            )
        }

        pub fn endpoint(&self) -> Device {
            Device {
                id: "sim".into(),
                name: Some("模拟设备（不接受单声道）".into()),
            }
        }

        pub fn ui(&self, notify: Notify) -> Ui {
            let mut backend = FakeBackend::with_negotiator(
                &[(16, 16), (24, 24), (32, 24), (32, 32)],
                NoMonoNegotiator,
            );
            backend.device = FakeDevice::Clock;
            if self.mode == "lost" {
                // 每包 10 ms：第 200 次取缓冲时报告设备失效。
                backend.invalidate = Some((0, 200));
            }
            let mut ui = Ui::new(
                move |_device: Option<&str>| -> Result<_, String> { Ok(backend.clone()) },
                notify,
            );
            if self.mode == "slow" {
                ui.model.read_limit = Some(44_100);
            }
            ui
        }
    }
}
