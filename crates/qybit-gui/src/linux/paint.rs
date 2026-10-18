//! 软件绘制：画布（RGBA 像素）、基本图形与文字。
//!
//! - 形状用 tiny-skia（抗锯齿的圆角、三角形、勾）；矩形与线直接写像素。
//! - 文字用 cosmic-text：排版与字体回退（fontdb 直接读系统字体目录与 fontconfig 配置文件）、
//!   swash 光栅化。单行文字的排版结果按字符串缓存（列表滚动时同一行不再重新排版）。
//! - 所有绘制都裁剪到给定矩形与画布之内，坐标越界不会 panic。

use std::collections::HashMap;

use cosmic_text::{
    Attrs, Buffer, Color, Family, FontSystem, LayoutRun, Metrics, Shaping, SwashCache, Wrap,
};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// 物理像素矩形。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// 两个矩形的交集（不相交时为空矩形）。
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect::new(x, y, (r - x).max(0), (b - y).max(0))
    }

    /// 包含两个矩形的最小矩形（空矩形不计入）。
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = self.right().max(o.right());
        let b = self.bottom().max(o.bottom());
        Rect::new(x, y, r - x, b - y)
    }

    /// 四边各缩进 `d`。
    pub fn inset(&self, d: i32) -> Rect {
        Rect::new(
            self.x + d,
            self.y + d,
            (self.w - 2 * d).max(0),
            (self.h - 2 * d).max(0),
        )
    }
}

/// 文字的水平对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// 单行文字缓存的上限（超过时清空重建）。
const TEXT_CACHE: usize = 4096;

pub struct Canvas {
    pub pix: Pixmap,
    fonts: FontSystem,
    glyphs: SwashCache,
    /// 单行文字的排版结果：键为（文字，是否粗体）。
    lines: HashMap<(String, bool), Buffer>,
    scale: f32,
}

impl Canvas {
    pub fn new(width: u32, height: u32, scale: f32) -> Self {
        Self {
            pix: new_pixmap(width, height),
            fonts: FontSystem::new(),
            glyphs: SwashCache::new(),
            lines: HashMap::new(),
            scale,
        }
    }

    /// 系统中是否有任何字体（一个都没有时无法排版文字，cosmic-text 会 panic）。
    pub fn has_fonts(&self) -> bool {
        self.fonts.db().faces().next().is_some()
    }

    /// 系统字体中是否有能显示常用汉字的字体（没有时界面中文显示为方块，前端在日志中说明）。
    pub fn has_cjk_font(&mut self) -> bool {
        let db = self.fonts.db();
        db.faces().any(|f| {
            f.families.iter().any(|(n, _)| {
                [
                    "CJK",
                    "Hei",
                    "Song",
                    "Kai",
                    "Ming",
                    "Gothic",
                    "WenQuanYi",
                    "Han",
                ]
                .iter()
                .any(|k| n.contains(k))
            })
        })
    }

    pub fn width(&self) -> i32 {
        self.pix.width() as i32
    }

    pub fn height(&self) -> i32 {
        self.pix.height() as i32
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width != self.pix.width() || height != self.pix.height() {
            self.pix = new_pixmap(width, height);
        }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width(), self.height())
    }

    fn metrics(&self) -> Metrics {
        let size = 13.0 * self.scale;
        Metrics::new(size, (size * 1.35).ceil())
    }

    /// 一行文字的高度。
    pub fn line_height(&self) -> i32 {
        self.metrics().line_height as i32
    }

    fn ensure_line(&mut self, s: &str, bold: bool) {
        let key = (s.to_owned(), bold);
        if self.lines.contains_key(&key) {
            return;
        }
        if self.lines.len() >= TEXT_CACHE {
            self.lines.clear();
        }
        let m = self.metrics();
        let mut b = Buffer::new(&mut self.fonts, m);
        b.set_wrap(&mut self.fonts, Wrap::None);
        b.set_size(&mut self.fonts, None, Some(m.line_height));
        let attrs = Attrs::new().family(Family::SansSerif);
        let attrs = if bold {
            attrs.weight(cosmic_text::Weight::BOLD)
        } else {
            attrs
        };
        b.set_text(&mut self.fonts, s, &attrs, Shaping::Advanced, None);
        self.lines.insert(key, b);
    }

    /// 一行文字的宽度（像素）。
    pub fn text_width(&mut self, s: &str) -> i32 {
        self.ensure_line(s, false);
        self.lines.get(&(s.to_owned(), false)).map_or(0, |b| {
            b.layout_runs().map(|r| r.line_w).fold(0.0, f32::max).ceil() as i32
        })
    }

    /// 在 `r` 内画一行文字（左右各留 `pad`，垂直居中）；放不下时截断并加“…”。
    pub fn text(&mut self, r: Rect, pad: i32, s: &str, color: u32, align: Align) {
        self.text_styled(r, pad, s, color, align, false);
    }

    pub fn text_bold(&mut self, r: Rect, pad: i32, s: &str, color: u32, align: Align) {
        self.text_styled(r, pad, s, color, align, true);
    }

    fn text_styled(&mut self, r: Rect, pad: i32, s: &str, color: u32, align: Align, bold: bool) {
        if s.is_empty() || r.is_empty() {
            return;
        }
        let avail = (r.w - 2 * pad).max(0);
        let full = self.width_of(s, bold);
        let fitted;
        let shown = if full <= avail {
            s
        } else {
            fitted = self.fit(s, avail, bold);
            fitted.as_str()
        };
        let w = self.width_of(shown, bold);
        let x = match align {
            Align::Left => r.x + pad,
            Align::Center => r.x + (r.w - w) / 2,
            Align::Right => r.right() - pad - w,
        };
        let lh = self.line_height();
        let y = r.y + (r.h - lh) / 2;
        let clip = r.intersect(&self.bounds());
        self.ensure_line(shown, bold);
        let Canvas {
            pix,
            fonts,
            glyphs,
            lines,
            ..
        } = self;
        if let Some(b) = lines.get(&(shown.to_owned(), bold)) {
            for run in b.layout_runs() {
                draw_run(pix, fonts, glyphs, &run, x, y, clip, color);
            }
        }
    }

    fn width_of(&mut self, s: &str, bold: bool) -> i32 {
        self.ensure_line(s, bold);
        self.lines.get(&(s.to_owned(), bold)).map_or(0, |b| {
            b.layout_runs().map(|r| r.line_w).fold(0.0, f32::max).ceil() as i32
        })
    }

    /// 截断到 `avail` 像素以内并加“…”。
    fn fit(&mut self, s: &str, avail: i32, bold: bool) -> String {
        let ell = self.width_of("…", bold);
        let limit = (avail - ell).max(0) as f32;
        self.ensure_line(s, bold);
        let mut cut = 0;
        if let Some(b) = self.lines.get(&(s.to_owned(), bold)) {
            for run in b.layout_runs() {
                for g in run.glyphs {
                    if g.x + g.w > limit {
                        break;
                    }
                    cut = cut.max(g.end);
                }
            }
        }
        let cut = (0..=cut.min(s.len()))
            .rev()
            .find(|&i| s.is_char_boundary(i))
            .unwrap_or(0);
        format!("{}…", &s[..cut])
    }

    /// 按宽度折行排版一段文字（日志区）。
    pub fn wrapped(&mut self, s: &str, width: i32) -> Buffer {
        let m = self.metrics();
        let mut b = Buffer::new(&mut self.fonts, m);
        b.set_wrap(&mut self.fonts, Wrap::WordOrGlyph);
        b.set_size(&mut self.fonts, Some(width.max(1) as f32), None);
        b.set_text(
            &mut self.fonts,
            s,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        b
    }

    /// 画折行排版的文字：左上角在 (x, y)，只画与 `clip` 相交的行。
    pub fn draw_wrapped(&mut self, b: &Buffer, x: i32, y: i32, clip: Rect, color: u32) {
        let clip = clip.intersect(&self.bounds());
        let Canvas {
            pix, fonts, glyphs, ..
        } = self;
        for run in b.layout_runs() {
            let top = y + run.line_top as i32;
            let bottom = top + run.line_height as i32;
            if bottom <= clip.y || top >= clip.bottom() {
                continue;
            }
            draw_run(pix, fonts, glyphs, &run, x, y, clip, color);
        }
    }

    /// 不透明填充。
    pub fn fill(&mut self, r: Rect, rgb: u32) {
        let c = r.intersect(&self.bounds());
        if c.is_empty() {
            return;
        }
        let w = self.pix.width() as usize;
        let px = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255];
        let data = self.pix.data_mut();
        for y in c.y..c.bottom() {
            let row = y as usize * w;
            for x in c.x..c.right() {
                let o = (row + x as usize) * 4;
                if let Some(p) = data.get_mut(o..o + 4) {
                    p.copy_from_slice(&px);
                }
            }
        }
    }

    /// 1 像素边框。
    pub fn frame(&mut self, r: Rect, rgb: u32) {
        self.fill(Rect::new(r.x, r.y, r.w, 1), rgb);
        self.fill(Rect::new(r.x, r.bottom() - 1, r.w, 1), rgb);
        self.fill(Rect::new(r.x, r.y, 1, r.h), rgb);
        self.fill(Rect::new(r.right() - 1, r.y, 1, r.h), rgb);
    }

    /// 圆角矩形：填充与 1 像素边框。
    pub fn rounded(&mut self, r: Rect, radius: f32, fill: u32, edge: Option<u32>) {
        let (x, y, w, h) = (
            r.x as f32 + 0.5,
            r.y as f32 + 0.5,
            r.w as f32 - 1.0,
            r.h as f32 - 1.0,
        );
        if w <= 0.0 || h <= 0.0 {
            return;
        }
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
        let mut paint = paint(fill);
        self.pix.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        if let Some(e) = edge {
            set_color(&mut paint, e);
            let stroke = Stroke {
                width: 1.0,
                ..Default::default()
            };
            self.pix
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    /// 实心多边形（图标：三角形等）。
    pub fn polygon(&mut self, pts: &[(f32, f32)], rgb: u32) {
        let mut pb = PathBuilder::new();
        let Some(&(x0, y0)) = pts.first() else { return };
        pb.move_to(x0, y0);
        for &(x, y) in &pts[1..] {
            pb.line_to(x, y);
        }
        pb.close();
        let Some(path) = pb.finish() else { return };
        self.pix.fill_path(
            &path,
            &paint(rgb),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    /// 折线（勾选标记等）。
    pub fn polyline(&mut self, pts: &[(f32, f32)], width: f32, rgb: u32) {
        let mut pb = PathBuilder::new();
        let Some(&(x0, y0)) = pts.first() else { return };
        pb.move_to(x0, y0);
        for &(x, y) in &pts[1..] {
            pb.line_to(x, y);
        }
        let Some(path) = pb.finish() else { return };
        let stroke = Stroke {
            width,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        self.pix
            .stroke_path(&path, &paint(rgb), &stroke, Transform::identity(), None);
    }

    /// 实心圆（指示灯）。
    pub fn dot(&mut self, cx: f32, cy: f32, r: f32, rgb: u32) {
        let Some(path) = PathBuilder::from_circle(cx, cy, r) else {
            return;
        };
        self.pix.fill_path(
            &path,
            &paint(rgb),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn paint(rgb: u32) -> Paint<'static> {
    let mut p = Paint {
        anti_alias: true,
        ..Default::default()
    };
    set_color(&mut p, rgb);
    p
}

fn set_color(p: &mut Paint<'_>, rgb: u32) {
    p.set_color_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255);
}

fn new_pixmap(w: u32, h: u32) -> Pixmap {
    // 尺寸为 0 或过大时退到 1×1（窗口最小化等）；tiny-skia 对 1×1 不会失败，没有不 panic 的
    // 其他出口时只能中止。
    match Pixmap::new(w.clamp(1, 16384), h.clamp(1, 16384)).or_else(|| Pixmap::new(1, 1)) {
        Some(p) => p,
        None => std::process::abort(),
    }
}

/// 画一行排版结果：(x, y) 为该段文字左上角，按覆盖度与背景混合。
#[allow(clippy::too_many_arguments)]
fn draw_run(
    pix: &mut Pixmap,
    fonts: &mut FontSystem,
    glyphs: &mut SwashCache,
    run: &LayoutRun<'_>,
    x: i32,
    y: i32,
    clip: Rect,
    rgb: u32,
) {
    let w = pix.width() as i32;
    let data = pix.data_mut();
    let base = Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    for g in run.glyphs {
        let pg = g.physical((x as f32, y as f32 + run.line_y), 1.0);
        glyphs.with_pixels(fonts, pg.cache_key, base, |dx, dy, c| {
            let a = u32::from(c.a());
            if a == 0 {
                return;
            }
            let (px, py) = (pg.x + dx, pg.y + dy);
            if !clip.contains(px, py) {
                return;
            }
            let o = ((py * w + px) * 4) as usize;
            if let Some(p) = data.get_mut(o..o + 3) {
                for (d, s) in p.iter_mut().zip([c.r(), c.g(), c.b()]) {
                    *d = ((u32::from(*d) * (255 - a) + u32::from(s) * a) / 255) as u8;
                }
            }
        });
    }
}
