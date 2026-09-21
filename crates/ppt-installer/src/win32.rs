//! 与 Win32 打交道的那一层：窗口、双缓冲贴图、DPI、鼠标、定时器。
//!
//! 界面本身在 `ppt-installer-ui` 里画，这里只负责把它「端」到屏幕上：
//! 一块 DIB 内存位图 + `BitBlt`。用 GDI 而不是 Direct2D，是因为安装程序
//! 要能在最干净、最老的机器上跑起来 —— GDI 从 Windows 2000 起就没变过。

use std::ffi::c_void;

use tiny_skia::Pixmap;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint,
    GetDC, InvalidateRect, ReleaseDC, SelectObject, UpdateWindow, BITMAPINFO, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    KillTimer, LoadCursorW, PostQuitMessage, RegisterClassW, SendMessageW, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow, TranslateMessage, GWLP_USERDATA,
    HTCAPTION, IDC_ARROW, IDC_HAND, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_NCLBUTTONDOWN, WNDCLASSW,
    WNDPROC, WS_MINIMIZEBOX, WS_POPUP, CS_HREDRAW, CS_VREDRAW,
};

/// 一块 DIB 内存位图：界面先画到这里，再一次性贴到窗口上。
///
/// 直接往窗口 DC 上逐像素画会很慢也很闪；内存位图 + BitBlt 是双缓冲的老办法，
/// 在任何 Windows 上都稳定。
struct Dib {
    mem_dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    fn new(w: i32, h: i32) -> Option<Dib> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let screen = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            if mem_dc.is_invalid() {
                return None;
            }
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize =
                std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w;
            // 负数高度 = 自上而下存，与 tiny-skia 的像素顺序一致
            info.bmiHeader.biHeight = -h;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;

            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(mem_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(b) => b,
                Err(_) => {
                    let _ = DeleteDC(mem_dc);
                    return None;
                }
            };
            if bitmap.is_invalid() || bits.is_null() {
                let _ = DeleteDC(mem_dc);
                return None;
            }
            let old = SelectObject(mem_dc, bitmap.into());
            Some(Dib {
                mem_dc,
                bitmap,
                old,
                bits: bits as *mut u8,
                w,
                h,
            })
        }
    }

    /// 把界面画好的图搬进 DIB。
    ///
    /// tiny-skia 给的是**预乘** RGBA，而界面每帧都会先刷一层不透明白底，
    /// 所以每个像素的 alpha 都是 255、RGB 就是最终颜色 ——
    /// 只需要把 R 与 B 换个位置（GDI 要 BGRA）。
    fn upload(&mut self, pixmap: &Pixmap) {
        let src = pixmap.data();
        let n = (self.w * self.h) as usize;
        if src.len() < n * 4 {
            return;
        }
        unsafe {
            let dst = std::slice::from_raw_parts_mut(self.bits, n * 4);
            for i in 0..n {
                let s = i * 4;
                dst[s] = src[s + 2];
                dst[s + 1] = src[s + 1];
                dst[s + 2] = src[s];
                dst[s + 3] = 255;
            }
        }
    }

    fn blit(&self, dst: HDC) {
        unsafe {
            let _ = BitBlt(dst, 0, 0, self.w, self.h, Some(self.mem_dc), 0, 0, SRCCOPY);
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem_dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

/// 窗口的绘制状态。窗口过程在 `main.rs`，这里只提供画布与系统调用。
pub struct Win {
    pub hwnd: HWND,
    pub scale: f32,
    dib: Option<Dib>,
    pixmap: Option<Pixmap>,
    pub dirty: bool,
    hand_cursor: bool,
}

impl Win {
    pub fn new(hwnd: HWND, scale: f32) -> Win {
        Win {
            hwnd,
            scale,
            dib: None,
            pixmap: None,
            dirty: true,
            hand_cursor: false,
        }
    }

    pub fn client_size(&self) -> (i32, i32) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
        }
        ((r.right - r.left).max(1), (r.bottom - r.top).max(1))
    }

    /// 取一块与客户区同尺寸的画布（尺寸变了就重建）。
    pub fn canvas(&mut self) -> &mut Pixmap {
        let (w, h) = self.client_size();
        let need_new = match (&self.pixmap, &self.dib) {
            (Some(p), Some(d)) => {
                p.width() as i32 != w || p.height() as i32 != h || d.w != w || d.h != h
            }
            _ => true,
        };
        if need_new {
            self.pixmap = Pixmap::new(w as u32, h as u32);
            self.dib = Dib::new(w, h);
        }
        self.pixmap.as_mut().expect("画布创建失败")
    }

    /// 把画布贴到窗口上（在 WM_PAINT 里调用）。
    pub fn present(&mut self) {
        let (Some(pixmap), Some(dib)) = (self.pixmap.as_mut(), self.dib.as_mut()) else {
            return;
        };
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            dib.upload(pixmap);
            dib.blit(hdc);
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// 窗口被拖到另一个 DPI 的屏幕上时，重新取一次缩放系数。
    pub fn refresh_scale(&mut self) {
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd);
            let s = if dpi == 0 { 96.0 } else { dpi as f32 / 96.0 };
            if (s - self.scale).abs() > 0.01 {
                self.scale = s;
                self.dirty = true;
            }
        }
    }

    pub fn set_hand_cursor(&mut self, hand: bool) {
        if self.hand_cursor == hand {
            return;
        }
        self.hand_cursor = hand;
        unsafe {
            if let Ok(c) = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }) {
                SetCursor(Some(c));
            }
        }
    }
}

/// 窗口过程里用到的用户数据：一个指向应用状态的裸指针。
///
/// 单窗口、单线程的安装程序，这样最直接；也避免了为了一点点状态去引入全局锁。
pub fn set_user_data<A>(hwnd: HWND, app: &mut A) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as *mut A as isize);
    }
}

/// 取回 `set_user_data` 存进去的应用状态。
///
/// # Safety
/// 调用者必须保证 `A` 与存入时的类型一致，且窗口还活着。
pub unsafe fn user_data<'a, A>(hwnd: HWND) -> Option<&'a mut A> {
    let p = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut A))
    }
}

/// 建一个无边框窗口：标题栏、关闭按钮、圆角都由我们自己画。
///
/// 尺寸按 96 DPI 的设计尺寸乘 DPI 系数，居中到屏幕。
pub fn create_window(
    title: &str,
    design_w: f32,
    design_h: f32,
    proc_: WNDPROC,
) -> Option<(HWND, f32)> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("OpenPPTViewSetupWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: proc_,
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        // 同一个类重复注册会失败，忽略即可（一个进程只会建一个窗口）
        let _ = RegisterClassW(&wc);

        let scale = primary_dpi() / 96.0;
        let w = (design_w * scale).round() as i32;
        let h = (design_h * scale).round() as i32;
        let (sw, sh) = screen_size();
        let x = ((sw - w) / 2).max(0);
        let y = ((sh - h) / 2).max(0);

        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(title_w.as_ptr()),
            WS_POPUP | WS_MINIMIZEBOX,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;

        // 无边框窗口默认没有投影，把 DWM 边框往外扩 1px 就有影子了；
        // Win11 上顺便要个圆角。失败都无所谓，只是好看一点。
        let margins = MARGINS {
            cxLeftWidth: 1,
            cxRightWidth: 1,
            cyTopHeight: 1,
            cyBottomHeight: 1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let pref: i32 = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const i32 as *const c_void,
            std::mem::size_of::<i32>() as u32,
        );

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
        Some((hwnd, scale))
    }
}

fn primary_dpi() -> f32 {
    // 还没建窗口，拿不到窗口 DPI，先用桌面 DC 估一个
    unsafe {
        let dc = GetDC(None);
        let dpi = windows::Win32::Graphics::Gdi::GetDeviceCaps(
            Some(dc),
            windows::Win32::Graphics::Gdi::LOGPIXELSX,
        );
        ReleaseDC(None, dc);
        if dpi <= 0 {
            96.0
        } else {
            dpi as f32
        }
    }
}

fn screen_size() -> (i32, i32) {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
    }
}

/// 消息循环。跑完返回进程退出码。
pub fn run_message_loop() -> i32 {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        msg.wParam.0 as i32
    }
}

/// 默认窗口过程（我们自己不处理的消息交给系统）。
pub fn default_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 定时器：用来推进安装步骤、刷新界面。
pub fn set_timer(hwnd: HWND, id: usize, ms: u32) {
    unsafe {
        SetTimer(Some(hwnd), id, ms, None);
    }
}

pub fn kill_timer(hwnd: HWND, id: usize) {
    unsafe {
        let _ = KillTimer(Some(hwnd), id);
    }
}

pub fn close_window(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

pub fn post_quit() {
    unsafe { PostQuitMessage(0) }
}

/// 让窗口能被拖动：在品牌带区域按住时当作标题栏处理。
pub fn begin_drag(hwnd: HWND) {
    unsafe {
        let _ = ReleaseCapture();
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            Some(LPARAM(0)),
        );
    }
}

/// 把窗口客户区抓成一张图。
///
/// 自检用：它验证的是「建窗口 → 画布 → DIB → 贴到屏幕 → 再读回来」这整条链路。
/// 界面本身长什么样，靠 `ppt-installer-ui` 的预览就够了；这条链路只有真跑窗口才知道对不对。
pub fn capture_client(hwnd: HWND) -> Option<Pixmap> {
    use windows::Win32::Graphics::Gdi::{CreateCompatibleBitmap, GetDIBits};
    unsafe {
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        let (w, h) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));

        let src = GetDC(Some(hwnd));
        let mem = CreateCompatibleDC(Some(src));
        let bmp = CreateCompatibleBitmap(src, w, h);
        let old = SelectObject(mem, bmp.into());
        let _ = BitBlt(mem, 0, 0, w, h, Some(src), 0, 0, SRCCOPY);

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize =
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let n = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(Some(hwnd), src);

        if n == 0 {
            return None;
        }
        let mut pix = Pixmap::new(w as u32, h as u32)?;
        let dst = pix.data_mut();
        for i in 0..(w * h) as usize {
            let s = i * 4;
            dst[s] = buf[s + 2];
            dst[s + 1] = buf[s + 1];
            dst[s + 2] = buf[s];
            dst[s + 3] = 255;
        }
        Some(pix)
    }
}
