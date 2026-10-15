//! GUI 选型样例（Windows 原生控件）：工具栏按钮、1000 行虚拟列表（只为可见行取文字）、
//! 每 250 ms 更新一次的状态栏、文件拖放。控件由系统绘制，中文由系统字体链接处理。
//! 环境变量 `QY_EVAL_STATIC=1` 时不定时刷新（测量完全空闲）。
#![windows_subsystem = "windows"]

#[cfg(windows)]
mod app {
    use std::cell::RefCell;
    use std::time::Instant;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, CreateFontIndirectW, HBRUSH, HFONT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::{
        ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
        LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVM_INSERTCOLUMNW,
        LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMCOUNT, LVN_GETDISPINFOW, LVS_EX_DOUBLEBUFFER,
        LVS_EX_FULLROWSELECT, LVS_OWNERDATA, LVS_REPORT, LVS_SHOWSELALWAYS, NMHDR, NMLVDISPINFOW,
        SB_SETTEXTW, STATUSCLASSNAMEW, WC_LISTVIEWW,
    };
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
    use windows::Win32::UI::WindowsAndMessaging::{
        BS_PUSHBUTTON, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
        GetClientRect, GetMessageW, HMENU, IDC_ARROW, LoadCursorW, MSG, MoveWindow,
        NONCLIENTMETRICSW, PostQuitMessage, RegisterClassW, SPI_GETNONCLIENTMETRICS,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SendMessageW, SetTimer, SystemParametersInfoW,
        TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WM_DROPFILES, WM_NOTIFY,
        WM_SETFONT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW,
        WS_TABSTOP, WS_VISIBLE,
    };
    use windows::core::{PCWSTR, PWSTR, Result, w};

    struct State {
        rows: usize,
        start: Instant,
        list: HWND,
        status: HWND,
        buttons: [HWND; 2],
        scratch: Vec<u16>,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn cell(row: usize, col: i32) -> String {
        match col {
            0 => format!("第 {row:04} 首 测试曲目.flac"),
            1 => format!("{}:{:02}", row % 7 + 2, row % 60),
            _ => "FLAC 44.1 kHz 16 位 2 声道".to_string(),
        }
    }

    fn status_text(start: Instant) -> Vec<u16> {
        let t = start.elapsed().as_secs();
        wide(&format!(
            "源 44100 Hz / 2 声道 / 16 位 → 设备 16/16 位（独占）  {}:{:02}  欠载 0",
            t / 60,
            t % 60
        ))
    }

    fn layout(hwnd: HWND) {
        STATE.with_borrow(|s| {
            let Some(s) = s else { return };
            let mut rc = Default::default();
            // SAFETY: hwnd 是本窗口过程收到的有效窗口句柄；rc 为本地变量。
            unsafe {
                let _ = GetClientRect(hwnd, &mut rc);
                let scale = |v: i32| v * GetDpiForWindow(hwnd) as i32 / 96;
                SendMessageW(s.status, WM_SIZE, None, None);
                let bar = scale(32);
                let status_h = scale(24);
                let _ = MoveWindow(s.buttons[0], scale(6), scale(4), scale(72), scale(24), true);
                let _ = MoveWindow(
                    s.buttons[1],
                    scale(84),
                    scale(4),
                    scale(72),
                    scale(24),
                    true,
                );
                let _ = MoveWindow(s.list, 0, bar, rc.right, rc.bottom - bar - status_h, true);
            }
        });
    }

    extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_SIZE => {
                layout(hwnd);
                LRESULT(0)
            }
            WM_TIMER => {
                STATE.with_borrow(|s| {
                    if let Some(s) = s {
                        let text = status_text(s.start);
                        // SAFETY: 状态栏句柄有效；text 在调用期间存活且以 0 结尾。
                        unsafe {
                            SendMessageW(
                                s.status,
                                SB_SETTEXTW,
                                Some(WPARAM(0)),
                                Some(LPARAM(text.as_ptr() as isize)),
                            );
                        }
                    }
                });
                LRESULT(0)
            }
            WM_NOTIFY => {
                // SAFETY: WM_NOTIFY 的 lparam 指向系统提供的 NMHDR；LVN_GETDISPINFOW 时为 NMLVDISPINFOW。
                unsafe {
                    let hdr = &*(lparam.0 as *const NMHDR);
                    if hdr.code == LVN_GETDISPINFOW {
                        let info = &mut *(lparam.0 as *mut NMLVDISPINFOW);
                        if info.item.mask.contains(LVIF_TEXT) && info.item.cchTextMax > 0 {
                            STATE.with_borrow_mut(|s| {
                                if let Some(s) = s {
                                    s.scratch = cell(info.item.iItem as usize, info.item.iSubItem)
                                        .encode_utf16()
                                        .collect();
                                    let n = s.scratch.len().min(info.item.cchTextMax as usize - 1);
                                    let dst =
                                        std::slice::from_raw_parts_mut(info.item.pszText.0, n + 1);
                                    dst[..n].copy_from_slice(&s.scratch[..n]);
                                    dst[n] = 0;
                                }
                            });
                        }
                    }
                }
                LRESULT(0)
            }
            WM_DROPFILES => {
                // SAFETY: wparam 是系统提供的 HDROP，处理后释放。
                unsafe {
                    let drop = HDROP(wparam.0 as *mut _);
                    let n = DragQueryFileW(drop, u32::MAX, None);
                    DragFinish(drop);
                    STATE.with_borrow_mut(|s| {
                        if let Some(s) = s {
                            s.rows += n as usize;
                            SendMessageW(s.list, LVM_SETITEMCOUNT, Some(WPARAM(s.rows)), None);
                        }
                    });
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // SAFETY: 结束消息循环。
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: 其余消息交给默认处理。
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    pub fn run() -> Result<()> {
        // SAFETY: 以下均为 Win32 窗口创建与消息循环调用，参数为本地有效值或系统返回的句柄。
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let icc = INITCOMMONCONTROLSEX {
                dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES,
            };
            let _ = InitCommonControlsEx(&icc);
            let instance = GetModuleHandleW(None)?.into();
            let class = w!("QyBitEvalWin32");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut _),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("QyBit GUI 选型（Windows 原生）"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                900,
                640,
                None,
                None,
                Some(instance),
                None,
            )?;
            // 系统消息字体（Segoe UI；中文经系统字体链接显示）。
            let mut ncm = NONCLIENTMETRICSW {
                cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )?;
            let font: HFONT = CreateFontIndirectW(&ncm.lfMessageFont);
            let child = |classname: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, id: usize| {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    classname,
                    text,
                    WS_CHILD | WS_VISIBLE | style,
                    0,
                    0,
                    10,
                    10,
                    Some(hwnd),
                    Some(HMENU(id as *mut _)),
                    Some(instance),
                    None,
                )
            };
            let play = child(
                w!("BUTTON"),
                w!("播放"),
                WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                1,
            )?;
            let stop = child(
                w!("BUTTON"),
                w!("停止"),
                WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                2,
            )?;
            let list = child(
                WC_LISTVIEWW,
                w!(""),
                WS_BORDER | WINDOW_STYLE(LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS),
                3,
            )?;
            let status = child(STATUSCLASSNAMEW, w!(""), WINDOW_STYLE(0), 4)?;
            for h in [play, stop, list, status] {
                SendMessageW(
                    h,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
            SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                None,
                Some(LPARAM(
                    (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize,
                )),
            );
            for (i, (title, width)) in [("文件", 360), ("时长", 70), ("格式", 220)]
                .iter()
                .enumerate()
            {
                let mut text = wide(title);
                let col = LVCOLUMNW {
                    mask: LVCF_TEXT | LVCF_WIDTH,
                    cx: *width * GetDpiForWindow(hwnd) as i32 / 96,
                    pszText: PWSTR(text.as_mut_ptr()),
                    ..Default::default()
                };
                SendMessageW(
                    list,
                    LVM_INSERTCOLUMNW,
                    Some(WPARAM(i)),
                    Some(LPARAM(&col as *const _ as isize)),
                );
            }
            SendMessageW(list, LVM_SETITEMCOUNT, Some(WPARAM(1000)), None);
            DragAcceptFiles(hwnd, true);
            STATE.with_borrow_mut(|s| {
                *s = Some(State {
                    rows: 1000,
                    start: Instant::now(),
                    list,
                    status,
                    buttons: [play, stop],
                    scratch: Vec::new(),
                })
            });
            layout(hwnd);
            if std::env::var_os("QY_EVAL_STATIC").is_none() {
                SetTimer(Some(hwnd), 1, 250, None);
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    app::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("只用于 Windows");
}
