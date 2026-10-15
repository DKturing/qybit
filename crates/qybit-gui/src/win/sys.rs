//! Win32 边界：窗口、控件、消息循环、文件对话框与拖放。本前端的全部 `unsafe` 集中在此，
//! 每处注明前提。界面逻辑在 [`super::app`]（安全代码）：窗口过程把消息转成 [`Input`]，在
//! 不持有状态借用的情况下执行 [`App`] 返回的 [`Op`]，因此控件同步回发的消息不会重入状态。
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT, HGDIOBJ, InvalidateRect,
    ScreenToClient,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BST_CHECKED, EM_SCROLLCARET, EM_SETSEL, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES,
    ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, LVCF_TEXT, LVCF_WIDTH,
    LVCOLUMNW, LVIF_TEXT, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_SETCOLUMNWIDTH,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMCOUNT, LVN_GETDISPINFOW, LVN_KEYDOWN, LVNI_SELECTED,
    LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, LVS_OWNERDATA, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVSICF_NOSCROLL, NM_DBLCLK, NMHDR, NMITEMACTIVATE, NMLVDISPINFOW,
    NMLVKEYDOWN, SB_SETPARTS, SB_SETTEXTW, STATUSCLASSNAMEW, TB_ENDTRACK, TB_PAGEDOWN, TB_PAGEUP,
    TB_THUMBTRACK, TBM_GETCHANNELRECT, TBM_GETTHUMBRECT, TBM_SETPOS, TBM_SETRANGEMAX,
    TBM_SETRANGEMIN, TBS_HORZ, TBS_NOTICKS, TRACKBAR_CLASSW, WC_COMBOBOXW, WC_LISTVIEWW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
    SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, VK_DELETE, VK_LBUTTON, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, HDROP, IFileOpenDialog,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BM_GETCHECK, BS_AUTOCHECKBOX, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL,
    CB_SETCURSEL, CBN_SELCHANGE, CBS_DROPDOWNLIST, CW_USEDEFAULT, CreateMenu, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ES_AUTOVSCROLL, ES_MULTILINE,
    ES_READONLY, GetClientRect, GetCursorPos, GetMessageW, HMENU, IDC_ARROW, LoadCursorW,
    MB_ICONINFORMATION, MB_OK, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, MessageBoxW, MoveWindow,
    NONCLIENTMETRICSW, PostMessageW, PostQuitMessage, RegisterClassW, SPI_GETNONCLIENTMETRICS,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetMenu, SetWindowPos, SetWindowTextW,
    ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WM_DPICHANGED, WM_DROPFILES, WM_HSCROLL, WM_NOTIFY, WM_SETFONT,
    WM_SIZE, WNDCLASSW, WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{PCWSTR, PWSTR, w};

use super::app::{
    App, COLUMNS, ID_ABOUT, ID_ADD_FILES, ID_ADD_FOLDER, ID_CLEAR, ID_EXIT, ID_NEXT, ID_PLAY,
    ID_PREV, ID_REMOVE, ID_STOP, IDC_DEVICE, IDC_FORMAT, IDC_LIST, IDC_LOG, IDC_MONO, IDC_SEEK,
    IDC_STATUS, IDC_VOLUME, IDC_VOLUME_LABEL, Input, Op, VolumeView, about_text,
};
use super::sim::Sim;
use qy_host_wasapi::volume::EndpointVolume;

/// STATIC 控件样式（winuser.h：`SS_LEFT` 0x0、`SS_NOPREFIX` 0x80、`SS_ENDELLIPSIS` 0x4000）。
/// windows crate 把它们放在 System_SystemServices 模块，为三个常量不引入该特性。
const SS_LEFT: u32 = 0x0000;
/// winuser.h：`SS_RIGHT` 0x2。
const SS_RIGHT: u32 = 0x0002;
const SS_NOPREFIX: u32 = 0x0080;
const SS_ENDELLIPSIS: u32 = 0x4000;
/// 进度条当前位置（commctrl.h：`TBM_GETPOS` 定义为 `WM_USER`，0x400）；windows crate 未导出。
const TBM_GETPOS: u32 = 0x0400;

/// 后台线程有新事件（`PostMessageW` 投递，跨线程安全）。
const WM_QY_NOTIFY: u32 = WM_APP + 1;

/// 主窗口与控件句柄（创建后不变）。
#[derive(Clone, Copy)]
struct Handles {
    main: HWND,
    play: HWND,
    buttons: [HWND; 4],
    device: HWND,
    mono: HWND,
    list: HWND,
    log: HWND,
    status: HWND,
    format: HWND,
    seek: HWND,
    volume: HWND,
    volume_label: HWND,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static HANDLES: Cell<Option<Handles>> = const { Cell::new(None) };
    static FONT: Cell<HFONT> = const { Cell::new(HFONT(std::ptr::null_mut())) };
    /// 状态栏是否分两栏（第二栏为保真状态；未播放时只有一栏，结论文字可用整行）。
    static TWO_PARTS: Cell<bool> = const { Cell::new(false) };
    /// 正在拖动或按住点击进度条（到 `TB_ENDTRACK` 为止）：此间不按播放状态移动滑块，否则松开时
    /// 读到的是被状态快照拉回的当前位置（S1）。
    static DRAGGING: Cell<bool> = const { Cell::new(false) };
    /// 当前输出设备的音量控制（GUI T5）；打开失败时为原因。
    static VOLUME: RefCell<Option<Result<EndpointVolume, String>>> = const { RefCell::new(None) };
    /// 正在拖动音量滑块：此间不按读回的音量移动滑块。
    static VOL_DRAGGING: Cell<bool> = const { Cell::new(false) };
    /// 模拟设备模式（只供界面核对）：不打开真实端点的音量。
    static SIM_MODE: Cell<bool> = const { Cell::new(false) };
    /// 音量标签上一次的文字（相同时不重设，避免闪烁）。
    static VOL_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// 跨线程通知状态：主窗口句柄（整数形式）与“已投递未处理”标志（合并连续通知）。
static MAIN: AtomicIsize = AtomicIsize::new(0);
static PENDING: AtomicBool = AtomicBool::new(false);

/// 打开当前选定设备的音量控制并显示（GUI T5）。在界面线程调用（COM 单线程套间已初始化）。
fn open_volume() {
    let opened = if SIM_MODE.get() {
        Err("模拟设备，没有音量控制".to_string())
    } else {
        let id = APP.with(|a| a.try_borrow().ok().and_then(|g| g.as_ref()?.device_id()));
        EndpointVolume::open(id.as_deref()).map_err(|e| e.message())
    };
    VOLUME.with_borrow_mut(|v| *v = Some(opened));
    show_volume();
}

/// 读回音量并更新标签与滑块（拖动中不移动滑块）。
fn show_volume() {
    let Some(h) = HANDLES.get() else {
        return;
    };
    let view = VOLUME.with_borrow(|v| match v {
        None => VolumeView::Unavailable("尚未打开".to_string()),
        Some(Err(e)) => VolumeView::Unavailable(e.clone()),
        Some(Ok(ev)) if !ev.hardware_volume() => VolumeView::NoHardware,
        Some(Ok(ev)) => match ev.level() {
            Ok(l) => VolumeView::Hardware {
                scalar: l.scalar,
                db: l.db,
                muted: l.muted,
            },
            Err(e) => VolumeView::Unavailable(e.message()),
        },
    });
    let text = view.text();
    let changed = VOL_TEXT.with_borrow_mut(|t| {
        let changed = *t != text;
        if changed {
            t.clone_from(&text);
        }
        changed
    });
    let pos = view.position();
    // SAFETY: 句柄有效；文本缓冲在调用期间有效。
    unsafe {
        if changed {
            let t = wide(&text);
            let _ = SetWindowTextW(h.volume_label, PCWSTR(t.as_ptr()));
        }
        let _ = EnableWindow(h.volume, pos.is_some());
        if !VOL_DRAGGING.get() {
            SendMessageW(
                h.volume,
                TBM_SETPOS,
                Some(WPARAM(1)),
                Some(LPARAM(pos.unwrap_or(0) as isize)),
            );
        }
    }
}

/// 设置硬件音量（0–100）。设备没有硬件音量时 [`EndpointVolume::set_scalar`] 拒绝，不改变
/// 软件音量。
fn set_volume(pos: u32) {
    VOLUME.with_borrow(|v| {
        if let Some(Ok(ev)) = v {
            let _ = ev.set_scalar(pos.min(100) as f32 / 100.0);
        }
    });
    show_volume();
}

/// 拖动或按住进度条时在状态栏第一栏显示“松开后定位到 …”。
fn seek_preview(h: Handles, permille: u32) {
    let preview = APP.with(|a| {
        a.try_borrow()
            .ok()
            .and_then(|g| g.as_ref()?.seek_preview(permille))
    });
    if let Some(t) = preview {
        let t = wide(&t);
        // SAFETY: 状态栏句柄有效；文本缓冲在调用期间有效。
        unsafe {
            SendMessageW(
                h.status,
                SB_SETTEXTW,
                Some(WPARAM(0)),
                Some(LPARAM(t.as_ptr() as isize)),
            );
        }
    }
}

/// 鼠标在滑轨上的位置（0–`max`）。滑块中心的行程是滑轨两端各减去半个滑块宽度
/// （`TBM_GETCHANNELRECT`、`TBM_GETTHUMBRECT`，客户区坐标）。
fn click_position(seek: HWND, max: i64) -> Option<u32> {
    let mut pt = POINT::default();
    let (mut channel, mut thumb) = (RECT::default(), RECT::default());
    // SAFETY: 输出参数指向本函数的局部变量；进度条句柄有效。
    unsafe {
        GetCursorPos(&mut pt).ok()?;
        if !ScreenToClient(seek, &mut pt).as_bool() {
            return None;
        }
        SendMessageW(
            seek,
            TBM_GETCHANNELRECT,
            None,
            Some(LPARAM(&mut channel as *mut RECT as isize)),
        );
        SendMessageW(
            seek,
            TBM_GETTHUMBRECT,
            None,
            Some(LPARAM(&mut thumb as *mut RECT as isize)),
        );
    }
    let half = (thumb.right - thumb.left) / 2;
    let (start, end) = (channel.left + half, channel.right - half);
    if end <= start {
        return None;
    }
    let x = pt.x.clamp(start, end);
    u32::try_from(i64::from(x - start) * max / i64::from(end - start)).ok()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}

fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}

/// 后台线程调用：投递一次通知（已有未处理的通知时不再投递）。
fn notify() {
    if PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let raw = MAIN.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: `raw` 是本进程主窗口的句柄值；PostMessageW 可从任意线程调用，窗口已销毁时
        // 只返回错误。
        unsafe {
            let _ = PostMessageW(
                Some(HWND(raw as *mut _)),
                WM_QY_NOTIFY,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub fn run() -> ExitCode {
    // SAFETY: 进程启动时设置 DPI 感知与 COM（单线程套间，文件对话框要求）；与本线程退出前的
    // CoUninitialize 配对。
    let com = unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok()
    };
    let sim = Sim::from_env();
    // 端点枚举使用多线程套间：放在单独的线程中完成。
    let (default, endpoints) = match &sim {
        Some(s) => (Some(s.endpoint()), Vec::new()),
        None => std::thread::spawn(|| {
            (
                qy_host_wasapi::endpoints::default_render_endpoint().ok(),
                qy_host_wasapi::endpoints::list_render_endpoints().unwrap_or_default(),
            )
        })
        .join()
        .unwrap_or_default(),
    };
    let title = sim.as_ref().map_or_else(|| "QyBit".to_string(), Sim::title);
    let code = match create(&title) {
        Some(h) => {
            let (ui, sim_files) = match &sim {
                Some(s) => (s.ui(Arc::new(notify)), s.files()),
                None => (wasapi_ui(), Vec::new()),
            };
            let app = App::new(ui, default, endpoints);
            fill_devices(&h, &app.device_names());
            let ops = app.initial();
            APP.with_borrow_mut(|a| *a = Some(app));
            for op in ops {
                exec(op);
            }
            SIM_MODE.set(sim.is_some());
            open_volume();
            // 命令行给出的文件与文件夹（如“打开方式”）加入播放列表。
            let mut args = sim_files;
            args.extend(std::env::args_os().skip(1).map(PathBuf::from));
            if !args.is_empty() {
                dispatch(Input::Paths(args));
            }
            message_loop();
            ExitCode::SUCCESS
        }
        None => ExitCode::FAILURE,
    };
    if com {
        // SAFETY: 与上面成功的 CoInitializeEx 配对。
        unsafe { CoUninitialize() };
    }
    code
}

/// WASAPI 独占输出的界面模型（播放线程中按设备标识创建后端）。
fn wasapi_ui() -> qy_ui::Ui {
    qy_ui::Ui::new(
        |device: Option<&str>| -> Result<qy_host_wasapi::wasapi::WasapiBackend, String> {
            let id = match device {
                Some(d) => d.to_string(),
                None => {
                    qy_host_wasapi::endpoints::default_render_endpoint()
                        .map_err(|e| format!("找不到默认输出设备（{e}）"))?
                        .id
                }
            };
            Ok(qy_host_wasapi::wasapi::WasapiBackend::new(id))
        },
        Arc::new(notify),
    )
}

fn message_loop() {
    let mut msg = MSG::default();
    // SAFETY: 标准消息循环；msg 为本地变量。
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 创建主窗口、菜单与控件。
fn create(title: &str) -> Option<Handles> {
    let title = wide(title);
    // SAFETY: 窗口类与窗口创建；所有参数为本地有效值或系统返回的句柄。
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let instance = GetModuleHandleW(None).ok()?.into();
        let class = w!("QyBitMainWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut _),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return None;
        }
        let main = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1000,
            700,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()?;
        MAIN.store(main.0 as isize, Ordering::Release);
        let _ = SetMenu(main, Some(menu()?));
        let child = |class: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, ex: u32, id: u16| {
            CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                text,
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                10,
                10,
                Some(main),
                Some(HMENU(usize::from(id) as *mut _)),
                Some(instance),
                None,
            )
            .ok()
        };
        let button = WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32);
        let h = Handles {
            main,
            play: child(w!("BUTTON"), w!("播放"), button, 0, ID_PLAY)?,
            buttons: [
                child(w!("BUTTON"), w!("停止"), button, 0, ID_STOP)?,
                child(w!("BUTTON"), w!("上一首"), button, 0, ID_PREV)?,
                child(w!("BUTTON"), w!("下一首"), button, 0, ID_NEXT)?,
                child(w!("BUTTON"), w!("添加文件…"), button, 0, ID_ADD_FILES)?,
            ],
            device: child(
                WC_COMBOBOXW,
                w!(""),
                WS_TABSTOP | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
                0,
                IDC_DEVICE,
            )?,
            mono: child(
                w!("BUTTON"),
                w!("单声道复制到左右两路（会改变声道）"),
                WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                0,
                IDC_MONO,
            )?,
            list: child(
                WC_LISTVIEWW,
                w!(""),
                WS_TABSTOP
                    | WS_BORDER
                    | WINDOW_STYLE(LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS),
                0,
                IDC_LIST,
            )?,
            log: child(
                w!("EDIT"),
                w!(""),
                WS_VSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32),
                WS_EX_CLIENTEDGE.0,
                IDC_LOG,
            )?,
            status: child(STATUSCLASSNAMEW, w!(""), WINDOW_STYLE(0), 0, IDC_STATUS)?,
            format: child(
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(SS_LEFT | SS_ENDELLIPSIS | SS_NOPREFIX),
                0,
                IDC_FORMAT,
            )?,
            seek: child(
                TRACKBAR_CLASSW,
                w!(""),
                WS_TABSTOP | WINDOW_STYLE(TBS_HORZ | TBS_NOTICKS),
                0,
                IDC_SEEK,
            )?,
            volume: child(
                TRACKBAR_CLASSW,
                w!(""),
                WS_TABSTOP | WINDOW_STYLE(TBS_HORZ | TBS_NOTICKS),
                0,
                IDC_VOLUME,
            )?,
            volume_label: child(
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(SS_RIGHT | SS_ENDELLIPSIS | SS_NOPREFIX),
                0,
                IDC_VOLUME_LABEL,
            )?,
        };
        SendMessageW(h.volume, TBM_SETRANGEMIN, Some(WPARAM(0)), Some(LPARAM(0)));
        SendMessageW(
            h.volume,
            TBM_SETRANGEMAX,
            Some(WPARAM(1)),
            Some(LPARAM(100)),
        );
        let _ = EnableWindow(h.volume, false);
        SendMessageW(h.seek, TBM_SETRANGEMIN, Some(WPARAM(0)), Some(LPARAM(0)));
        SendMessageW(h.seek, TBM_SETRANGEMAX, Some(WPARAM(1)), Some(LPARAM(1000)));
        let _ = EnableWindow(h.seek, false);
        HANDLES.set(Some(h));
        SendMessageW(
            h.list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            None,
            Some(LPARAM(
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_GRIDLINES) as isize,
            )),
        );
        for (i, (title, _)) in COLUMNS.iter().enumerate() {
            let mut text = wide(title);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH,
                cx: 50,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                h.list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(i)),
                Some(LPARAM(&col as *const _ as isize)),
            );
        }
        apply_dpi(&h);
        DragAcceptFiles(main, true);
        let _ = ShowWindow(main, SW_SHOW);
        Some(h)
    }
}

fn menu() -> Option<HMENU> {
    // SAFETY: 菜单创建与追加；字符串为静态宽字符串。
    unsafe {
        let bar = CreateMenu().ok()?;
        let file = CreatePopupMenu().ok()?;
        let play = CreatePopupMenu().ok()?;
        let help = CreatePopupMenu().ok()?;
        let item = |m: HMENU, id: u16, text: PCWSTR| {
            let _ = AppendMenuW(m, MF_STRING, usize::from(id), text);
        };
        item(file, ID_ADD_FILES, w!("添加文件(&O)…"));
        item(file, ID_ADD_FOLDER, w!("添加文件夹(&D)…"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, None);
        item(file, ID_REMOVE, w!("移除选中(&R)\tDel"));
        item(file, ID_CLEAR, w!("清空列表(&C)"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, None);
        item(file, ID_EXIT, w!("退出(&X)"));
        item(play, ID_PLAY, w!("播放 / 暂停(&P)\tSpace"));
        item(play, ID_STOP, w!("停止(&S)"));
        item(play, ID_PREV, w!("上一首(&V)"));
        item(play, ID_NEXT, w!("下一首(&N)"));
        item(help, ID_ABOUT, w!("关于 QyBit(&A)"));
        let _ = AppendMenuW(bar, MF_POPUP, file.0 as usize, w!("文件(&F)"));
        let _ = AppendMenuW(bar, MF_POPUP, play.0 as usize, w!("播放(&P)"));
        let _ = AppendMenuW(bar, MF_POPUP, help.0 as usize, w!("帮助(&H)"));
        Some(bar)
    }
}

fn fill_devices(h: &Handles, names: &[String]) {
    for n in names {
        let t = wide(n);
        // SAFETY: 下拉框句柄有效；字符串在调用期间存活且以 0 结尾。
        unsafe {
            SendMessageW(
                h.device,
                CB_ADDSTRING,
                None,
                Some(LPARAM(t.as_ptr() as isize)),
            );
        }
    }
    // SAFETY: 同上。
    unsafe {
        SendMessageW(h.device, CB_SETCURSEL, Some(WPARAM(0)), None);
    }
}

/// 按窗口 DPI 重建字体、设置列宽并重新布局。
fn apply_dpi(h: &Handles) {
    // SAFETY: 句柄有效；NONCLIENTMETRICSW 为本地变量；旧字体在所有控件换用新字体后删除。
    unsafe {
        let dpi = GetDpiForWindow(h.main).max(96);
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let font = if SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut _),
            0,
            dpi,
        )
        .is_ok()
        {
            CreateFontIndirectW(&ncm.lfMessageFont)
        } else {
            HFONT(std::ptr::null_mut())
        };
        let all = [
            h.play,
            h.buttons[0],
            h.buttons[1],
            h.buttons[2],
            h.buttons[3],
            h.device,
            h.mono,
            h.list,
            h.log,
            h.status,
            h.format,
        ];
        for c in all {
            SendMessageW(
                c,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
        let old = FONT.replace(font);
        if !old.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(old.0));
        }
        for (i, (_, width)) in COLUMNS.iter().enumerate() {
            SendMessageW(
                h.list,
                LVM_SETCOLUMNWIDTH,
                Some(WPARAM(i)),
                Some(LPARAM((*width * dpi as i32 / 96) as isize)),
            );
        }
    }
    layout(h);
}

fn layout(h: &Handles) {
    // SAFETY: 句柄有效；rc 为本地变量。
    unsafe {
        let dpi = GetDpiForWindow(h.main).max(96) as i32;
        let s = |v: i32| v * dpi / 96;
        let mut rc = RECT::default();
        let _ = GetClientRect(h.main, &mut rc);
        SendMessageW(h.status, WM_SIZE, None, None);
        let mut src = RECT::default();
        let _ = GetClientRect(h.status, &mut src);
        let status_h = src.bottom - src.top;
        set_parts(h, TWO_PARTS.get());
        let (bar_y, bar_h) = (s(6), s(28));
        let mut x = s(6);
        for (b, w) in [
            (h.play, 72),
            (h.buttons[0], 72),
            (h.buttons[1], 72),
            (h.buttons[2], 72),
            (h.buttons[3], 96),
        ] {
            let _ = MoveWindow(b, x, bar_y, s(w), bar_h, true);
            x += s(w) + s(6);
        }
        let _ = MoveWindow(h.device, x + s(6), bar_y + s(2), s(260), s(300), true);
        x += s(272);
        let _ = MoveWindow(
            h.mono,
            x + s(6),
            bar_y,
            (rc.right - x - s(12)).max(0),
            bar_h,
            true,
        );
        let (fmt_y, fmt_h) = (bar_y + bar_h + s(4), s(24));
        // 格式行右侧：设备硬件音量的读数标签与滑块（GUI T5）。
        let (vol_w, label_w) = (s(140), s(300));
        let vol_x = (rc.right - s(6) - vol_w).max(0);
        let label_x = (vol_x - s(4) - label_w).max(0);
        let _ = MoveWindow(h.volume, vol_x, fmt_y, vol_w, fmt_h, true);
        let _ = MoveWindow(
            h.volume_label,
            label_x,
            fmt_y + s(4),
            label_w,
            fmt_h - s(4),
            true,
        );
        let _ = MoveWindow(
            h.format,
            s(8),
            fmt_y + s(4),
            (label_x - s(16)).max(0),
            fmt_h - s(4),
            true,
        );
        let (seek_y, seek_h) = (fmt_y + fmt_h, s(26));
        let _ = MoveWindow(h.seek, s(2), seek_y, (rc.right - s(4)).max(0), seek_h, true);
        let top = seek_y + seek_h + s(2);
        let free = (rc.bottom - top - status_h).max(0);
        let log_h = (free / 4).max(s(60)).min(free);
        let _ = MoveWindow(h.list, 0, top, rc.right, free - log_h, true);
        let _ = MoveWindow(h.log, 0, top + free - log_h, rc.right, log_h, true);
    }
}

/// 状态栏分栏：两栏时各占一半，一栏时占整行。
fn set_parts(h: &Handles, two: bool) {
    // SAFETY: 句柄有效；rc 与 parts 为本地变量，SB_SETPARTS 调用期间有效。
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(h.status, &mut rc);
        let parts: &[i32] = if two { &[rc.right / 2, -1] } else { &[-1] };
        SendMessageW(
            h.status,
            SB_SETPARTS,
            Some(WPARAM(parts.len())),
            Some(LPARAM(parts.as_ptr() as isize)),
        );
    }
}

/// 播放列表当前选中的行（升序）。
fn selected_rows(list: HWND) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i: isize = -1;
    loop {
        // SAFETY: 列表句柄有效；LVM_GETNEXTITEM 只读查询。
        let r = unsafe {
            SendMessageW(
                list,
                LVM_GETNEXTITEM,
                Some(WPARAM(i as usize)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
        };
        if r.0 < 0 || r.0 as isize == i {
            break;
        }
        i = r.0 as isize;
        out.push(i as usize);
    }
    out
}

/// 把输入交给界面逻辑，再在不持有状态借用时执行返回的显示操作。
fn dispatch(input: Input) {
    let Some(h) = HANDLES.get() else {
        return;
    };
    let selected = match input {
        Input::Command(_) => selected_rows(h.list),
        _ => Vec::new(),
    };
    let ops = APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => g
            .as_mut()
            .map(|app| app.handle(input, &selected))
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    });
    for op in ops {
        exec(op);
    }
}

fn exec(op: Op) {
    let Some(h) = HANDLES.get() else {
        return;
    };
    match op {
        // SAFETY（本函数各分支）：句柄有效；传入的字符串在调用期间存活且以 0 结尾。
        Op::Count(n) => unsafe {
            SendMessageW(
                h.list,
                LVM_SETITEMCOUNT,
                Some(WPARAM(n)),
                Some(LPARAM(LVSICF_NOSCROLL as isize)),
            );
        },
        Op::Redraw => unsafe {
            let _ = InvalidateRect(Some(h.list), None, false);
        },
        Op::Status {
            status,
            fidelity,
            format,
        } => {
            let two = !fidelity.is_empty();
            if TWO_PARTS.replace(two) != two {
                set_parts(&h, two);
            }
            let t = wide(&format);
            unsafe {
                let _ = SetWindowTextW(h.format, PCWSTR(t.as_ptr()));
            }
            for (part, text) in [(0usize, status), (1, fidelity)] {
                let t = wide(&text);
                unsafe {
                    SendMessageW(
                        h.status,
                        SB_SETTEXTW,
                        Some(WPARAM(part)),
                        Some(LPARAM(t.as_ptr() as isize)),
                    );
                }
            }
        }
        Op::Seekbar(pos) => unsafe {
            let _ = EnableWindow(h.seek, pos.is_some());
            if !DRAGGING.get() {
                SendMessageW(
                    h.seek,
                    TBM_SETPOS,
                    Some(WPARAM(1)),
                    Some(LPARAM(pos.unwrap_or(0) as isize)),
                );
            }
        },
        Op::Log(text) => {
            let t = wide(&text);
            unsafe {
                let _ = SetWindowTextW(h.log, PCWSTR(t.as_ptr()));
                let end = t.len().saturating_sub(1);
                SendMessageW(
                    h.log,
                    EM_SETSEL,
                    Some(WPARAM(end)),
                    Some(LPARAM(end as isize)),
                );
                SendMessageW(h.log, EM_SCROLLCARET, None, None);
            }
        }
        Op::PlayLabel(label) => {
            let t = wide(label);
            unsafe {
                let _ = SetWindowTextW(h.play, PCWSTR(t.as_ptr()));
            }
        }
        Op::OpenFiles => {
            let paths = pick(h.main, false);
            if !paths.is_empty() {
                dispatch(Input::Paths(paths));
            }
        }
        Op::OpenFolder => {
            let paths = pick(h.main, true);
            if !paths.is_empty() {
                dispatch(Input::Paths(paths));
            }
        }
        Op::About => {
            let t = wide(&about_text());
            unsafe {
                MessageBoxW(
                    Some(h.main),
                    PCWSTR(t.as_ptr()),
                    w!("关于 QyBit"),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
        }
        Op::Close => unsafe {
            let _ = PostMessageW(Some(h.main), WM_CLOSE, WPARAM(0), LPARAM(0));
        },
    }
}

/// 系统文件对话框（多选）；取消或出错时为空。
fn pick(owner: HWND, folders: bool) -> Vec<PathBuf> {
    // SAFETY: 本线程已初始化 COM（单线程套间）；接口由 windows crate 管理引用计数；
    // GetDisplayName 返回的字符串用 CoTaskMemFree 释放。
    unsafe {
        let Ok(dlg) =
            CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
        else {
            return Vec::new();
        };
        let mut opts = dlg.GetOptions().unwrap_or_default();
        opts |= FOS_ALLOWMULTISELECT | FOS_FORCEFILESYSTEM;
        opts |= if folders {
            FOS_PICKFOLDERS
        } else {
            FOS_FILEMUSTEXIST
        };
        let _ = dlg.SetOptions(opts);
        if !folders {
            let specs = [
                COMDLG_FILTERSPEC {
                    pszName: w!("音频文件（WAV、FLAC、AIFF）"),
                    pszSpec: w!("*.wav;*.wave;*.flac;*.aif;*.aiff;*.aifc"),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("所有文件"),
                    pszSpec: w!("*.*"),
                },
            ];
            let _ = dlg.SetFileTypes(&specs);
        }
        if dlg.Show(Some(owner)).is_err() {
            return Vec::new();
        }
        let Ok(items) = dlg.GetResults() else {
            return Vec::new();
        };
        let n = items.GetCount().unwrap_or(0);
        (0..n)
            .filter_map(|i| {
                let item = items.GetItemAt(i).ok()?;
                let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
                let s = p.to_string().ok();
                CoTaskMemFree(Some(p.0 as *const _));
                s.map(PathBuf::from)
            })
            .collect()
    }
}

/// 拖放到窗口的文件与文件夹。
fn dropped(drop: HDROP) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // SAFETY: `drop` 来自 WM_DROPFILES；按查询到的长度分配缓冲；处理后 DragFinish 释放。
    unsafe {
        let n = DragQueryFileW(drop, u32::MAX, None);
        for i in 0..n {
            let len = DragQueryFileW(drop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
            out.push(PathBuf::from(String::from_utf16_lossy(
                &buf[..got.min(len)],
            )));
        }
        DragFinish(drop);
    }
    out
}

/// 列表单元格文字（虚拟列表按需索取）。界面状态正被借用时给出空文字（不阻塞）。
fn fill_cell(info: &mut NMLVDISPINFOW) {
    if !info.item.mask.contains(LVIF_TEXT)
        || info.item.cchTextMax <= 0
        || info.item.pszText.is_null()
    {
        return;
    }
    let (row, col) = (info.item.iItem, info.item.iSubItem);
    let text = if row >= 0 && col >= 0 {
        APP.with(|a| {
            a.try_borrow()
                .ok()
                .and_then(|g| g.as_ref().map(|app| app.cell(row as usize, col as usize)))
        })
        .unwrap_or_default()
    } else {
        String::new()
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    let n = units.len().min(info.item.cchTextMax as usize - 1);
    // SAFETY: pszText 指向控件提供的 cchTextMax 个 u16 的缓冲；写入 n + 1 ≤ cchTextMax 个。
    unsafe {
        let dst = std::slice::from_raw_parts_mut(info.item.pszText.0, n + 1);
        dst[..n].copy_from_slice(&units[..n]);
        dst[n] = 0;
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            if let Some(h) = HANDLES.get() {
                layout(&h);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // SAFETY: WM_DPICHANGED 的 lparam 指向系统给出的建议窗口矩形。
            unsafe {
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if let Some(h) = HANDLES.get() {
                apply_dpi(&h);
            }
            LRESULT(0)
        }
        // 窗口重新激活时重新打开音量控制：用户可能在系统中换了默认设备或改了音量。
        WM_ACTIVATE => {
            if loword(wparam.0) != 0 {
                open_volume();
            }
            LRESULT(0)
        }
        WM_QY_NOTIFY => {
            PENDING.store(false, Ordering::Release);
            dispatch(Input::Notify);
            show_volume();
            LRESULT(0)
        }
        // 进度条（S1）：拖动或按住滑轨时只在状态栏预览时间，松开（TB_ENDTRACK，键盘操作也以此
        // 结束）时按滑块位置定位。点击滑轨时滑块直接移到点击处（系统默认每次只移动一页）。
        WM_HSCROLL => {
            // 设备硬件音量（GUI T5）：拖动中即设置，松开后读回。
            if let Some(h) = HANDLES.get()
                && lparam.0 == h.volume.0 as isize
            {
                let code = u32::from(loword(wparam.0));
                if code == TB_ENDTRACK {
                    VOL_DRAGGING.set(false);
                    show_volume();
                } else {
                    VOL_DRAGGING.set(code == TB_THUMBTRACK);
                    // SAFETY: 只读查询鼠标左键状态（区分点击滑轨与 PageUp/PageDown 键）；音量滑块
                    // 句柄有效。
                    unsafe {
                        if matches!(code, TB_PAGEUP | TB_PAGEDOWN)
                            && GetKeyState(i32::from(VK_LBUTTON.0)) < 0
                            && let Some(pos) = click_position(h.volume, 100)
                        {
                            SendMessageW(
                                h.volume,
                                TBM_SETPOS,
                                Some(WPARAM(1)),
                                Some(LPARAM(pos as isize)),
                            );
                        }
                    }
                    // SAFETY: 音量滑块句柄有效；只读查询。
                    let pos = unsafe { SendMessageW(h.volume, TBM_GETPOS, None, None) };
                    set_volume(u32::try_from(pos.0).unwrap_or(0));
                }
                return LRESULT(0);
            }
            if let Some(h) = HANDLES.get()
                && lparam.0 == h.seek.0 as isize
            {
                match u32::from(loword(wparam.0)) {
                    TB_THUMBTRACK => {
                        DRAGGING.set(true);
                        seek_preview(h, u32::from(hiword(wparam.0)));
                    }
                    TB_PAGEUP | TB_PAGEDOWN => {
                        DRAGGING.set(true);
                        // SAFETY: 只读查询鼠标左键状态；高位为 1 表示按下（区分点击与 PageUp/PageDown 键）。
                        if unsafe { GetKeyState(i32::from(VK_LBUTTON.0)) } < 0
                            && let Some(pos) = click_position(h.seek, 1000)
                        {
                            // SAFETY: 进度条句柄有效。
                            unsafe {
                                SendMessageW(
                                    h.seek,
                                    TBM_SETPOS,
                                    Some(WPARAM(1)),
                                    Some(LPARAM(pos as isize)),
                                );
                            }
                        }
                        // SAFETY: 进度条句柄有效；只读查询。
                        let pos = unsafe { SendMessageW(h.seek, TBM_GETPOS, None, None) };
                        seek_preview(h, u32::try_from(pos.0).unwrap_or(0));
                    }
                    TB_ENDTRACK => {
                        DRAGGING.set(false);
                        // SAFETY: 进度条句柄有效；只读查询。
                        let pos = unsafe { SendMessageW(h.seek, TBM_GETPOS, None, None) };
                        dispatch(Input::Seek(u32::try_from(pos.0).unwrap_or(0)));
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let (id, code) = (loword(wparam.0), hiword(wparam.0));
            match id {
                IDC_DEVICE if u32::from(code) == CBN_SELCHANGE => {
                    if let Some(h) = HANDLES.get() {
                        // SAFETY: 下拉框句柄有效；只读查询。
                        let i = unsafe { SendMessageW(h.device, CB_GETCURSEL, None, None) };
                        if i.0 >= 0 {
                            dispatch(Input::Device(i.0 as usize));
                            open_volume();
                        }
                    }
                }
                IDC_MONO if code == 0 => {
                    if let Some(h) = HANDLES.get() {
                        // SAFETY: 复选框句柄有效；只读查询。
                        let r = unsafe { SendMessageW(h.mono, BM_GETCHECK, None, None) };
                        dispatch(Input::Mono(r.0 as u32 == BST_CHECKED.0));
                    }
                }
                ID_PLAY..=ID_ABOUT if code <= 1 => dispatch(Input::Command(id)),
                _ => {}
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            // SAFETY: WM_NOTIFY 的 lparam 指向系统提供的 NMHDR；按 code 转为对应的通知结构
            // （LVN_GETDISPINFOW → NMLVDISPINFOW，NM_DBLCLK → NMITEMACTIVATE，
            // LVN_KEYDOWN → NMLVKEYDOWN），只在列表控件的通知上读取。
            unsafe {
                let hdr = &*(lparam.0 as *const NMHDR);
                if hdr.idFrom == usize::from(IDC_LIST) {
                    match hdr.code {
                        LVN_GETDISPINFOW => fill_cell(&mut *(lparam.0 as *mut NMLVDISPINFOW)),
                        NM_DBLCLK => {
                            let a = &*(lparam.0 as *const NMITEMACTIVATE);
                            if a.iItem >= 0 {
                                dispatch(Input::Activate(a.iItem as usize));
                            }
                        }
                        LVN_KEYDOWN => {
                            // NMLVKEYDOWN 按 1 字节对齐（packed）：按值读出，不取字段引用。
                            let k = std::ptr::read_unaligned(lparam.0 as *const NMLVKEYDOWN);
                            let key = k.wVKey;
                            let rows = selected_rows(hdr.hwndFrom);
                            match key {
                                v if v == VK_DELETE.0 => dispatch(Input::Delete(rows)),
                                v if v == VK_RETURN.0 => {
                                    if let Some(&r) = rows.first() {
                                        dispatch(Input::Activate(r));
                                    }
                                }
                                v if v == VK_SPACE.0 => dispatch(Input::Command(ID_PLAY)),
                                v if v == VK_LEFT.0 => dispatch(Input::SeekBy(-5)),
                                v if v == VK_RIGHT.0 => dispatch(Input::SeekBy(5)),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            LRESULT(0)
        }
        WM_DROPFILES => {
            let paths = dropped(HDROP(wparam.0 as *mut _));
            dispatch(Input::Paths(paths));
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: 销毁本窗口；之后收到 WM_DESTROY。
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            MAIN.store(0, Ordering::Release);
            // 停止播放并等待后台线程结束（Ui 析构）。
            let app = APP.with_borrow_mut(Option::take);
            drop(app);
            let font = FONT.replace(HFONT(std::ptr::null_mut()));
            // SAFETY: 删除本前端创建的字体（控件已随窗口销毁）；结束消息循环。
            unsafe {
                if !font.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(font.0));
                }
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: 其余消息交给默认处理。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
