//! 共用绘制层：界面状态、布局、软件绘制与输入处理。窗口后端只负责把像素区域送到屏幕、
//! 把系统事件翻译成 [`Input`]。
//!
//! 样例内容与 GUI T1 相同（docs/gui-t1-selection.md §1）：工具栏按钮、1000 行播放列表（文件名、
//! 时长、格式三列，中文文字）、每 250 ms 刷新一次的状态行；另加文件拖放、键盘与滚轮。
//! 刷新状态行时只重绘状态栏所在区域，后端也只提交这一块。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

/// 状态行刷新间隔。
pub const TICK: Duration = Duration::from_millis(250);

/// 后端交给界面的输入（坐标为物理像素）。
pub enum Input {
    Resize(u32, u32),
    Click(f32, f32),
    /// 滚轮：正数向下，单位为行。
    Wheel(i32),
    Key(Key),
    Drop(Vec<PathBuf>),
    Tick,
}

#[derive(Clone, Copy, Debug)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Space,
}

/// 需要重绘的区域。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    All,
    List,
    Status,
}

/// 物理像素矩形（后端按它提交）。
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32
            && y >= self.y as f32
            && x < (self.x + self.w) as f32
            && y < (self.y + self.h) as f32
    }
}

struct Row {
    title: String,
    secs: u32,
    format: &'static str,
}

const FORMATS: [&str; 4] = [
    "44.1 kHz / 16 位 / 2 声道",
    "96 kHz / 24 位 / 2 声道",
    "192 kHz / 24 位 / 2 声道",
    "48 kHz / 32 位浮点 / 2 声道",
];

const BUTTONS: [&str; 6] = ["打开…", "播放", "暂停", "停止", "上一首", "下一首"];

const BG: u32 = 0xf6f6f6;
const TEXT: u32 = 0x1f1f1f;
const DIM: u32 = 0x666666;
const LINE: u32 = 0xd0d0d0;
const SEL: u32 = 0xcce4f7;
const PLAYING: u32 = 0x0067c0;

/// 每类重绘的耗时统计（绘制，不含提交）。
#[derive(Default)]
pub struct Stats {
    pub count: u32,
    pub total: Duration,
    pub max: Duration,
}

impl Stats {
    fn add(&mut self, d: Duration) {
        self.count += 1;
        self.total += d;
        self.max = self.max.max(d);
    }

    pub fn line(&self, name: &str) -> String {
        if self.count == 0 {
            return format!("{name}：0 次");
        }
        let avg = self.total.as_secs_f64() * 1000.0 / f64::from(self.count);
        format!(
            "{name}：{} 次，平均 {avg:.2} ms，最长 {:.2} ms",
            self.count,
            self.max.as_secs_f64() * 1000.0
        )
    }
}

pub struct App {
    fonts: FontSystem,
    glyphs: SwashCache,
    layouts: HashMap<String, Buffer>,
    scale: f32,
    pub pixmap: Pixmap,
    rows: Vec<Row>,
    sel: Option<usize>,
    playing: Option<usize>,
    paused: bool,
    pos_ms: u64,
    top: usize,
    last_click: Option<(Instant, usize)>,
    pub full: Stats,
    pub list: Stats,
    pub status: Stats,
    pub font_load: Duration,
}

impl App {
    pub fn new(width: u32, height: u32, scale: f32) -> Self {
        let t = Instant::now();
        let fonts = FontSystem::new();
        let font_load = t.elapsed();
        let rows = (0..1000u32)
            .map(|i| Row {
                title: format!("第 {:04} 首 · 测试曲目（中文文件名）{}.flac", i + 1, i + 1),
                secs: 120 + (i * 37) % 300,
                format: FORMATS[i as usize % FORMATS.len()],
            })
            .collect();
        App {
            fonts,
            glyphs: SwashCache::new(),
            layouts: HashMap::new(),
            scale,
            pixmap: new_pixmap(width, height),
            rows,
            sel: None,
            playing: Some(0),
            paused: false,
            pos_ms: 0,
            top: 0,
            last_click: None,
            full: Stats::default(),
            list: Stats::default(),
            status: Stats::default(),
            font_load,
        }
    }

    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    fn px(&self, v: f32) -> u32 {
        (v * self.scale).round() as u32
    }

    fn toolbar(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.width(),
            h: self.px(44.0),
        }
    }

    fn header(&self) -> Rect {
        Rect {
            x: 0,
            y: self.toolbar().h,
            w: self.width(),
            h: self.px(28.0),
        }
    }

    fn status_rect(&self) -> Rect {
        let h = self.px(28.0).min(self.height());
        Rect {
            x: 0,
            y: self.height() - h,
            w: self.width(),
            h,
        }
    }

    fn list_rect(&self) -> Rect {
        let y = self.header().y + self.header().h;
        let bottom = self.status_rect().y;
        Rect {
            x: 0,
            y,
            w: self.width(),
            h: bottom.saturating_sub(y),
        }
    }

    fn row_h(&self) -> u32 {
        self.px(26.0).max(1)
    }

    fn visible_rows(&self) -> usize {
        (self.list_rect().h / self.row_h()) as usize
    }

    fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.visible_rows())
    }

    fn button_rects(&self) -> Vec<Rect> {
        let mut x = self.px(8.0);
        let y = self.px(7.0);
        let h = self.px(30.0);
        BUTTONS
            .iter()
            .map(|b| {
                let w = self.px(if b.chars().count() > 2 { 72.0 } else { 56.0 });
                let r = Rect { x, y, w, h };
                x += w + self.px(6.0);
                r
            })
            .collect()
    }

    /// 处理输入，返回需要重绘的区域。
    pub fn update(&mut self, input: Input) -> Option<Region> {
        match input {
            Input::Resize(w, h) => {
                if w == self.width() && h == self.height() {
                    return None;
                }
                self.pixmap = new_pixmap(w, h);
                self.top = self.top.min(self.max_top());
                Some(Region::All)
            }
            Input::Tick => {
                if self.playing.is_some() && !self.paused {
                    self.pos_ms += TICK.as_millis() as u64;
                    Some(Region::Status)
                } else {
                    None
                }
            }
            Input::Wheel(lines) => self.scroll_to(self.top as i64 + i64::from(lines) * 3),
            Input::Key(k) => self.key(k),
            Input::Drop(paths) => {
                for p in paths {
                    let title = p.file_name().map_or_else(
                        || p.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    );
                    self.rows.push(Row {
                        title,
                        secs: 0,
                        format: "（未读取）",
                    });
                }
                self.scroll_to(self.rows.len() as i64);
                Some(Region::All)
            }
            Input::Click(x, y) => self.click(x, y),
        }
    }

    fn scroll_to(&mut self, top: i64) -> Option<Region> {
        let t = top.clamp(0, self.max_top() as i64) as usize;
        if t == self.top {
            return None;
        }
        self.top = t;
        Some(Region::List)
    }

    fn select(&mut self, i: usize) -> Option<Region> {
        let i = i.min(self.rows.len().saturating_sub(1));
        self.sel = Some(i);
        let vis = self.visible_rows().max(1);
        if i < self.top {
            self.top = i;
        } else if i >= self.top + vis {
            self.top = i + 1 - vis;
        }
        Some(Region::List)
    }

    fn key(&mut self, k: Key) -> Option<Region> {
        let cur = self.sel.unwrap_or(self.top);
        let page = self.visible_rows().max(1);
        match k {
            Key::Up => self.select(cur.saturating_sub(1)),
            Key::Down => self.select(cur + 1),
            Key::PageUp => self.select(cur.saturating_sub(page)),
            Key::PageDown => self.select(cur + page),
            Key::Home => self.select(0),
            Key::End => self.select(self.rows.len()),
            Key::Enter => self.play(self.sel.unwrap_or(0)),
            Key::Space => {
                self.paused = !self.paused;
                Some(Region::Status)
            }
        }
    }

    fn play(&mut self, i: usize) -> Option<Region> {
        if i >= self.rows.len() {
            return None;
        }
        self.playing = Some(i);
        self.paused = false;
        self.pos_ms = 0;
        Some(Region::All)
    }

    fn click(&mut self, x: f32, y: f32) -> Option<Region> {
        if let Some(b) = self.button_rects().iter().position(|r| r.contains(x, y)) {
            return match b {
                0 => {
                    eprintln!("原型：文件对话框未实现（见文档）");
                    None
                }
                1 => self.play(self.sel.or(self.playing).unwrap_or(0)),
                2 => {
                    self.paused = !self.paused;
                    Some(Region::Status)
                }
                3 => {
                    self.playing = None;
                    Some(Region::All)
                }
                4 => self.play(self.playing.unwrap_or(0).saturating_sub(1)),
                _ => self.play(self.playing.map_or(0, |p| p + 1)),
            };
        }
        let list = self.list_rect();
        if list.contains(x, y) {
            let i = self.top + ((y as u32 - list.y) / self.row_h()) as usize;
            if i >= self.rows.len() {
                return None;
            }
            let now = Instant::now();
            let double = matches!(self.last_click, Some((t, r)) if r == i && now - t < Duration::from_millis(400));
            self.last_click = Some((now, i));
            if double {
                return self.play(i);
            }
            self.sel = Some(i);
            return Some(Region::List);
        }
        None
    }

    /// 重绘区域，返回需要提交的矩形。
    pub fn paint(&mut self, region: Region) -> Rect {
        let t = Instant::now();
        let r = match region {
            Region::All => {
                let all = self.full_rect();
                fill(&mut self.pixmap, all, BG);
                self.paint_toolbar();
                self.paint_header();
                self.paint_list();
                self.paint_status();
                self.full_rect()
            }
            Region::List => {
                self.paint_list();
                self.list_rect()
            }
            Region::Status => {
                self.paint_status();
                self.status_rect()
            }
        };
        let d = t.elapsed();
        match region {
            Region::All => self.full.add(d),
            Region::List => self.list.add(d),
            Region::Status => self.status.add(d),
        }
        r
    }

    fn full_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.width(),
            h: self.height(),
        }
    }

    fn paint_toolbar(&mut self) {
        let bar = self.toolbar();
        fill(&mut self.pixmap, bar, BG);
        for (r, label) in self.button_rects().into_iter().zip(BUTTONS) {
            let radius = self.px(4.0) as f32;
            rounded(&mut self.pixmap, r, radius, 0xffffff, LINE);
            self.text_centered(r, label, TEXT);
        }
        let x = self.button_rects().last().map_or(0, |r| r.x + r.w) + self.px(16.0);
        let label = Rect {
            x,
            y: bar.y,
            w: bar.w.saturating_sub(x),
            h: bar.h,
        };
        self.text(
            label,
            self.px(4.0),
            "设备：hw:CARD=Generic_1,DEV=0（独占）",
            DIM,
        );
        hline(&mut self.pixmap, bar.y + bar.h - 1, bar.w, LINE);
    }

    fn columns(&self) -> [Rect; 3] {
        let w = self.width();
        let fmt_w = self.px(230.0).min(w);
        let dur_w = self.px(70.0).min(w - fmt_w);
        let title_w = w - fmt_w - dur_w;
        [
            Rect {
                x: 0,
                y: 0,
                w: title_w,
                h: 0,
            },
            Rect {
                x: title_w,
                y: 0,
                w: dur_w,
                h: 0,
            },
            Rect {
                x: title_w + dur_w,
                y: 0,
                w: fmt_w,
                h: 0,
            },
        ]
    }

    fn paint_header(&mut self) {
        let h = self.header();
        fill(&mut self.pixmap, h, 0xececec);
        for (c, label) in self.columns().into_iter().zip(["标题", "时长", "格式"]) {
            let cell = Rect {
                x: c.x,
                y: h.y,
                w: c.w,
                h: h.h,
            };
            self.text(cell, self.px(8.0), label, DIM);
        }
        hline(&mut self.pixmap, h.y + h.h - 1, h.w, LINE);
    }

    fn paint_list(&mut self) {
        let list = self.list_rect();
        fill(&mut self.pixmap, list, 0xffffff);
        let rh = self.row_h();
        let cols = self.columns();
        let vis = self.visible_rows() + 1;
        for i in self.top..(self.top + vis).min(self.rows.len()) {
            let y = list.y + (i - self.top) as u32 * rh;
            if y >= list.y + list.h {
                break;
            }
            let h = rh.min(list.y + list.h - y);
            let row = Rect {
                x: 0,
                y,
                w: list.w,
                h,
            };
            if self.sel == Some(i) {
                fill(&mut self.pixmap, row, SEL);
            }
            let color = if self.playing == Some(i) {
                PLAYING
            } else {
                TEXT
            };
            let title = if self.playing == Some(i) {
                format!("▶ {}", self.rows[i].title)
            } else {
                self.rows[i].title.clone()
            };
            let dur = if self.rows[i].secs == 0 {
                String::from("—")
            } else {
                mmss(u64::from(self.rows[i].secs) * 1000)
            };
            let fmt = self.rows[i].format;
            let pad = self.px(8.0);
            self.text(
                Rect {
                    x: cols[0].x,
                    y,
                    w: cols[0].w,
                    h,
                },
                pad,
                &title,
                color,
            );
            self.text(
                Rect {
                    x: cols[1].x,
                    y,
                    w: cols[1].w,
                    h,
                },
                pad,
                &dur,
                color,
            );
            self.text(
                Rect {
                    x: cols[2].x,
                    y,
                    w: cols[2].w,
                    h,
                },
                pad,
                fmt,
                color,
            );
        }
        // 滚动条
        let total = self.rows.len().max(1) as f32;
        let track = list.h as f32;
        let thumb_h =
            (track * self.visible_rows() as f32 / total).clamp(self.px(20.0) as f32, track);
        let thumb_y =
            list.y as f32 + (track - thumb_h) * self.top as f32 / self.max_top().max(1) as f32;
        let sw = self.px(6.0);
        fill(
            &mut self.pixmap,
            Rect {
                x: list.w.saturating_sub(sw + 2),
                y: thumb_y as u32,
                w: sw,
                h: thumb_h as u32,
            },
            0xb8b8b8,
        );
    }

    fn paint_status(&mut self) {
        let s = self.status_rect();
        fill(&mut self.pixmap, s, 0xececec);
        hline(&mut self.pixmap, s.y, s.w, LINE);
        let text = match self.playing {
            None => String::from("已停止"),
            Some(i) => {
                let r = &self.rows[i];
                format!(
                    "{} 第 {} 首  {} / {}  ·  {}  ·  已协商，读回一致  ·  欠载 0 次",
                    if self.paused {
                        "已暂停"
                    } else {
                        "播放中"
                    },
                    i + 1,
                    mmss(self.pos_ms),
                    mmss(u64::from(r.secs) * 1000),
                    r.format
                )
            }
        };
        self.text(s, self.px(8.0), &text, TEXT);
    }

    fn text_centered(&mut self, cell: Rect, s: &str, color: u32) {
        let w = self.measure(s);
        let pad = cell.w.saturating_sub(w) / 2;
        self.text(cell, pad, s, color);
    }

    fn metrics(&self) -> Metrics {
        let size = 14.0 * self.scale;
        Metrics::new(size, (size * 1.3).ceil())
    }

    /// 排版结果按字符串缓存（列表滚动时同一行的文字不再重新排版）；缩放变化时清空。
    fn ensure_layout(&mut self, s: &str) {
        if self.layouts.contains_key(s) {
            return;
        }
        if self.layouts.len() >= 4096 {
            self.layouts.clear();
        }
        let m = self.metrics();
        let mut b = Buffer::new(&mut self.fonts, m);
        b.set_wrap(&mut self.fonts, Wrap::None);
        b.set_size(&mut self.fonts, None, Some(m.line_height));
        b.set_text(
            &mut self.fonts,
            s,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.layouts.insert(s.to_owned(), b);
    }

    #[allow(dead_code)] // 只有 winit 后端会收到缩放变化（x11rb 后端在启动时读取 Xft.dpi）
    pub fn set_scale(&mut self, scale: f32) {
        if scale != self.scale {
            self.scale = scale;
            self.layouts.clear();
        }
    }

    fn measure(&mut self, s: &str) -> u32 {
        self.ensure_layout(s);
        self.layouts[s]
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0, f32::max)
            .ceil() as u32
    }

    /// 在单元格内左起 `pad` 处画一行文字，垂直居中，超出单元格的部分裁掉。
    fn text(&mut self, cell: Rect, pad: u32, s: &str, color: u32) {
        self.ensure_layout(s);
        let lh = self.metrics().line_height as i32;
        let x0 = (cell.x + pad) as i32;
        let y0 = cell.y as i32 + (cell.h as i32 - lh) / 2;
        let clip_x1 = (cell.x + cell.w).saturating_sub(pad / 2) as i32;
        let clip_y1 = (cell.y + cell.h) as i32;
        let App {
            layouts,
            fonts,
            glyphs,
            pixmap,
            ..
        } = self;
        let b = &layouts[s];
        let (cw, data) = (pixmap.width() as i32, pixmap.data_mut());
        let c = Color::rgb((color >> 16) as u8, (color >> 8) as u8, color as u8);
        b.draw(fonts, glyphs, c, |gx, gy, w, h, col| {
            let a = u32::from(col.a());
            if a == 0 {
                return;
            }
            for yy in gy..gy + h as i32 {
                for xx in gx..gx + w as i32 {
                    let (px, py) = (x0 + xx, y0 + yy);
                    if px < cell.x as i32 || py < cell.y as i32 || px >= clip_x1 || py >= clip_y1 {
                        continue;
                    }
                    let o = ((py * cw + px) * 4) as usize;
                    if let Some(p) = data.get_mut(o..o + 3) {
                        for (d, s) in p.iter_mut().zip([col.r(), col.g(), col.b()]) {
                            *d = ((u32::from(*d) * (255 - a) + u32::from(s) * a) / 255) as u8;
                        }
                    }
                }
            }
        });
    }
}

fn new_pixmap(w: u32, h: u32) -> Pixmap {
    match Pixmap::new(w.max(1), h.max(1)) {
        Some(p) => p,
        None => panic!("窗口尺寸 {w}×{h} 超出范围"),
    }
}

fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// 不透明填充（裁到画布内）。
fn fill(p: &mut Pixmap, r: Rect, rgb: u32) {
    let (w, h) = (p.width(), p.height());
    let x1 = (r.x + r.w).min(w);
    let y1 = (r.y + r.h).min(h);
    let px = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255];
    let data = p.data_mut();
    for y in r.y.min(h)..y1 {
        let row = (y * w) as usize * 4;
        for x in r.x.min(w)..x1 {
            let o = row + x as usize * 4;
            data[o..o + 4].copy_from_slice(&px);
        }
    }
}

fn hline(p: &mut Pixmap, y: u32, w: u32, rgb: u32) {
    fill(p, Rect { x: 0, y, w, h: 1 }, rgb);
}

/// 圆角按钮：tiny-skia 抗锯齿填充与描边。
fn rounded(p: &mut Pixmap, r: Rect, radius: f32, fill_rgb: u32, edge: u32) {
    let (x, y, w, h) = (
        r.x as f32 + 0.5,
        r.y as f32 + 0.5,
        r.w as f32 - 1.0,
        r.h as f32 - 1.0,
    );
    let k = radius.min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + k, y);
    pb.line_to(x + w - k, y);
    pb.quad_to(x + w, y, x + w, y + k);
    pb.line_to(x + w, y + h - k);
    pb.quad_to(x + w, y + h, x + w - k, y + h);
    pb.line_to(x + k, y + h);
    pb.quad_to(x, y + h, x, y + h - k);
    pb.line_to(x, y + k);
    pb.quad_to(x, y, x + k, y);
    pb.close();
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color_rgba8(
        (fill_rgb >> 16) as u8,
        (fill_rgb >> 8) as u8,
        fill_rgb as u8,
        255,
    );
    p.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    paint.set_color_rgba8((edge >> 16) as u8, (edge >> 8) as u8, edge as u8, 255);
    let stroke = tiny_skia::Stroke {
        width: 1.0,
        ..Default::default()
    };
    p.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// 测量模式（`QY_PROTO_BENCH=1`）：滚动 200 次、状态行 200 次，后端每次提交后计时。
pub fn bench_inputs() -> impl Iterator<Item = Input> {
    (0..200)
        .map(|i| Input::Wheel(if (i / 50) % 2 == 0 { 1 } else { -1 }))
        .chain((0..200).map(|_| Input::Tick))
}

/// 环境变量开关。
pub fn env_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}
