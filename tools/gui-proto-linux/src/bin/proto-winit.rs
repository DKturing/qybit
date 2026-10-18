//! 窗口后端 A：winit 0.30 + softbuffer 0.4（X11 与 Wayland；系统库运行时加载）。
//! 环境变量：`QY_PROTO_STATIC=1` 不刷新状态行；`QY_PROTO_BENCH=1` 运行测量后退出。

#[path = "../ui.rs"]
mod ui;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use ui::{App, Input, Key, Region, TICK};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{Window, WindowId};

struct Prog {
    start: Instant,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    app: Option<App>,
    cursor: (f32, f32),
    pending: Option<Region>,
    next_tick: Instant,
    still: bool,
    bench: bool,
    first: bool,
    present_total: Duration,
    presents: u32,
}

impl Prog {
    fn present(&mut self, region: Region) {
        let (Some(app), Some(surface)) = (self.app.as_mut(), self.surface.as_mut()) else {
            return;
        };
        let rect = app.paint(region);
        let t = Instant::now();
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => panic!("softbuffer：{e}"),
        };
        let w = app.width() as usize;
        let src = app.pixmap.data();
        // 缓冲内容不是上一帧时（age ≠ 1）整幅复制，否则只复制变化的区域。
        let rect = if buf.age() == 1 {
            rect
        } else {
            ui::Rect {
                x: 0,
                y: 0,
                w: app.width(),
                h: app.height(),
            }
        };
        for y in rect.y..rect.y + rect.h {
            for x in rect.x..rect.x + rect.w {
                let i = y as usize * w + x as usize;
                let p = &src[i * 4..i * 4 + 3];
                buf[i] = (u32::from(p[0]) << 16) | (u32::from(p[1]) << 8) | u32::from(p[2]);
            }
        }
        let damage = softbuffer::Rect {
            x: rect.x,
            y: rect.y,
            width: NonZeroU32::new(rect.w.max(1)).unwrap_or(NonZeroU32::MIN),
            height: NonZeroU32::new(rect.h.max(1)).unwrap_or(NonZeroU32::MIN),
        };
        if let Err(e) = buf.present_with_damage(&[damage]) {
            panic!("softbuffer 提交：{e}");
        }
        self.present_total += t.elapsed();
        self.presents += 1;
        if self.first {
            self.first = false;
            eprintln!(
                "首帧：{:.1} ms（其中加载字体库 {:.1} ms）",
                self.start.elapsed().as_secs_f64() * 1000.0,
                app.font_load.as_secs_f64() * 1000.0
            );
        }
    }

    fn input(&mut self, i: Input) {
        if let Some(app) = self.app.as_mut()
            && let Some(r) = app.update(i)
        {
            self.pending = Some(match self.pending {
                None => r,
                Some(p) if p == r => r,
                Some(_) => Region::All,
            });
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn report(&self) {
        if let Some(app) = &self.app {
            eprintln!("{}", app.full.line("整窗重绘"));
            eprintln!("{}", app.list.line("列表重绘"));
            eprintln!("{}", app.status.line("状态行重绘"));
            if self.presents > 0 {
                eprintln!(
                    "提交（复制到 softbuffer 并 present）：{} 次，平均 {:.2} ms",
                    self.presents,
                    self.present_total.as_secs_f64() * 1000.0 / f64::from(self.presents)
                );
            }
        }
    }
}

impl ApplicationHandler for Prog {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("QyBit 原型（winit）")
            .with_inner_size(LogicalSize::new(900.0, 600.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => panic!("创建窗口：{e}"),
        };
        let ctx = match Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => panic!("softbuffer：{e}"),
        };
        let mut surface = match Surface::new(&ctx, window.clone()) {
            Ok(s) => s,
            Err(e) => panic!("softbuffer：{e}"),
        };
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        if let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) {
            let _ = surface.resize(nw, nh);
        }
        self.app = Some(App::new(w, h, window.scale_factor() as f32));
        self.surface = Some(surface);
        self.window = Some(window.clone());
        self.pending = Some(Region::All);
        window.request_redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.report();
                el.exit();
            }
            WindowEvent::Resized(size) => {
                let (w, h) = (size.width.max(1), size.height.max(1));
                if let (Some(s), Some(nw), Some(nh)) = (
                    self.surface.as_mut(),
                    NonZeroU32::new(w),
                    NonZeroU32::new(h),
                ) {
                    let _ = s.resize(nw, nh);
                }
                self.input(Input::Resize(w, h));
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(app) = self.app.as_mut() {
                    app.set_scale(scale_factor as f32);
                }
                self.pending = Some(Region::All);
            }
            WindowEvent::RedrawRequested => {
                let r = self.pending.take().unwrap_or(Region::All);
                self.present(r);
                if self.bench {
                    self.bench = false;
                    let t = Instant::now();
                    for i in ui::bench_inputs() {
                        if let Some(r) = self.app.as_mut().and_then(|a| a.update(i)) {
                            self.present(r);
                        }
                    }
                    eprintln!(
                        "测量：400 次输入共 {:.1} ms",
                        t.elapsed().as_secs_f64() * 1000.0
                    );
                    self.report();
                    el.exit();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.input(Input::Click(self.cursor.0, self.cursor.1));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y.signum() as i32,
                    MouseScrollDelta::PixelDelta(p) => -(p.y / 26.0) as i32,
                };
                if lines != 0 {
                    self.input(Input::Wheel(lines));
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let k = match event.logical_key {
                    WKey::Named(NamedKey::ArrowUp) => Key::Up,
                    WKey::Named(NamedKey::ArrowDown) => Key::Down,
                    WKey::Named(NamedKey::PageUp) => Key::PageUp,
                    WKey::Named(NamedKey::PageDown) => Key::PageDown,
                    WKey::Named(NamedKey::Home) => Key::Home,
                    WKey::Named(NamedKey::End) => Key::End,
                    WKey::Named(NamedKey::Enter) => Key::Enter,
                    WKey::Named(NamedKey::Space) => Key::Space,
                    _ => return,
                };
                self.input(Input::Key(k));
            }
            WindowEvent::DroppedFile(path) => self.input(Input::Drop(vec![path])),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.still {
            el.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        if now >= self.next_tick {
            self.input(Input::Tick);
            while self.next_tick <= now {
                self.next_tick += TICK;
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }
}

fn main() {
    let start = Instant::now();
    let el = match EventLoop::new() {
        Ok(e) => e,
        Err(e) => panic!("事件循环：{e}"),
    };
    let mut prog = Prog {
        start,
        window: None,
        surface: None,
        app: None,
        cursor: (0.0, 0.0),
        pending: None,
        next_tick: Instant::now() + TICK,
        still: ui::env_on("QY_PROTO_STATIC"),
        bench: ui::env_on("QY_PROTO_BENCH"),
        first: true,
        present_total: Duration::ZERO,
        presents: 0,
    };
    if let Err(e) = el.run_app(&mut prog) {
        panic!("{e}");
    }
}
