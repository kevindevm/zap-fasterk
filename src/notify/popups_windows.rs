//! Optional local desktop cards. The message pump never activates a window.
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::backend::Waker;
use crate::model::Action;
use crate::ui::notification_overlay::Snapshot;

const UPDATE: u32 = WM_APP + 31;
const STOP: u32 = WM_APP + 32;
const WIDTH: i32 = 360;
const HEIGHT: i32 = 128;
const GAP: i32 = 8;

struct State {
    cards: Vec<Snapshot>,
    dismissed: std::collections::HashSet<u64>,
    actions: Vec<Action>,
    waker: Waker,
}

/// Only the dedicated native thread accesses its HWND. The interface sends snapshots.
pub(crate) struct NativePopups {
    state: Arc<Mutex<State>>,
    window: usize,
    thread: Option<JoinHandle<()>>,
}

impl NativePopups {
    pub(crate) fn start(waker: Waker) -> Option<Self> {
        // Synthetic application fixtures must never create or flash desktop windows.
        if cfg!(test) {
            return None;
        }
        let state = Arc::new(Mutex::new(State {
            cards: Vec::new(),
            dismissed: Default::default(),
            actions: Vec::new(),
            waker,
        }));
        let worker = state.clone();
        let (ready, received) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("notification-cards".into())
            .spawn(move || {
                // HWND and GDI objects remain on this thread until its message pump exits.
                unsafe {
                    let Ok(module) = GetModuleHandleW(None) else {
                        let _ = ready.send(None);
                        return;
                    };
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(procedure),
                        hInstance: module.into(),
                        lpszClassName: w!("ZapFasterkLocalCards"),
                        ..Default::default()
                    };
                    // Repeated handle creation reuses the same process-local registered class.
                    let _ = RegisterClassW(&class);
                    let window = CreateWindowExW(
                        WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                        w!("ZapFasterkLocalCards"),
                        w!("Zap Fasterk"),
                        WS_POPUP,
                        0,
                        0,
                        WIDTH,
                        HEIGHT,
                        None,
                        None,
                        Some(module.into()),
                        Some(Arc::as_ptr(&worker).cast()),
                    );
                    let Ok(window) = window else {
                        let _ = ready.send(None);
                        return;
                    };
                    if ready.send(Some(window.0 as usize)).is_err() {
                        let _ = DestroyWindow(window);
                        return;
                    }
                    let mut message = MSG::default();
                    while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            })
            .ok()?;
        match received.recv_timeout(Duration::from_secs(2)) {
            Ok(Some(window)) => Some(Self {
                state,
                window,
                thread: Some(thread),
            }),
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                None
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drop(received);
                None
            }
        }
    }

    /// Replacing the shared snapshot removes old private text before native repaint.
    pub(crate) fn sync(&mut self, mut cards: Vec<Snapshot>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state
            .dismissed
            .retain(|id| cards.iter().any(|card| card.id == *id));
        cards.retain(|card| !state.dismissed.contains(&card.id));
        if state.cards == cards {
            return true;
        }
        state.cards = cards;
        drop(state);
        // Synchronous bounded dispatch scrubs painted pixels before returning from a
        // privacy change. The native thread never calls back into the interface.
        unsafe {
            SendMessageTimeoutW(
                HWND(self.window as *mut _),
                UPDATE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                100,
                None,
            )
            .0 != 0
        }
    }

    pub(crate) fn drain(&self) -> Vec<Action> {
        std::mem::take(
            &mut self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .actions,
        )
    }
}

impl Drop for NativePopups {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .cards
            .clear();
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.window as *mut _)),
                STOP,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The Arc retained by the native thread outlives every dispatch and WM_DESTROY.
    unsafe {
        if message == WM_NCCREATE {
            let creation = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, creation.lpCreateParams as isize);
        }
        let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Mutex<State>;
        if pointer.is_null() {
            return DefWindowProcW(window, message, wparam, lparam);
        }
        let state = &*pointer;
        match message {
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            UPDATE | WM_TIMER | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                refresh(window, state);
                LRESULT(1)
            }
            WM_PAINT => {
                paint(window, state);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 as u16) as i16 as i32;
                let y = ((lparam.0 as usize >> 16) as u16) as i16 as i32;
                let mut guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
                if let Some((index, dismiss)) = hit(x, y, guard.cards.len()) {
                    let card = guard.cards.remove(index);
                    guard.dismissed.insert(card.id);
                    if card.expires > Instant::now() {
                        guard.actions.push(if dismiss {
                            Action::DismissInAppNotification(card.id)
                        } else {
                            Action::OpenInAppNotification(card.id)
                        });
                        guard.waker.wake();
                    }
                }
                drop(guard);
                refresh(window, state);
                LRESULT(0)
            }
            STOP => {
                let _ = DestroyWindow(window);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(window), 1);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

fn hit(x: i32, y: i32, count: usize) -> Option<(usize, bool)> {
    if !(0..WIDTH).contains(&x) || y < 0 {
        return None;
    }
    let index = (y / (HEIGHT + GAP)) as usize;
    let within = y % (HEIGHT + GAP);
    (index < count && within < HEIGHT).then_some((index, x >= WIDTH - 36 && within < 36))
}

unsafe fn refresh(window: HWND, shared: &Mutex<State>) {
    unsafe {
        let now = Instant::now();
        let mut state = shared.lock().unwrap_or_else(|poison| poison.into_inner());
        let previous = state.cards.len();
        state.cards.retain(|card| card.expires > now);
        if state.cards.len() != previous {
            state.waker.wake();
        }
        let _ = KillTimer(Some(window), 1);
        if state.cards.is_empty() {
            let _ = ShowWindow(window, SW_HIDE);
            return;
        }
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            let _ = ShowWindow(window, SW_HIDE);
            return;
        }
        let height = state.cards.len() as i32 * (HEIGHT + GAP) - GAP;
        let delay = state
            .cards
            .iter()
            .map(|card| card.expires.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::from_secs(1))
            .as_millis()
            .clamp(1, u32::MAX as u128) as u32;
        drop(state);
        let _ = SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            info.rcWork.right - WIDTH - 16,
            info.rcWork.bottom - height - 16,
            WIDTH,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = InvalidateRect(Some(window), None, true);
        // Paint synchronously so a privacy update cannot leave old content displayed.
        let _ = UpdateWindow(window);
        SetTimer(Some(window), 1, delay, None);
    }
}

unsafe fn paint(window: HWND, shared: &Mutex<State>) {
    unsafe {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut paint);
        let background = CreateSolidBrush(COLORREF(0x002c2622));
        let _ = FillRect(dc, &paint.rcPaint, background);
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(0x00f5f5f5));
        let previous = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        let state = shared.lock().unwrap_or_else(|poison| poison.into_inner());
        for (index, card) in state.cards.iter().enumerate() {
            if card.expires <= Instant::now() {
                continue;
            }
            let top = index as i32 * (HEIGHT + GAP);
            let mut title = card.title.encode_utf16().collect::<Vec<_>>();
            let mut title_rect = RECT {
                left: 16,
                top: top + 14,
                right: WIDTH - 40,
                bottom: top + 38,
            };
            DrawTextW(
                dc,
                &mut title,
                &mut title_rect,
                DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            let mut body = card.body.encode_utf16().collect::<Vec<_>>();
            let mut body_rect = RECT {
                left: 16,
                top: top + 44,
                right: WIDTH - 16,
                bottom: top + HEIGHT - 14,
            };
            DrawTextW(
                dc,
                &mut body,
                &mut body_rect,
                DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            let mut close = vec![0x00d7];
            let mut close_rect = RECT {
                left: WIDTH - 32,
                top: top + 10,
                right: WIDTH - 10,
                bottom: top + 34,
            };
            DrawTextW(
                dc,
                &mut close,
                &mut close_rect,
                DT_CENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        let _ = SelectObject(dc, previous);
        let _ = DeleteObject(background.into());
        let _ = EndPaint(window, &paint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn click_targets_distinguish_close_card_gap_and_bounds_without_a_native_window() {
        assert_eq!(hit(10, 10, 3), Some((0, false)));
        assert_eq!(hit(WIDTH - 20, 10, 3), Some((0, true)));
        assert_eq!(hit(10, HEIGHT + GAP + 20, 3), Some((1, false)));
        assert_eq!(hit(10, HEIGHT + 2, 3), None);
        assert_eq!(hit(-1, 10, 3), None);
        assert_eq!(hit(10, HEIGHT * 10, 3), None);
        assert!(NativePopups::start(Waker::default()).is_none());
    }
}
