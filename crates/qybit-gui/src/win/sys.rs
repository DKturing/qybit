//! Win32 边界：窗口、控件、消息循环、文件对话框与拖放。本前端的全部 `unsafe` 集中在此，
//! 每处注明前提。界面逻辑在 [`super::app`]（安全代码）：窗口过程把消息转成 [`Input`]，在
//! 不持有状态借用的情况下执行 [`App`] 返回的 [`Op`]，因此控件同步回发的消息不会重入状态。
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, GetSysColorBrush, HBRUSH, HDC, HFONT,
    HGDIOBJ, InvalidateRect, ScreenToClient, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BST_CHECKED, EM_SCROLLCARET, EM_SETSEL, HDF_SORTDOWN, HDF_SORTUP, HDI_FORMAT, HDITEMW,
    HDM_GETITEMW, HDM_SETITEMW, HEADER_CONTROL_FORMAT_FLAGS, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES,
    ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, LIST_VIEW_ITEM_STATE_FLAGS,
    LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVHITTESTINFO, LVIF_TEXT, LVIR_BOUNDS, LVIS_DROPHILITED,
    LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_ENSUREVISIBLE, LVM_GETHEADER, LVM_GETITEMCOUNT,
    LVM_GETITEMRECT, LVM_GETNEXTITEM, LVM_GETTOPINDEX, LVM_HITTEST, LVM_INSERTCOLUMNW, LVM_SCROLL,
    LVM_SETCOLUMNWIDTH, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMCOUNT, LVM_SETITEMSTATE,
    LVN_BEGINDRAG, LVN_COLUMNCLICK, LVN_GETDISPINFOW, LVN_KEYDOWN, LVNI_SELECTED,
    LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, LVS_OWNERDATA, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVSICF_NOSCROLL, NM_DBLCLK, NMHDR, NMITEMACTIVATE, NMLISTVIEW,
    NMLVDISPINFOW, NMLVKEYDOWN, SB_SETPARTS, SB_SETTEXTW, STATUSCLASSNAMEW, TB_ENDTRACK,
    TB_PAGEDOWN, TB_PAGEUP, TB_THUMBTRACK, TBM_GETCHANNELRECT, TBM_GETTHUMBRECT, TBM_SETPOS,
    TBM_SETRANGEMAX, TBM_SETRANGEMIN, TBS_HORZ, TBS_NOTICKS, TRACKBAR_CLASSW, WC_COMBOBOXW,
    WC_LISTVIEWW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
    SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, MOD_NOREPEAT, RegisterHotKey, ReleaseCapture, SetCapture,
    UnregisterHotKey, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_LBUTTON, VK_LEFT,
    VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP, VK_RETURN,
    VK_RIGHT, VK_SPACE, VK_UP,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, HDROP, IFileOpenDialog,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BM_GETCHECK, BS_AUTOCHECKBOX, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL,
    CB_RESETCONTENT, CB_SETCURSEL, CBN_DROPDOWN, CBN_SELCHANGE, CBS_DROPDOWNLIST, CW_USEDEFAULT,
    CheckMenuItem, CreateMenu, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, GetClientRect, GetCursorPos,
    GetMenu, GetMessageW, GetWindowPlacement, GetWindowRect, HMENU, IDC_ARROW, IDC_SIZENS,
    IsIconic, KillTimer, LoadCursorW, MB_ICONINFORMATION, MB_OK, MF_BYCOMMAND, MF_CHECKED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, MessageBoxW, MoveWindow,
    NONCLIENTMETRICSW, PostMessageW, PostQuitMessage, RegisterClassW, SPI_GETNONCLIENTMETRICS,
    SW_HIDE, SW_SHOW, SW_SHOWMAXIMIZED, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW,
    SetCursor, SetMenu, SetTimer, SetWindowPlacement, SetWindowPos, SetWindowTextW, ShowWindow,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WINDOWPLACEMENT, WM_ACTIVATE, WM_APP,
    WM_APPCOMMAND, WM_CAPTURECHANGED, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY,
    WM_DPICHANGED, WM_DROPFILES, WM_ENDSESSION, WM_HOTKEY, WM_HSCROLL, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NOTIFY, WM_SETCURSOR, WM_SETFONT, WM_SIZE, WM_TIMER, WNDCLASSW,
    WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{PCWSTR, PWSTR, w};

use super::app::{
    App, COLUMNS, ID_ABOUT, ID_ADD_FILES, ID_ADD_FOLDER, ID_CLEAR, ID_CLEAR_LOG, ID_EXIT, ID_LAST,
    ID_MEDIA_KEYS, ID_MOVE_DOWN, ID_MOVE_UP, ID_NEXT, ID_PLAY, ID_PREV, ID_REMEMBER, ID_REMOVE,
    ID_SORT_ADDED, ID_SORT_FORMAT, ID_SORT_LENGTH, ID_SORT_NAME, ID_STOP, ID_TOGGLE_LOG,
    IDC_DEVICE, IDC_FLOAT, IDC_FORMAT, IDC_LIST, IDC_LOG, IDC_MONO, IDC_RATE, IDC_RATE_LAMP,
    IDC_SEEK, IDC_STATUS, IDC_VOLUME, IDC_VOLUME_LABEL, Input, Media, Op, VolumeView, about_text,
    column_key,
};
use super::sim::Sim;
use qy_host_wasapi::endpoints::EndpointInfo;
use qy_host_wasapi::volume::EndpointVolume;
use qy_host_wasapi::watch::EndpointWatcher;
use qy_ui::settings::{self, Settings, Window};
use qy_ui::text::RateLamp;

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
/// 端点变化通知（系统线程投递，GUI T6）：重新枚举输出设备。
const WM_QY_DEVICES: u32 = WM_APP + 2;
/// 设定列表选中行（排序、移动之后投递，在控件处理完当前按键之后执行）。
const WM_QY_SELECT: u32 = WM_APP + 3;
/// 窗口在另一个虚拟桌面时，定时检查是否回到可见（GUI T6）。
const TIMER_VISIBLE: usize = 1;

/// `WM_APPCOMMAND` 的命令（winuser.h：`APPCOMMAND_MEDIA_NEXTTRACK` 11、`_PREVIOUSTRACK` 12、
/// `_STOP` 13、`_PLAY_PAUSE` 14、`_PLAY` 46、`_PAUSE` 47；`GET_APPCOMMAND_LPARAM` 取 lparam 高位
/// 字去掉设备位 `FAPPCOMMAND_MASK` 0xF000）。
fn app_command(lparam: LPARAM) -> Option<Media> {
    match u32::from(hiword(lparam.0 as usize)) & 0x0FFF {
        11 => Some(Media::Next),
        12 => Some(Media::Prev),
        13 => Some(Media::Stop),
        14 => Some(Media::PlayPause),
        46 => Some(Media::Play),
        47 => Some(Media::Pause),
        _ => None,
    }
}

/// 全局媒体键（`RegisterHotKey` 的编号、虚拟键与动作）。
const HOTKEYS: [(i32, u16, Media); 4] = [
    (1, VK_MEDIA_PLAY_PAUSE.0, Media::PlayPause),
    (2, VK_MEDIA_STOP.0, Media::Stop),
    (3, VK_MEDIA_NEXT_TRACK.0, Media::Next),
    (4, VK_MEDIA_PREV_TRACK.0, Media::Prev),
];

/// 指示灯颜色（COLORREF 为 0x00BBGGRR）。
fn lamp_color(l: RateLamp) -> COLORREF {
    COLORREF(match l {
        RateLamp::Off => 0x0080_8080,
        RateLamp::Opening => 0x0000_A0E6,
        RateLamp::Native => 0x003C_A000,
        RateLamp::Rejected | RateLamp::Mismatch => 0x001E_1EC8,
    })
}

/// 主窗口与控件句柄（创建后不变）。
#[derive(Clone, Copy)]
struct Handles {
    main: HWND,
    play: HWND,
    buttons: [HWND; 4],
    device: HWND,
    mono: HWND,
    float: HWND,
    list: HWND,
    log: HWND,
    status: HWND,
    format: HWND,
    seek: HWND,
    volume: HWND,
    volume_label: HWND,
    /// 采样率指示灯（“●”，颜色表示状态）与文字（GUI T6）。
    rate_lamp: HWND,
    rate: HWND,
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
    /// 日志区是否显示。
    static LOG_VISIBLE: Cell<bool> = const { Cell::new(true) };
    /// 用户拖动分隔条设定的日志区高度（96 DPI 下的像素）；0 表示按默认比例（可用高度的 1/4）。
    static LOG_HEIGHT: Cell<i32> = const { Cell::new(0) };
    /// 播放列表与日志区之间分隔条的纵向范围（客户区坐标，上界含、下界不含）；隐藏日志时为空。
    static SPLIT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// 正在拖动分隔条。
    static SPLITTING: Cell<bool> = const { Cell::new(false) };
    /// 采样率指示灯的状态与上一次的文字（GUI T6）。
    static LAMP: Cell<RateLamp> = const { Cell::new(RateLamp::Off) };
    static RATE_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
    /// 正在拖动的行（GUI T6）；`None` 为没有在拖动。
    static DRAG_ROWS: RefCell<Option<Vec<usize>>> = const { RefCell::new(None) };
    /// 拖动中高亮的落点行。
    static DRAG_MARK: Cell<Option<usize>> = const { Cell::new(None) };
    /// 待设定的选中行（`WM_QY_SELECT` 时执行）。
    static PENDING_SELECT: RefCell<Option<Vec<usize>>> = const { RefCell::new(None) };
    /// 端点变化通知已注册；未注册时打开设备下拉框时重新枚举。
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    /// 已注册的全局媒体键。
    static HOTKEYS_ON: Cell<[bool; 4]> = const { Cell::new([false; 4]) };
    /// 可见性定时器已启动。
    static VIS_TIMER: Cell<bool> = const { Cell::new(false) };
    /// 模拟设备模式下的端点（不枚举真实设备）。
    static SIM_ENDPOINT: RefCell<Option<EndpointInfo>> = const { RefCell::new(None) };
}

/// 端点变化通知已投递未处理（合并连续通知）。
static DEVICES_PENDING: AtomicBool = AtomicBool::new(false);

/// 跨线程通知状态：主窗口句柄（整数形式）与“已投递未处理”标志（合并连续通知）。
static MAIN: AtomicIsize = AtomicIsize::new(0);
static PENDING: AtomicBool = AtomicBool::new(false);

/// 打开当前选定设备的音量控制并显示（GUI T5）。在界面线程调用（COM 单线程套间已初始化）。
fn open_volume() {
    let opened = if SIM_MODE.get() {
        Err("模拟设备，没有音量控制".to_string())
    } else {
        let id = APP.with(|a| a.try_borrow().ok().and_then(|g| g.as_ref()?.device_id()));
        EndpointVolume::open(id.as_deref()).map_err(|e| match id {
            Some(_) => format!("找不到所选设备（{}）", e.message()),
            None => format!("找不到默认输出设备（{}）", e.message()),
        })
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

/// 输出设备：系统默认端点与全部活动端点。端点枚举使用多线程套间，放在单独的线程中完成；模拟
/// 设备模式下只有模拟端点。
fn enumerate() -> (Option<EndpointInfo>, Vec<EndpointInfo>) {
    if let Some(e) = SIM_ENDPOINT.with_borrow(Clone::clone) {
        return (Some(e), Vec::new());
    }
    std::thread::spawn(|| {
        (
            qy_host_wasapi::endpoints::default_render_endpoint().ok(),
            qy_host_wasapi::endpoints::list_render_endpoints().unwrap_or_default(),
        )
    })
    .join()
    .unwrap_or_default()
}

/// 端点变化后重新枚举并更新下拉框与音量显示（GUI T6）。
fn refresh_devices() {
    let (default, endpoints) = enumerate();
    dispatch(Input::Devices { default, endpoints });
    open_volume();
}

/// 系统线程上的端点变化回调：只投递一条消息（已有未处理的不再投递）。
fn devices_changed() {
    if DEVICES_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let raw = MAIN.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: 同 [`notify`]：PostMessageW 可从任意线程调用，窗口已销毁时只返回错误。
        unsafe {
            let _ = PostMessageW(
                Some(HWND(raw as *mut _)),
                WM_QY_DEVICES,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// 窗口在还原状态下的位置与是否最大化。
fn placement(main: HWND) -> Option<Window> {
    let mut wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: 主窗口句柄有效；wp 为本地变量且 length 已设置。
    unsafe { GetWindowPlacement(main, &mut wp) }.ok()?;
    let r = wp.rcNormalPosition;
    Some(Window {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
        maximized: wp.showCmd == SW_SHOWMAXIMIZED.0 as u32,
    })
}

/// 保存设置（GUI T6）：界面设置加上窗口位置与日志区高度。未开启“记住”时不保存。返回保存的
/// 位置与跳过的路径数。
fn save_settings(h: &Handles) -> Option<Result<(PathBuf, usize), String>> {
    let mut s: Settings = APP.with(|a| {
        let g = a.try_borrow().ok()?;
        let app = g.as_ref()?;
        app.remember().then(|| app.settings())
    })?;
    s.log_height = u32::try_from(LOG_HEIGHT.get()).unwrap_or(0);
    s.window = placement(h.main);
    let Some(path) = settings::default_path() else {
        return Some(Err("找不到用户配置目录（APPDATA）".into()));
    };
    Some(
        settings::save(&path, &s)
            .map(|skipped| (path.clone(), skipped))
            .map_err(|e| format!("{}：{e}", path.display())),
    )
}

pub fn run() -> ExitCode {
    // SAFETY: 进程启动时设置 DPI 感知与 COM（单线程套间，文件对话框要求）；与本线程退出前的
    // CoUninitialize 配对。
    let com = unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok()
    };
    let sim = Sim::from_env();
    if let Some(s) = &sim {
        SIM_ENDPOINT.with_borrow_mut(|e| *e = Some(s.endpoint()));
    }
    let (default, endpoints) = enumerate();
    // 上次保存的设置（GUI T6）：文件不存在即未开启“记住”。
    let mut notes = Vec::new();
    let saved = match settings::default_path().map(|p| (settings::load(&p), p)) {
        Some((Ok(p), _)) => p,
        Some((Err(e), path)) => {
            notes.push(format!(
                "无法读取上次保存的设置（{}：{e}），按默认设置启动",
                path.display()
            ));
            None
        }
        None => None,
    };
    if let Some(p) = &saved {
        LOG_HEIGHT.set(i32::try_from(p.settings.log_height).unwrap_or(0));
    }
    let title = sim.as_ref().map_or_else(|| "QyBit".to_string(), Sim::title);
    let code = match create(&title, saved.as_ref().and_then(|p| p.settings.window)) {
        Some(_) => {
            let (ui, sim_files) = match &sim {
                Some(s) => (s.ui(Arc::new(notify)), s.files()),
                None => (wasapi_ui(), Vec::new()),
            };
            let mut app = App::new(ui, default, endpoints);
            if let Some(p) = &saved {
                app.restore(&p.settings, p.ignored);
            }
            let ops = app.initial();
            APP.with_borrow_mut(|a| *a = Some(app));
            for op in ops {
                exec(op);
            }
            SIM_MODE.set(sim.is_some());
            open_volume();
            // 端点变化通知（GUI T6）：设备插拔、启用停用与默认设备变化时更新下拉框。
            let watcher = if sim.is_some() {
                None
            } else {
                match EndpointWatcher::new(devices_changed) {
                    Ok(w) => {
                        WATCHING.set(true);
                        Some(w)
                    }
                    Err(e) => {
                        notes.push(format!(
                            "设备变化通知不可用（{}）：打开设备下拉框时重新读取设备列表",
                            e.message()
                        ));
                        None
                    }
                }
            };
            for n in notes {
                dispatch(Input::Note(n));
            }
            // 命令行给出的文件与文件夹（如“打开方式”）加入播放列表。
            let mut args = sim_files;
            args.extend(std::env::args_os().skip(1).map(PathBuf::from));
            if !args.is_empty() {
                dispatch(Input::Paths(args));
            }
            message_loop();
            // 在 CoUninitialize 之前注销端点变化通知。
            drop(watcher);
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

/// 创建主窗口、菜单与控件；`window` 为上次保存的窗口位置（GUI T6）。
fn create(title: &str, window: Option<Window>) -> Option<Handles> {
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
            float: child(
                w!("BUTTON"),
                w!("浮点转整数（会改变样本）"),
                WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                0,
                IDC_FLOAT,
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
            rate_lamp: child(
                w!("STATIC"),
                w!("●"),
                WINDOW_STYLE(SS_LEFT | SS_NOPREFIX),
                0,
                IDC_RATE_LAMP,
            )?,
            rate: child(
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(SS_LEFT | SS_ENDELLIPSIS | SS_NOPREFIX),
                0,
                IDC_RATE,
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
        // 上次保存的位置：系统在该位置完全不在任何显示器上时会自动移到可见处
        // （Microsoft Learn《SetWindowPlacement》）。
        let placed = window.is_some_and(|w| {
            let wp = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                showCmd: if w.maximized {
                    SW_SHOWMAXIMIZED.0 as u32
                } else {
                    SW_SHOWNORMAL.0 as u32
                },
                rcNormalPosition: RECT {
                    left: w.left,
                    top: w.top,
                    right: w.right,
                    bottom: w.bottom,
                },
                ..Default::default()
            };
            SetWindowPlacement(main, &wp).is_ok()
        });
        if !placed {
            let _ = ShowWindow(main, SW_SHOW);
        }
        Some(h)
    }
}

fn menu() -> Option<HMENU> {
    // SAFETY: 菜单创建与追加；字符串为静态宽字符串。
    unsafe {
        let bar = CreateMenu().ok()?;
        let file = CreatePopupMenu().ok()?;
        let play = CreatePopupMenu().ok()?;
        let list = CreatePopupMenu().ok()?;
        let view = CreatePopupMenu().ok()?;
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
        item(file, ID_REMEMBER, w!("记住播放列表与设置(&M)"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, None);
        item(file, ID_EXIT, w!("退出(&X)"));
        item(play, ID_PLAY, w!("播放 / 暂停(&P)\tSpace"));
        item(play, ID_STOP, w!("停止(&S)"));
        item(play, ID_PREV, w!("上一首(&V)"));
        item(play, ID_NEXT, w!("下一首(&N)"));
        let _ = AppendMenuW(play, MF_SEPARATOR, 0, None);
        item(
            play,
            ID_MEDIA_KEYS,
            w!("全局媒体键（窗口不在前台时也响应）(&K)"),
        );
        item(list, ID_SORT_ADDED, w!("按添加顺序排序(&A)"));
        item(list, ID_SORT_NAME, w!("按文件名排序(&N)"));
        item(list, ID_SORT_LENGTH, w!("按时长排序(&T)"));
        item(list, ID_SORT_FORMAT, w!("按格式排序(&F)"));
        let _ = AppendMenuW(list, MF_SEPARATOR, 0, None);
        item(list, ID_MOVE_UP, w!("上移选中(&U)\tCtrl+↑"));
        item(list, ID_MOVE_DOWN, w!("下移选中(&D)\tCtrl+↓"));
        let _ = AppendMenuW(
            view,
            MF_STRING | MF_CHECKED,
            usize::from(ID_TOGGLE_LOG),
            w!("显示日志(&L)"),
        );
        item(view, ID_CLEAR_LOG, w!("清除日志(&E)"));
        item(help, ID_ABOUT, w!("关于 QyBit(&A)"));
        let _ = AppendMenuW(bar, MF_POPUP, file.0 as usize, w!("文件(&F)"));
        let _ = AppendMenuW(bar, MF_POPUP, play.0 as usize, w!("播放(&P)"));
        let _ = AppendMenuW(bar, MF_POPUP, list.0 as usize, w!("列表(&L)"));
        let _ = AppendMenuW(bar, MF_POPUP, view.0 as usize, w!("查看(&V)"));
        let _ = AppendMenuW(bar, MF_POPUP, help.0 as usize, w!("帮助(&H)"));
        Some(bar)
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
            h.float,
            h.list,
            h.log,
            h.status,
            h.format,
            h.volume_label,
            h.rate_lamp,
            h.rate,
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
        // 设备下拉框占第一行余下的宽度（至多 360），不与选项争位置。
        let dev_w = (rc.right - x - s(12)).clamp(0, s(360));
        let _ = MoveWindow(h.device, x + s(6), bar_y + s(2), dev_w, s(300), true);
        // 第二行：两个非严格选项。单独一行，窗口较窄或缩放较大时也完整可见。
        let (opt_y, opt_h) = (bar_y + bar_h + s(2), s(26));
        let _ = MoveWindow(h.mono, s(8), opt_y, s(270), opt_h, true);
        let _ = MoveWindow(h.float, s(8) + s(280), opt_y, s(240), opt_h, true);
        let (fmt_y, fmt_h) = (opt_y + opt_h + s(2), s(24));
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
        // 格式行左侧：采样率指示灯与文字（GUI T6），其后是源与设备格式。
        let _ = MoveWindow(h.rate_lamp, s(8), fmt_y + s(4), s(14), fmt_h - s(4), true);
        let _ = MoveWindow(h.rate, s(24), fmt_y + s(4), s(150), fmt_h - s(4), true);
        let fmt_x = s(180);
        let _ = MoveWindow(
            h.format,
            fmt_x,
            fmt_y + s(4),
            (label_x - s(8) - fmt_x).max(0),
            fmt_h - s(4),
            true,
        );
        let (seek_y, seek_h) = (fmt_y + fmt_h, s(26));
        let _ = MoveWindow(h.seek, s(2), seek_y, (rc.right - s(4)).max(0), seek_h, true);
        let top = seek_y + seek_h + s(2);
        let free = (rc.bottom - top - status_h).max(0);
        if LOG_VISIBLE.get() {
            // 列表与日志之间是可拖动的分隔条；列表至少 60、日志至少 40（96 DPI 像素）。
            let gap = s(6);
            let want = match LOG_HEIGHT.get() {
                0 => free / 4,
                v => s(v),
            };
            let max = (free - gap - s(60)).max(0);
            let log_h = want.clamp(s(40).min(max), max);
            let list_h = (free - gap - log_h).max(0);
            let _ = MoveWindow(h.list, 0, top, rc.right, list_h, true);
            let _ = MoveWindow(h.log, 0, top + list_h + gap, rc.right, log_h, true);
            SPLIT.set((top + list_h, top + list_h + gap));
        } else {
            let _ = MoveWindow(h.list, 0, top, rc.right, free, true);
            SPLIT.set((0, 0));
        }
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
        Input::Command(_) | Input::Sort(_) | Input::MoveBy(_) | Input::Media(_) => {
            selected_rows(h.list)
        }
        _ => Vec::new(),
    };
    let visible = window_visible(&h);
    let ops = APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => g
            .as_mut()
            .map(|app| app.handle(input, &selected, visible))
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    });
    for op in ops {
        exec(op);
    }
}

/// 窗口当前是否可见：未最小化（L1），且不在另一个虚拟桌面上（DWM 隐藏，GUI T6）。隐藏时启动
/// 定时器，回到可见后补画暂存的变化（DWM 合成下被其他窗口遮挡没有可靠的判断方法，不处理）。
fn window_visible(h: &Handles) -> bool {
    let mut cloaked: u32 = 0;
    // SAFETY: 主窗口句柄有效；IsIconic 只查询状态；DWMWA_CLOAKED 的输出为 4 字节整数。
    let (iconic, cloaked) = unsafe {
        let iconic = IsIconic(h.main).as_bool();
        let ok = DwmGetWindowAttribute(
            h.main,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok();
        (iconic, ok && cloaked != 0)
    };
    if cloaked && !VIS_TIMER.replace(true) {
        // SAFETY: 主窗口句柄有效；定时器在 WM_TIMER 中回到可见后销毁。
        unsafe {
            SetTimer(Some(h.main), TIMER_VISIBLE, 500, None);
        }
    }
    !iconic && !cloaked
}

/// 设定列表的选中行（先清除全部选中），第一行为焦点并滚动到可见。
fn select_rows(list: HWND, rows: &[usize]) {
    let set = |row: usize, state: LIST_VIEW_ITEM_STATE_FLAGS, mask: LIST_VIEW_ITEM_STATE_FLAGS| {
        let item = LVITEMW {
            state,
            stateMask: mask,
            ..Default::default()
        };
        // SAFETY: 列表句柄有效；item 在调用期间有效。row 为 usize::MAX（-1）时作用于全部行。
        unsafe {
            SendMessageW(
                list,
                LVM_SETITEMSTATE,
                Some(WPARAM(row)),
                Some(LPARAM(&item as *const LVITEMW as isize)),
            );
        }
    };
    // 选中与焦点两个状态位（windows crate 的状态类型不提供按位或）。
    let both = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);
    set(usize::MAX, LIST_VIEW_ITEM_STATE_FLAGS(0), both);
    for (k, &r) in rows.iter().enumerate() {
        let state = if k == 0 { both } else { LVIS_SELECTED };
        set(r, state, state);
    }
    if let Some(&first) = rows.first() {
        // SAFETY: 列表句柄有效。
        unsafe {
            SendMessageW(
                list,
                LVM_ENSUREVISIBLE,
                Some(WPARAM(first)),
                Some(LPARAM(0)),
            );
        }
    }
}

/// 列标题的排序箭头（`HDF_SORTUP` / `HDF_SORTDOWN`，需要 Common Controls 6）。
fn sort_arrow(list: HWND, arrow: Option<(usize, bool)>) {
    // SAFETY: 列表与其表头句柄有效；HDITEMW 为本地变量，只读写格式字段。
    unsafe {
        let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut _);
        if header.0.is_null() {
            return;
        }
        for col in 0..COLUMNS.len() {
            let mut item = HDITEMW {
                mask: HDI_FORMAT,
                ..Default::default()
            };
            SendMessageW(
                header,
                HDM_GETITEMW,
                Some(WPARAM(col)),
                Some(LPARAM(&mut item as *mut HDITEMW as isize)),
            );
            let mut fmt = item.fmt.0 & !(HDF_SORTUP.0 | HDF_SORTDOWN.0);
            if let Some((c, desc)) = arrow
                && c == col
            {
                fmt |= if desc { HDF_SORTDOWN.0 } else { HDF_SORTUP.0 };
            }
            item.fmt = HEADER_CONTROL_FORMAT_FLAGS(fmt);
            SendMessageW(
                header,
                HDM_SETITEMW,
                Some(WPARAM(col)),
                Some(LPARAM(&item as *const HDITEMW as isize)),
            );
        }
    }
}

/// 注册或注销全局媒体键（GUI T6）。返回日志说明。
fn media_keys(main: HWND, on: bool) -> String {
    let mut state = HOTKEYS_ON.get();
    let mut busy = 0;
    for (k, &(id, vk, _)) in HOTKEYS.iter().enumerate() {
        // SAFETY: 主窗口句柄有效；只注销本窗口注册的编号。
        unsafe {
            if on && !state[k] {
                state[k] = RegisterHotKey(Some(main), id, MOD_NOREPEAT, u32::from(vk)).is_ok();
                busy += usize::from(!state[k]);
            } else if !on && state[k] {
                let _ = UnregisterHotKey(Some(main), id);
                state[k] = false;
            }
        }
    }
    HOTKEYS_ON.set(state);
    match (on, busy) {
        (false, _) => "全局媒体键：已关闭，媒体键只在 QyBit 窗口激活时响应".into(),
        (true, 0) => "全局媒体键：已启用，窗口不在前台时也响应".into(),
        (true, n) => {
            format!("全局媒体键：有 {n} 个已被其他程序占用，这些键只在 QyBit 窗口激活时响应")
        }
    }
}

/// 拖动的落点：光标所在行的上半部为该行之前、下半部为该行之后；最后一行以下为末尾。
/// 光标靠近列表上下边缘时滚动一行。
fn drop_target(list: HWND) -> Option<usize> {
    let mut pt = POINT::default();
    let mut rc = RECT::default();
    // SAFETY: 列表句柄有效；输出参数均为本地变量；LVHITTESTINFO 与 RECT 在调用期间有效。
    unsafe {
        GetCursorPos(&mut pt).ok()?;
        if !ScreenToClient(list, &mut pt).as_bool() {
            return None;
        }
        let _ = GetClientRect(list, &mut rc);
        let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0.max(0) as usize;
        let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut _);
        let mut hr = RECT::default();
        let _ = GetClientRect(header, &mut hr);
        let item_rect = |i: usize| {
            let mut r = RECT {
                left: LVIR_BOUNDS as i32,
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_GETITEMRECT,
                Some(WPARAM(i)),
                Some(LPARAM(&mut r as *mut RECT as isize)),
            );
            r
        };
        let line = if count > 0 {
            let r = item_rect(0);
            (r.bottom - r.top).max(1)
        } else {
            1
        };
        if pt.y < hr.bottom + line / 2 {
            SendMessageW(
                list,
                LVM_SCROLL,
                Some(WPARAM(0)),
                Some(LPARAM(-(line as isize))),
            );
        } else if pt.y > rc.bottom - line / 2 {
            SendMessageW(
                list,
                LVM_SCROLL,
                Some(WPARAM(0)),
                Some(LPARAM(line as isize)),
            );
        }
        if count == 0 {
            return Some(0);
        }
        let mut hit = LVHITTESTINFO {
            pt,
            ..Default::default()
        };
        let i = SendMessageW(
            list,
            LVM_HITTEST,
            None,
            Some(LPARAM(&mut hit as *mut LVHITTESTINFO as isize)),
        )
        .0;
        if i >= 0 {
            let i = i as usize;
            let r = item_rect(i);
            return Some(if pt.y >= (r.top + r.bottom) / 2 {
                i + 1
            } else {
                i
            });
        }
        if pt.y >= item_rect(count - 1).bottom {
            return Some(count);
        }
        Some(SendMessageW(list, LVM_GETTOPINDEX, None, None).0.max(0) as usize)
    }
}

/// 拖动中的落点提示：高亮落点处的行（末尾时高亮最后一行），状态栏写明松开后的位置。
fn drag_hover(h: &Handles) {
    let Some(to) = drop_target(h.list) else {
        return;
    };
    // SAFETY: 列表句柄有效。
    let count = unsafe { SendMessageW(h.list, LVM_GETITEMCOUNT, None, None) }
        .0
        .max(0) as usize;
    let mark = to.min(count.saturating_sub(1));
    if DRAG_MARK.replace(Some(mark)) != Some(mark) {
        drop_mark(h.list, Some(mark));
    }
    let text = if to >= count {
        "拖动：松开后移到列表末尾".to_string()
    } else {
        format!("拖动：松开后移到第 {} 行之前", to + 1)
    };
    let t = wide(&text);
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

/// 设定拖动落点的高亮（`None` 时清除）。
fn drop_mark(list: HWND, row: Option<usize>) {
    for (r, on) in [(usize::MAX, false)]
        .into_iter()
        .chain(row.map(|r| (r, true)))
    {
        let item = LVITEMW {
            state: if on {
                LVIS_DROPHILITED
            } else {
                LIST_VIEW_ITEM_STATE_FLAGS(0)
            },
            stateMask: LVIS_DROPHILITED,
            ..Default::default()
        };
        // SAFETY: 列表句柄有效；item 在调用期间有效；usize::MAX（-1）作用于全部行。
        unsafe {
            SendMessageW(
                list,
                LVM_SETITEMSTATE,
                Some(WPARAM(r)),
                Some(LPARAM(&item as *const LVITEMW as isize)),
            );
        }
    }
}

/// 结束拖动：清除高亮；`commit` 时按落点移动。
fn end_drag(h: &Handles, commit: bool) {
    let Some(rows) = DRAG_ROWS.take() else {
        return;
    };
    let to = if commit { drop_target(h.list) } else { None };
    DRAG_MARK.set(None);
    drop_mark(h.list, None);
    if let Some(to) = to {
        dispatch(Input::Move { rows, to });
    }
    // 恢复状态栏第一栏（拖动中显示的是落点提示）。
    dispatch(Input::Redraw);
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
                // 拖动中第一栏显示落点提示。
                if part == 0 && DRAG_ROWS.with_borrow(Option::is_some) {
                    continue;
                }
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
        Op::LogVisible(on) => {
            LOG_VISIBLE.set(on);
            // SAFETY: 句柄有效；菜单句柄来自主窗口。
            unsafe {
                let _ = ShowWindow(h.log, if on { SW_SHOW } else { SW_HIDE });
                let state = if on { MF_CHECKED } else { MF_UNCHECKED };
                let _ = CheckMenuItem(
                    GetMenu(h.main),
                    u32::from(ID_TOGGLE_LOG),
                    (MF_BYCOMMAND | state).0,
                );
            }
            layout(&h);
        }
        Op::Close => unsafe {
            let _ = PostMessageW(Some(h.main), WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        Op::Rate { lamp, text } => {
            LAMP.set(lamp);
            let changed = RATE_TEXT.with_borrow_mut(|t| {
                let changed = *t != text;
                if changed {
                    t.clone_from(&text);
                }
                changed
            });
            if changed {
                let t = wide(&text);
                unsafe {
                    let _ = SetWindowTextW(h.rate, PCWSTR(t.as_ptr()));
                }
            }
            unsafe {
                let _ = InvalidateRect(Some(h.rate_lamp), None, true);
            }
        }
        Op::Select(rows) => {
            // 在控件处理完当前按键（如 Ctrl+↑ 移动焦点）之后再设定。
            PENDING_SELECT.with_borrow_mut(|p| *p = Some(rows));
            unsafe {
                let _ = PostMessageW(Some(h.main), WM_QY_SELECT, WPARAM(0), LPARAM(0));
            }
        }
        Op::SortArrow(arrow) => sort_arrow(h.list, arrow),
        Op::Devices { names, selected } => unsafe {
            SendMessageW(h.device, CB_RESETCONTENT, None, None);
            for n in names {
                let t = wide(&n);
                SendMessageW(
                    h.device,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(t.as_ptr() as isize)),
                );
            }
            SendMessageW(h.device, CB_SETCURSEL, Some(WPARAM(selected)), None);
        },
        Op::Check(id, on) => unsafe {
            let state = if on { MF_CHECKED } else { MF_UNCHECKED };
            let _ = CheckMenuItem(GetMenu(h.main), u32::from(id), (MF_BYCOMMAND | state).0);
        },
        Op::SaveSettings => {
            let note = match save_settings(&h) {
                Some(Ok((path, 0))) => format!(
                    "已开启“记住播放列表与设置”：退出时保存到 {}（单声道复制与浮点转整数不保存，每次启动默认关闭）",
                    path.display()
                ),
                Some(Ok((path, n))) => format!(
                    "已开启“记住播放列表与设置”：保存到 {}；{n} 个路径无法写入设置文件，未保存",
                    path.display()
                ),
                Some(Err(e)) => format!("无法保存设置：{e}"),
                None => return,
            };
            dispatch(Input::Note(note));
        }
        Op::DeleteSettings => {
            let note = match settings::default_path() {
                Some(path) => match settings::remove(&path) {
                    Ok(()) => format!(
                        "已关闭“记住播放列表与设置”：已删除 {}，下次启动使用默认设置",
                        path.display()
                    ),
                    Err(e) => format!("无法删除设置文件 {}：{e}", path.display()),
                },
                None => "已关闭“记住播放列表与设置”".into(),
            };
            dispatch(Input::Note(note));
        }
        Op::MediaKeys(on) => {
            let note = media_keys(h.main, on);
            exec(Op::Check(ID_MEDIA_KEYS, on));
            dispatch(Input::Note(note));
        }
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

/// 鼠标消息 lparam 中的客户区纵坐标（有符号 16 位）。
fn client_y(lparam: LPARAM) -> i32 {
    i32::from(((lparam.0 as usize >> 16) & 0xFFFF) as u16 as i16)
}

fn in_split(y: i32) -> bool {
    let (a, b) = SPLIT.get();
    a < b && (a..b).contains(&y)
}

/// 鼠标当前是否在分隔条上（WM_SETCURSOR 用）。
fn over_split(hwnd: HWND) -> bool {
    let mut p = POINT::default();
    // SAFETY: 输出参数为本地变量；主窗口句柄有效。
    unsafe {
        if GetCursorPos(&mut p).is_err() || !ScreenToClient(hwnd, &mut p).as_bool() {
            return false;
        }
    }
    in_split(p.y)
}

/// 拖动分隔条到 `y`：日志区上沿跟随鼠标，高度按 96 DPI 像素记下，重新布局。
fn drag_split(y: i32) {
    let Some(h) = HANDLES.get() else {
        return;
    };
    let mut rc = RECT::default();
    let mut lr = RECT::default();
    // SAFETY: 句柄有效；输出参数为本地变量。
    let dpi = unsafe {
        let _ = GetClientRect(h.main, &mut rc);
        let _ = GetWindowRect(h.log, &mut lr);
        let mut bottom = POINT {
            x: lr.left,
            y: lr.bottom,
        };
        let _ = ScreenToClient(h.main, &mut bottom);
        lr.bottom = bottom.y;
        GetDpiForWindow(h.main).max(96) as i32
    };
    let gap = 6 * dpi / 96;
    let log_h = (lr.bottom - (y + gap / 2)).max(0);
    LOG_HEIGHT.set((log_h * 96 / dpi).max(1));
    layout(&h);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            if let Some(h) = HANDLES.get() {
                layout(&h);
            }
            // 最小化或恢复（L1）：恢复时补画最小化期间暂存的变化。
            dispatch(Input::Resized);
            LRESULT(0)
        }
        // 列表与日志之间的分隔条：鼠标在其上时显示上下箭头，按下后拖动调整日志区高度。
        WM_SETCURSOR if over_split(hwnd) => {
            // SAFETY: 系统光标资源，无需释放。
            unsafe {
                if let Ok(c) = LoadCursorW(None, IDC_SIZENS) {
                    SetCursor(Some(c));
                }
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN if in_split(client_y(lparam)) => {
            SPLITTING.set(true);
            // SAFETY: 主窗口句柄有效。
            unsafe {
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE if SPLITTING.get() => {
            drag_split(client_y(lparam));
            LRESULT(0)
        }
        WM_LBUTTONUP if SPLITTING.get() => {
            SPLITTING.set(false);
            // SAFETY: 释放本线程设置的鼠标捕获。
            unsafe {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        // 拖动列表行调整顺序（GUI T6）：LVN_BEGINDRAG 时捕获鼠标，移动时提示落点，松开时移动。
        WM_MOUSEMOVE if DRAG_ROWS.with_borrow(Option::is_some) => {
            if let Some(h) = HANDLES.get() {
                drag_hover(&h);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP if DRAG_ROWS.with_borrow(Option::is_some) => {
            if let Some(h) = HANDLES.get() {
                end_drag(&h, true);
            }
            // SAFETY: 释放本线程设置的鼠标捕获（之后的 WM_CAPTURECHANGED 不再有拖动可取消）。
            unsafe {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        // 捕获被夺走（Esc、切换窗口等）：取消拖动，不移动。
        WM_CAPTURECHANGED => {
            SPLITTING.set(false);
            if let Some(h) = HANDLES.get() {
                end_drag(&h, false);
            }
            LRESULT(0)
        }
        // 采样率指示灯的颜色（GUI T6）：只为指示灯设置文字颜色，背景与窗口相同。
        WM_CTLCOLORSTATIC
            if HANDLES
                .get()
                .is_some_and(|h| lparam.0 == h.rate_lamp.0 as isize) =>
        {
            // SAFETY: wparam 是系统给出的该控件的设备上下文；系统颜色画刷无需释放。
            unsafe {
                let hdc = HDC(wparam.0 as *mut _);
                SetTextColor(hdc, lamp_color(LAMP.get()));
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
            }
        }
        // 键盘媒体键（GUI T6）：窗口在前台时经 WM_APPCOMMAND（焦点控件的默认处理转给父窗口），
        // 窗口不在前台时经全局热键 WM_HOTKEY。
        WM_APPCOMMAND => match app_command(lparam) {
            Some(m) => {
                dispatch(Input::Media(m));
                LRESULT(1)
            }
            // SAFETY: 其余命令交给默认处理。
            None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        },
        WM_HOTKEY => {
            if let Some(&(_, _, m)) = HOTKEYS.iter().find(|k| k.0 as usize == wparam.0) {
                dispatch(Input::Media(m));
            }
            LRESULT(0)
        }
        WM_QY_DEVICES => {
            DEVICES_PENDING.store(false, Ordering::Release);
            refresh_devices();
            LRESULT(0)
        }
        WM_QY_SELECT => {
            if let (Some(h), Some(rows)) = (HANDLES.get(), PENDING_SELECT.take()) {
                select_rows(h.list, &rows);
            }
            LRESULT(0)
        }
        // 窗口在另一个虚拟桌面时的定时检查：回到可见后补画并停止定时器。
        WM_TIMER if wparam.0 == TIMER_VISIBLE => {
            if let Some(h) = HANDLES.get()
                && window_visible(&h)
            {
                VIS_TIMER.set(false);
                // SAFETY: 销毁本窗口的定时器。
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_VISIBLE);
                }
            }
            dispatch(Input::Resized);
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
                // 从另一个虚拟桌面切回时补画暂存的变化。
                dispatch(Input::Resized);
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
                // 端点变化通知不可用时，打开下拉框即重新读取设备列表。
                IDC_DEVICE if u32::from(code) == CBN_DROPDOWN && !WATCHING.get() => {
                    refresh_devices();
                }
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
                IDC_FLOAT if code == 0 => {
                    if let Some(h) = HANDLES.get() {
                        // SAFETY: 复选框句柄有效；只读查询。
                        let r = unsafe { SendMessageW(h.float, BM_GETCHECK, None, None) };
                        dispatch(Input::Float(r.0 as u32 == BST_CHECKED.0));
                    }
                }
                IDC_MONO if code == 0 => {
                    if let Some(h) = HANDLES.get() {
                        // SAFETY: 复选框句柄有效；只读查询。
                        let r = unsafe { SendMessageW(h.mono, BM_GETCHECK, None, None) };
                        dispatch(Input::Mono(r.0 as u32 == BST_CHECKED.0));
                    }
                }
                ID_PLAY..=ID_LAST if code <= 1 => dispatch(Input::Command(id)),
                _ => {}
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            // SAFETY: WM_NOTIFY 的 lparam 指向系统提供的 NMHDR；按 code 转为对应的通知结构
            // （LVN_GETDISPINFOW → NMLVDISPINFOW，NM_DBLCLK → NMITEMACTIVATE，
            // LVN_KEYDOWN → NMLVKEYDOWN，LVN_COLUMNCLICK / LVN_BEGINDRAG → NMLISTVIEW），
            // 只在列表控件的通知上读取。
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
                        LVN_COLUMNCLICK => {
                            let n = &*(lparam.0 as *const NMLISTVIEW);
                            if let Some(key) = usize::try_from(n.iSubItem).ok().and_then(column_key)
                            {
                                dispatch(Input::Sort(key));
                            }
                        }
                        LVN_BEGINDRAG => {
                            let rows = selected_rows(hdr.hwndFrom);
                            if !rows.is_empty() {
                                DRAG_ROWS.with_borrow_mut(|d| *d = Some(rows));
                                SetCapture(hwnd);
                            }
                        }
                        LVN_KEYDOWN => {
                            // NMLVKEYDOWN 按 1 字节对齐（packed）：按值读出，不取字段引用。
                            let k = std::ptr::read_unaligned(lparam.0 as *const NMLVKEYDOWN);
                            let key = k.wVKey;
                            let ctrl = GetKeyState(i32::from(VK_CONTROL.0)) < 0;
                            let rows = selected_rows(hdr.hwndFrom);
                            match key {
                                v if v == VK_ESCAPE.0 && DRAG_ROWS.with_borrow(Option::is_some) => {
                                    // 取消拖动（WM_CAPTURECHANGED 中处理）。
                                    let _ = ReleaseCapture();
                                }
                                v if ctrl && v == VK_UP.0 => dispatch(Input::MoveBy(-1)),
                                v if ctrl && v == VK_DOWN.0 => dispatch(Input::MoveBy(1)),
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
            // 开启“记住”时保存设置（GUI T6；此时窗口位置仍可读取）。
            if let Some(h) = HANDLES.get() {
                let _ = save_settings(&h);
            }
            // SAFETY: 销毁本窗口；之后收到 WM_DESTROY。
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        // 注销或关机：同样保存设置。
        WM_ENDSESSION => {
            if wparam.0 != 0
                && let Some(h) = HANDLES.get()
            {
                let _ = save_settings(&h);
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
