//! QyBit 界面模型（GUI T2）：与平台无关、只用安全 Rust 的界面层。
//!
//! 结构（docs/gui-t1-selection.md §7）：
//! - [`Ui`] 在界面线程持有 [`Model`]（播放列表、当前播放、最近的报告与日志），提供界面操作
//!   （添加/移除文件、播放、暂停、停止、切歌、选择设备与选项）；
//! - 播放在独立的**播放线程**运行（[`qy_player::play_with`]：真实核心、自有解码、pack 与独占
//!   输出后端，渲染线程规则不变）；界面只发送控制命令、接收状态快照与文字，不持有、不等待音频
//!   队列；
//! - 文件头读取（时长与格式）与文件夹扫描在**读取线程**运行，界面线程不做文件 I/O；
//! - 两个后台线程经无界通道把 [`UiEvent`] 交给界面线程，每次发送后调用前端提供的 `notify`
//!   （Windows 前端为 `PostMessageW`）；后台线程从不等待界面线程，界面卡顿不影响播放；
//! - [`text`] 给出各前端共用的显示文字（格式、时长、状态、保真结论）。
//!
//! 平台前端只把模型映射到各自的控件（`qybit-gui`：Windows 原生控件、Linux 自绘）。格式由组装层给出
//! （[`Ui::with_formats`]，E2）；[`Ui::new`] 用官方解码器。

pub mod model;
pub mod probe;
pub mod session;
pub mod settings;
pub mod text;

pub use model::{Changes, Deferred, Entry, Model, Outcome, Playing, SortKey, Ui};
pub use probe::{Probe, TrackInfo};
pub use session::UiEvent;
