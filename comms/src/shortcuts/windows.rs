use super::SharedShortcuts;
use std::cell::RefCell;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    static SHORTCUTS: RefCell<Option<SharedShortcuts>> = const { RefCell::new(None) };
}

fn input(key: u16, pressed: bool) {
    SHORTCUTS.with_borrow(|shared| {
        if let Some(shared) = shared {
            shared.lock().unwrap().input(key, pressed);
        }
    });
}

unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let event = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
        match w.0 as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => input(event.vkCode as u16, true),
            WM_KEYUP | WM_SYSKEYUP => input(event.vkCode as u16, false),
            _ => (),
        }
    }
    // Do not suppress normal keyboard/mouse behavior in games or other apps.
    unsafe { CallNextHookEx(None, code, w, l) }
}

unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let event = unsafe { &*(l.0 as *const MSLLHOOKSTRUCT) };
        let key = match w.0 as u32 {
            WM_LBUTTONDOWN | WM_LBUTTONUP => Some(0x01),
            WM_RBUTTONDOWN | WM_RBUTTONUP => Some(0x02),
            WM_MBUTTONDOWN | WM_MBUTTONUP => Some(0x04),
            WM_XBUTTONDOWN | WM_XBUTTONUP => Some(if event.mouseData >> 16 == 1 {
                0x05
            } else {
                0x06
            }),
            _ => None,
        };
        if let Some(key) = key {
            input(
                key,
                matches!(
                    w.0 as u32,
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                ),
            );
        }
    }
    unsafe { CallNextHookEx(None, code, w, l) }
}

pub fn start(shared: SharedShortcuts) {
    std::thread::spawn(move || {
        SHORTCUTS.with_borrow_mut(|slot| *slot = Some(shared.clone()));
        let result = unsafe { run() };
        if let Err(error) = result {
            let mut state = shared.lock().unwrap();
            state.lost_input();
            state.error = Some(format!(
                "Global shortcuts unavailable: {error}. Use the microphone button instead."
            ));
        }
    });
}

unsafe fn run() -> windows::core::Result<()> {
    let keyboard_hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), None, 0) }?;
    let mouse_hook = match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), None, 0) } {
        Ok(hook) => hook,
        Err(error) => {
            unsafe { UnhookWindowsHookEx(keyboard_hook) }?;
            return Err(error);
        }
    };
    let mut message = MSG::default();
    let result = loop {
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if status == -1 {
            break Err(windows::core::Error::from_thread());
        }
        if status == 0 {
            break Ok(());
        }
        unsafe { DispatchMessageW(&message) };
    };
    unsafe {
        let _ = UnhookWindowsHookEx(keyboard_hook);
        let _ = UnhookWindowsHookEx(mouse_hook);
    }
    result
}
