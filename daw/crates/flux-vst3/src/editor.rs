//! A plugin's editor in its own top-level window (Windows), owned by the plugin host process.
//!
//! The window isn't parented to Flux's: cross-process parent/owner links attach the two processes'
//! input queues, so a hung plugin could freeze the DAW. It is only placed over Flux's window.

#![cfg(windows)]

use crate::com::PlugFrame;
use crate::plugin::Plugin;
use crate::util::wide;
use anyhow::{anyhow, bail, Result};
use std::cell::Cell;
use std::rc::Rc;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "FluxPluginEditor";

/// Per-window state reachable from the window procedure (GWLP_USERDATA).
struct WinState {
    view: *mut IPlugView,
    resizable: bool,
    /// We are resizing the window ourselves (don't echo WM_SIZE back to the plugin).
    in_resize: Cell<bool>,
    closed: Cell<bool>,
}

pub struct Editor {
    hwnd: HWND,
    view: ComPtr<IPlugView>,
    _frame: ComWrapper<PlugFrame>,
    state: Rc<WinState>,
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let name = wide(CLASS);
        // SAFETY: registering a window class with a static name and a valid window procedure.
        unsafe {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(std::ptr::null()),
                hIcon: std::ptr::null_mut(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: name.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            RegisterClassExW(&wc);
            // the class keeps a pointer to the name
            std::mem::forget(name);
        }
    });
}

fn style(resizable: bool) -> (u32, u32) {
    let mut s = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
    if resizable {
        s |= WS_THICKFRAME | WS_MAXIMIZEBOX;
    }
    (s, 0)
}

/// Outer window size for a client area of `w` x `h` at the window's DPI.
fn outer_size(hwnd: HWND, resizable: bool, w: i32, h: i32) -> (i32, i32) {
    let (s, ex) = style(resizable);
    let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
    // SAFETY: plain Win32 calls with a valid RECT.
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        AdjustWindowRectExForDpi(&mut r, s, 0, ex, dpi);
    }
    (r.right - r.left, r.bottom - r.top)
}

impl Editor {
    /// Create the plugin's view and show it in a new window. `near`: a window to center over.
    pub fn open(plugin: &Plugin, title: &str, near: Option<isize>) -> Result<Editor> {
        let view = plugin.create_view().ok_or_else(|| anyhow!("'{}' has no editor", plugin.class().name))?;
        // SAFETY: calls on a live view; window handles are created and owned here.
        unsafe {
            if view.isPlatformTypeSupported(kPlatformTypeHWND) != kResultTrue {
                bail!("'{}' editor doesn't support Windows (HWND) windows", plugin.class().name);
            }
            register_class();
            let resizable = view.canResize() == kResultTrue;
            let (s, ex) = style(resizable);
            let cls = wide(CLASS);
            let t = wide(title);
            let hwnd = CreateWindowExW(
                ex,
                cls.as_ptr(),
                t.as_ptr(),
                s,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                640,
                480,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                bail!("CreateWindowExW failed: {}", std::io::Error::last_os_error());
            }
            let state = Rc::new(WinState { view: view.as_ptr(), resizable, in_resize: Cell::new(false), closed: Cell::new(false) });
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::as_ptr(&state) as isize);

            // HiDPI: tell the plugin the scale before it reports its size
            let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            if let Some(cs) = view.cast::<IPlugViewContentScaleSupport>() {
                cs.setContentScaleFactor(scale);
            }
            let st = state.clone();
            let hw = hwnd as isize;
            let frame = PlugFrame::new(Box::new(move |v: *mut IPlugView, mut r: ViewRect| {
                let hwnd = hw as HWND;
                let (w, h) = outer_size(hwnd, st.resizable, r.right - r.left, r.bottom - r.top);
                st.in_resize.set(true);
                SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
                st.in_resize.set(false);
                if let Some(view) = vst3::ComRef::from_raw(v) {
                    view.onSize(&mut r);
                }
                true
            }));
            view.setFrame(frame.as_com_ref::<IPlugFrame>().map_or(std::ptr::null_mut(), |r| r.as_ptr()));

            let mut r: ViewRect = std::mem::zeroed();
            view.getSize(&mut r);
            let (cw, ch) = ((r.right - r.left).max(100), (r.bottom - r.top).max(60));
            let (w, h) = outer_size(hwnd, resizable, cw, ch);
            let (x, y) = match near {
                Some(n) => {
                    let mut pr: RECT = std::mem::zeroed();
                    if GetWindowRect(n as HWND, &mut pr) != 0 {
                        ((pr.left + pr.right - w) / 2, (pr.top + pr.bottom - h) / 2)
                    } else {
                        (CW_USEDEFAULT, CW_USEDEFAULT)
                    }
                }
                None => (CW_USEDEFAULT, CW_USEDEFAULT),
            };
            state.in_resize.set(true);
            if x == CW_USEDEFAULT {
                SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
            } else {
                SetWindowPos(hwnd, std::ptr::null_mut(), x.max(0), y.max(0), w, h, SWP_NOZORDER);
            }
            state.in_resize.set(false);

            let r = view.attached(hwnd, kPlatformTypeHWND);
            if r != kResultOk {
                view.setFrame(std::ptr::null_mut());
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DestroyWindow(hwnd);
                bail!("the editor refused to attach ({})", crate::util::result_name(r));
            }
            ShowWindow(hwnd, SW_SHOWNORMAL);
            raise(hwnd);
            Ok(Editor { hwnd, view, _frame: frame, state })
        }
    }

    /// The user closed the window (the server then calls `close`).
    pub fn closed(&self) -> bool {
        self.state.closed.get()
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd as isize
    }

    /// Bring the window to the front.
    pub fn focus(&self) {
        // SAFETY: our own live window.
        unsafe {
            if IsIconic(self.hwnd) != 0 {
                ShowWindow(self.hwnd, SW_RESTORE);
            } else {
                ShowWindow(self.hwnd, SW_SHOWNORMAL);
            }
        }
        raise(self.hwnd);
    }

    pub fn close(self) {
        drop(self);
    }
}

/// Bring a window to the front. Activation only works if Flux allowed it (`AllowSetForegroundWindow`,
/// which the client does before asking for an editor); otherwise the window is at least put above
/// the others without taking focus.
fn raise(hwnd: HWND) {
    // SAFETY: our own live window.
    unsafe {
        if SetForegroundWindow(hwnd) == 0 || GetForegroundWindow() != hwnd {
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
            SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // SAFETY: detach the view, then destroy our window; the frame object outlives the view's use of it.
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds a pointer to the WinState owned by the Editor (cleared before it drops).
    unsafe {
        let st = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WinState;
        if st.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let st = &*st;
        match msg {
            WM_CLOSE => {
                st.closed.set(true);
                ShowWindow(hwnd, SW_HIDE);
                0
            }
            WM_SIZE if st.resizable && !st.in_resize.get() => {
                if let Some(view) = vst3::ComRef::from_raw(st.view) {
                    let mut cr: RECT = std::mem::zeroed();
                    GetClientRect(hwnd, &mut cr);
                    let mut r = ViewRect { left: 0, top: 0, right: cr.right - cr.left, bottom: cr.bottom - cr.top };
                    view.checkSizeConstraint(&mut r);
                    view.onSize(&mut r);
                }
                0
            }
            WM_DPICHANGED => {
                let dpi = (wp & 0xffff) as f32;
                if let Some(view) = vst3::ComRef::from_raw(st.view)
                    && let Some(cs) = view.cast::<IPlugViewContentScaleSupport>()
                {
                    cs.setContentScaleFactor(dpi / 96.0);
                }
                let r = &*(lp as *const RECT);
                SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_ERASEBKGND => 1,
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
