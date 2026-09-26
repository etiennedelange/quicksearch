//! Showing, hiding, and remembering the main window.

use std::sync::mpsc;
use std::time::Duration;

use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, Runtime, WebviewWindow, WindowEvent,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// Hides the window if the user is looking at it, otherwise brings it up and
/// focuses it. A window that is open but behind another app counts as "not
/// looking at it": the hotkey should raise it, not hide it. Returns whether
/// this call hid the window.
pub fn toggle_main_window<R: Runtime>(app: &AppHandle<R>, label: &str) -> bool {
    let Some(window) = app.get_webview_window(label) else {
        return false;
    };
    let visible = window.is_visible().unwrap_or(false);
    let minimized = window.is_minimized().unwrap_or(false);
    let focused = window.is_focused().unwrap_or(false);
    if visible && !minimized && focused {
        let _ = window.hide();
        true
    } else {
        show_and_focus(&window);
        false
    }
}

pub fn show_and_focus<R: Runtime>(window: &WebviewWindow<R>) {
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// Registers `shortcut` system-wide and calls `on_press` on key-down only,
/// so one keystroke is one toggle.
pub fn register_toggle_hotkey<R: Runtime>(
    app: &AppHandle<R>,
    shortcut: Shortcut,
    on_press: impl Fn(&AppHandle<R>) + Send + Sync + 'static,
) -> Result<(), tauri_plugin_global_shortcut::Error> {
    app.global_shortcut()
        .on_shortcut(shortcut, move |app, _shortcut, event| {
            if event.state == ShortcutState::Pressed {
                on_press(app);
            }
        })
}

/// Outer position and inner size, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Applies saved geometry. The size always applies; the position only if
/// the window would still land on a connected monitor, so unplugging a
/// display can't leave the window somewhere the user can't reach.
pub fn restore<R: Runtime>(window: &WebviewWindow<R>, geometry: Geometry) {
    let _ = window.set_size(PhysicalSize::new(geometry.width, geometry.height));
    let on_screen = window
        .available_monitors()
        .map(|monitors| {
            monitors.iter().any(|monitor| {
                let pos = monitor.position();
                let size = monitor.size();
                intersects(
                    geometry,
                    Geometry {
                        x: pos.x,
                        y: pos.y,
                        width: size.width,
                        height: size.height,
                    },
                )
            })
        })
        .unwrap_or(false);
    if on_screen {
        let _ = window.set_position(PhysicalPosition::new(geometry.x, geometry.y));
    }
}

/// Whether at least a 64px-square corner of the title area of `window`
/// overlaps `monitor` — enough to grab and drag it back.
fn intersects(window: Geometry, monitor: Geometry) -> bool {
    const GRAB: i64 = 64;
    let (wx, wy) = (window.x as i64, window.y as i64);
    let (mx, my) = (monitor.x as i64, monitor.y as i64);
    let right = (wx + window.width as i64).min(mx + monitor.width as i64);
    let left = wx.max(mx);
    let bottom = (wy + GRAB).min(my + monitor.height as i64);
    let top = wy.max(my);
    right - left >= GRAB && bottom - top > 0
}

/// Calls `on_change` with the window's geometry once it has stopped moving
/// or resizing for half a second. Minimized and maximized states aren't
/// reported: restoring those as a normal window's size would be wrong.
pub fn watch<R: Runtime>(window: &WebviewWindow<R>, on_change: impl Fn(Geometry) + Send + 'static) {
    const SETTLE: Duration = Duration::from_millis(500);

    let (tx, rx) = mpsc::channel::<()>();
    let reader = window.clone();
    std::thread::spawn(move || {
        // Block until the first change, then keep swallowing changes until
        // SETTLE passes without one.
        while rx.recv().is_ok() {
            loop {
                match rx.recv_timeout(SETTLE) {
                    Ok(()) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            if let Some(geometry) = current(&reader) {
                on_change(geometry);
            }
        }
    });

    window.on_window_event(move |event| {
        if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
            let _ = tx.send(());
        }
    });
}

fn current<R: Runtime>(window: &WebviewWindow<R>) -> Option<Geometry> {
    if !window.is_visible().ok()? || window.is_minimized().ok()? || window.is_maximized().ok()? {
        return None;
    }
    let position = window.outer_position().ok()?;
    let size = window.inner_size().ok()?;
    Some(Geometry {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Geometry {
        Geometry {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn window_fully_on_monitor_intersects() {
        assert!(intersects(rect(100, 100, 900, 560), rect(0, 0, 1920, 1080)));
    }

    #[test]
    fn window_on_a_disconnected_monitor_does_not_intersect() {
        assert!(!intersects(
            rect(2000, 100, 900, 560),
            rect(0, 0, 1920, 1080)
        ));
    }

    #[test]
    fn window_with_its_title_bar_above_the_screen_does_not_intersect() {
        assert!(!intersects(
            rect(100, -200, 900, 560),
            rect(0, 0, 1920, 1080)
        ));
    }

    #[test]
    fn window_on_a_monitor_left_of_the_primary_intersects() {
        assert!(intersects(
            rect(-1500, 50, 900, 560),
            rect(-1920, 0, 1920, 1080)
        ));
    }
}
