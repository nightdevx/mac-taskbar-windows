//! Screen-edge reservation through the shell's AppBar API, so maximised
//! windows stop at our bars instead of sliding under them.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA, SHAppBarMessage,
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Strip `thickness` px thick along `edge` of `monitor`.
pub fn edge_rect(monitor: RECT, edge: Edge, thickness: i32) -> RECT {
    match edge {
        Edge::Top => RECT {
            bottom: monitor.top + thickness,
            ..monitor
        },
        Edge::Bottom => RECT {
            top: monitor.bottom - thickness,
            ..monitor
        },
        Edge::Left => RECT {
            right: monitor.left + thickness,
            ..monitor
        },
        Edge::Right => RECT {
            left: monitor.right - thickness,
            ..monitor
        },
    }
}

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        ..Default::default()
    }
}

pub fn register(hwnd: HWND, callback_msg: u32) {
    let mut d = APPBARDATA {
        uCallbackMessage: callback_msg,
        ..data(hwnd)
    };
    unsafe { SHAppBarMessage(ABM_NEW, &mut d) };
}

/// Asks the shell for a strip along `edge`, returns the rect it granted.
/// Another bar on the same edge (the Windows taskbar) pushes ours inwards.
pub fn set_edge(hwnd: HWND, edge: Edge, monitor: RECT, thickness: i32) -> RECT {
    let abe = match edge {
        Edge::Top => ABE_TOP,
        Edge::Bottom => ABE_BOTTOM,
        Edge::Left => ABE_LEFT,
        Edge::Right => ABE_RIGHT,
    };
    let mut d = APPBARDATA {
        uEdge: abe,
        rc: edge_rect(monitor, edge, thickness),
        ..data(hwnd)
    };
    unsafe {
        SHAppBarMessage(ABM_QUERYPOS, &mut d);
        match edge {
            Edge::Top => d.rc.bottom = d.rc.top + thickness,
            Edge::Bottom => d.rc.top = d.rc.bottom - thickness,
            Edge::Left => d.rc.right = d.rc.left + thickness,
            Edge::Right => d.rc.left = d.rc.right - thickness,
        }
        SHAppBarMessage(ABM_SETPOS, &mut d);
    }
    d.rc
}

pub fn remove(hwnd: HWND) {
    let mut d = data(hwnd);
    unsafe { SHAppBarMessage(ABM_REMOVE, &mut d) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_rects_span_monitor_width() {
        let primary = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        assert_eq!(
            edge_rect(primary, Edge::Top, 42),
            RECT {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 42
            }
        );
        assert_eq!(
            edge_rect(primary, Edge::Bottom, 96),
            RECT {
                left: 0,
                top: 1344,
                right: 2560,
                bottom: 1440
            }
        );
        // Secondary monitor left of and above the primary one.
        let second = RECT {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 880,
        };
        assert_eq!(
            edge_rect(second, Edge::Top, 28),
            RECT {
                left: -1920,
                top: -200,
                right: 0,
                bottom: -172
            }
        );
        assert_eq!(
            edge_rect(second, Edge::Bottom, 80),
            RECT {
                left: -1920,
                top: 800,
                right: 0,
                bottom: 880
            }
        );
    }
}
