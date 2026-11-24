//! The pure decisions behind a right-click gesture, free of Win32 calls so tests drive them.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::Accessibility::{
    ROLE_SYSTEM_CELL, ROLE_SYSTEM_CHECKBUTTON, ROLE_SYSTEM_CLIENT, ROLE_SYSTEM_GRAPHIC,
    ROLE_SYSTEM_LIST, ROLE_SYSTEM_LISTITEM, ROLE_SYSTEM_PANE, ROLE_SYSTEM_STATICTEXT,
    ROLE_SYSTEM_TEXT, ROLE_SYSTEM_WINDOW,
};

/// How long a prebuilt menu waits for the `WM_CONTEXTMENU` that uses it.
pub(crate) const READY_TTL: Duration = Duration::from_secs(10);

/// One folder view's right-click gesture. `M` is the menu type, generic so tests need no COM.
#[derive(Default)]
pub(crate) enum Gesture<M> {
    #[default]
    Idle,
    /// The right button went down at `down`, in `child`'s client coordinates, and the arm
    /// timer `timer_id` on `child` is pending for press generation `gen`.
    Armed {
        child: HWND,
        down: POINT,
        timer_id: usize,
        gen: u64,
    },
    Ready {
        child: HWND,
        down: POINT,
        down_screen: POINT,
        background: bool,
        menu: M,
        built_at: Instant,
        timer_id: usize,
    },
    /// Right-press was queued to `rwm-host.exe` at `t = 0 ms`; preserves `down_screen` so release
    /// can skip duplicate `AccessibleObjectFromPoint` hit-testing.
    HostPrefetched { down: POINT, down_screen: POINT },
}

impl<M> Gesture<M> {
    /// The child and timer ID for any active arm timer.
    pub(crate) fn armed_timer(&self) -> Option<(HWND, usize)> {
        match self {
            Self::Armed {
                child, timer_id, ..
            } => Some((*child, *timer_id)),
            _ => None,
        }
    }

    /// The child and timer ID for an active ready-expiry timer.
    pub(crate) fn ready_timer(&self) -> Option<(HWND, usize)> {
        match self {
            Self::Ready {
                child, timer_id, ..
            } => Some((*child, *timer_id)),
            _ => None,
        }
    }

    /// Consumes the arm only when its child and timer ID match, returning `(down, gen)`.
    pub(crate) fn fire(&mut self, child: HWND, timer_id: usize) -> Option<(POINT, u64)> {
        match *self {
            Self::Armed {
                child: armed,
                down,
                timer_id: expected,
                gen,
            } if armed == child && expected == timer_id => {
                *self = Self::Idle;
                Some((down, gen))
            }
            _ => None,
        }
    }

    /// Drops an expired `Ready` menu if its child and timer ID match.
    pub(crate) fn expire(&mut self, child: HWND, timer_id: usize) -> Option<M> {
        let matches = matches!(
            self,
            Self::Ready { child: ready, timer_id: expected, .. }
                if *ready == child && *expected == timer_id
        );
        if matches {
            let Self::Ready { menu, .. } = std::mem::take(self) else {
                unreachable!()
            };
            Some(menu)
        } else {
            None
        }
    }

    /// True once a pressed/ready/host-prefetched gesture has left the system drag rectangle.
    pub(crate) fn is_drag(&self, at: POINT, (cx, cy): (i32, i32)) -> bool {
        let down = match self {
            Self::Armed { down, .. }
            | Self::Ready { down, .. }
            | Self::HostPrefetched { down, .. } => down,
            Self::Idle => return false,
        };
        (at.x - down.x).abs() > cx || (at.y - down.y).abs() > cy
    }

    /// Consumes the gesture and returns `(optional ready menu, optional hit cache (down_screen, background))`.
    pub(crate) fn take_for_context_menu(self, now: Instant) -> (Option<M>, Option<(POINT, bool)>) {
        match self {
            Self::Ready {
                down_screen,
                background,
                menu,
                built_at,
                ..
            } if now.saturating_duration_since(built_at) < READY_TTL => {
                (Some(menu), Some((down_screen, background)))
            }
            Self::HostPrefetched { down_screen, .. } => (None, Some((down_screen, false))),
            _ => (None, None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoverAction {
    None,
    ReArm(u32),
    Prefetch(POINT),
}

/// Coalesces hover dwell checks without re-arming timers inside the deadzone jitter band.
#[derive(Debug, Default)]
pub(crate) struct HoverTracker {
    pub(crate) last_pt: Option<POINT>,
    pub(crate) last_move_tick_ms: u64,
    pub(crate) timer_active: bool,
    pub(crate) last_prefetched_pt: Option<POINT>,
}

impl HoverTracker {
    /// Records pointer movement (`client` coordinates). Returns `true` when caller should start
    /// a new coalesced timer; subsequent movements reset `last_move_tick_ms` with zero syscalls.
    #[inline]
    pub(crate) fn on_move(&mut self, pt: POINT, now_ms: u64, deadzone_px: i32) -> bool {
        if let Some(prev) = self.last_pt {
            if (pt.x - prev.x).abs() <= deadzone_px && (pt.y - prev.y).abs() <= deadzone_px {
                return false;
            }
        }
        self.last_pt = Some(pt);
        self.last_move_tick_ms = now_ms;
        if self.timer_active {
            false
        } else {
            self.timer_active = true;
            true
        }
    }

    pub(crate) fn on_timer(
        &mut self,
        now_ms: u64,
        dwell_ms: u32,
        deadzone_px: i32,
    ) -> HoverAction {
        let Some(pt) = self.last_pt else {
            self.timer_active = false;
            return HoverAction::None;
        };
        let elapsed = now_ms.saturating_sub(self.last_move_tick_ms);
        if elapsed < u64::from(dwell_ms) {
            let remaining = (u64::from(dwell_ms) - elapsed).max(15).min(u64::from(dwell_ms)) as u32;
            self.timer_active = true;
            return HoverAction::ReArm(remaining);
        }
        self.timer_active = false;
        if let Some(prev) = self.last_prefetched_pt {
            if (pt.x - prev.x).abs() <= deadzone_px && (pt.y - prev.y).abs() <= deadzone_px {
                return HoverAction::None;
            }
        }
        HoverAction::Prefetch(pt)
    }

    pub(crate) fn mark_prefetched(&mut self, pt: POINT) {
        self.last_prefetched_pt = Some(pt);
    }

    pub(crate) fn reset(&mut self) -> bool {
        let was_active = self.timer_active;
        self.last_pt = None;
        self.last_move_tick_ms = 0;
        self.timer_active = false;
        self.last_prefetched_pt = None;
        was_active
    }
}

/// True when `a` and `b` lie within Windows drag tolerances `(cx, cy)`.
#[inline]
pub(crate) fn within_drag_rect(a: POINT, b: POINT, (cx, cy): (i32, i32)) -> bool {
    (a.x - b.x).abs() <= cx && (a.y - b.y).abs() <= cy
}

/// What a point inside a folder view lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Item,
    Background,
    /// A column header, scroll bar, group header or similar part that keeps Explorer's menu.
    Other,
}

/// Classifies an accessibility hit test from the innermost element outward. Text, icons,
/// cells and check boxes belong to the list item around them, so the walk goes on to the
/// parent. `None` means the walk ended undecided.
pub(crate) fn classify_roles(roles: impl IntoIterator<Item = u32>) -> Option<Hit> {
    for role in roles {
        match role {
            ROLE_SYSTEM_LISTITEM => return Some(Hit::Item),
            ROLE_SYSTEM_LIST | ROLE_SYSTEM_CLIENT | ROLE_SYSTEM_PANE | ROLE_SYSTEM_WINDOW => {
                return Some(Hit::Background)
            }
            ROLE_SYSTEM_CELL
            | ROLE_SYSTEM_GRAPHIC
            | ROLE_SYSTEM_STATICTEXT
            | ROLE_SYSTEM_TEXT
            | ROLE_SYSTEM_CHECKBUTTON => {}
            _ => return Some(Hit::Other),
        }
    }
    None
}

pub(crate) fn point_from_lparam(lparam: isize) -> POINT {
    let (x, y) = (lparam as u16 as i16, (lparam >> 16) as u16 as i16);
    POINT {
        x: x.into(),
        y: y.into(),
    }
}

pub(crate) fn lparam_from_point(point: POINT) -> isize {
    (((point.y as u16 as u32) << 16) | point.x as u16 as u32) as isize
}

/// The screen point of a `WM_CONTEXTMENU`, or `None` when the keyboard raised it. Senders pass
/// that marker both sign-extended (`-1`) and not (`0xFFFF_FFFF`).
pub(crate) fn context_menu_point(lparam: isize) -> Option<POINT> {
    let point = point_from_lparam(lparam);
    (point.x != -1 || point.y != -1).then_some(point)
}

/// Where a keyboard-raised menu opens: the focused item when known, else the view's middle,
/// kept inside the view so a scrolled-away item cannot push the menu off the window.
pub(crate) fn keyboard_anchor(focus: Option<POINT>, view: RECT) -> POINT {
    let middle = POINT {
        x: (view.left + view.right) / 2,
        y: (view.top + view.bottom) / 2,
    };
    let point = focus.unwrap_or(middle);
    POINT {
        x: point.x.clamp(view.left, view.right.max(view.left)),
        y: point.y.clamp(view.top, view.bottom.max(view.top)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Accessibility::{
        ROLE_SYSTEM_COLUMNHEADER, ROLE_SYSTEM_GROUPING, ROLE_SYSTEM_SCROLLBAR,
    };

    fn window(raw: usize) -> HWND {
        HWND(raw as *mut _)
    }

    fn armed(child: usize) -> Gesture<&'static str> {
        Gesture::Armed {
            child: window(child),
            down: POINT { x: 100, y: 100 },
            timer_id: 0x5257_4D01,
            gen: 1,
        }
    }

    #[test]
    fn the_arm_fires_once_and_only_for_its_own_child() {
        let mut gesture = armed(1);
        assert_eq!(gesture.fire(window(2), 0x5257_4D01), None);
        assert_eq!(gesture.fire(window(1), 0x5257_4D02), None);
        assert_eq!(gesture.armed_timer(), Some((window(1), 0x5257_4D01)));
        assert_eq!(
            gesture.fire(window(1), 0x5257_4D01),
            Some((POINT { x: 100, y: 100 }, 1))
        );
        assert!(matches!(gesture, Gesture::Idle));
        assert_eq!(
            gesture.fire(window(1), 0x5257_4D01),
            None,
            "a stale WM_TIMER finds no arm"
        );
    }

    #[test]
    fn a_drag_starts_beyond_the_drag_rectangle_on_either_axis() {
        let drag = (4, 6);
        assert!(!armed(1).is_drag(POINT { x: 104, y: 94 }, drag));
        assert!(armed(1).is_drag(POINT { x: 95, y: 100 }, drag));
        assert!(armed(1).is_drag(POINT { x: 100, y: 107 }, drag));
        let ready = Gesture::Ready {
            child: window(1),
            down: POINT { x: 100, y: 100 },
            down_screen: POINT { x: 200, y: 200 },
            background: false,
            menu: "menu",
            built_at: Instant::now(),
            timer_id: 0x5257_4E01,
        };
        assert!(ready.is_drag(POINT { x: 110, y: 100 }, drag));
        assert!(!Gesture::<&str>::Idle.is_drag(POINT { x: 900, y: 900 }, drag));
    }

    #[test]
    fn only_a_ready_menu_younger_than_the_ttl_is_reused() {
        let built_at = Instant::now();
        let ready = || Gesture::Ready {
            child: window(1),
            down: POINT { x: 100, y: 100 },
            down_screen: POINT { x: 200, y: 200 },
            background: true,
            menu: "menu",
            built_at,
            timer_id: 0x5257_4E01,
        };
        assert_eq!(
            ready().take_for_context_menu(built_at + READY_TTL / 2),
            (Some("menu"), Some((POINT { x: 200, y: 200 }, true)))
        );
        assert_eq!(ready().take_for_context_menu(built_at + READY_TTL), (None, None));
        assert_eq!(armed(1).take_for_context_menu(built_at), (None, None));
        assert_eq!(ready().armed_timer(), None);
        let mut expiring = ready();
        assert_eq!(expiring.expire(window(1), 0x5257_4E02), None);
        assert_eq!(expiring.expire(window(1), 0x5257_4E01), Some("menu"));
        assert!(matches!(expiring, Gesture::Idle));

        let host_prefetched: Gesture<&str> = Gesture::HostPrefetched {
            down: POINT { x: 10, y: 10 },
            down_screen: POINT { x: 500, y: 500 },
        };
        assert!(host_prefetched.is_drag(POINT { x: 20, y: 10 }, (4, 4)));
        assert_eq!(
            host_prefetched.take_for_context_menu(built_at),
            (None, Some((POINT { x: 500, y: 500 }, false)))
        );
    }

    #[test]
    fn hits_resolve_from_the_innermost_element_outward() {
        let text_in_item = [ROLE_SYSTEM_TEXT, ROLE_SYSTEM_GRAPHIC, ROLE_SYSTEM_LISTITEM];
        assert_eq!(classify_roles(text_in_item), Some(Hit::Item));
        for surface in [ROLE_SYSTEM_LIST, ROLE_SYSTEM_CLIENT, ROLE_SYSTEM_PANE] {
            assert_eq!(classify_roles([surface]), Some(Hit::Background));
        }
        let empty_folder_text = [ROLE_SYSTEM_STATICTEXT, ROLE_SYSTEM_LIST];
        assert_eq!(classify_roles(empty_folder_text), Some(Hit::Background));
        for part in [
            ROLE_SYSTEM_COLUMNHEADER,
            ROLE_SYSTEM_SCROLLBAR,
            ROLE_SYSTEM_GROUPING,
        ] {
            assert_eq!(classify_roles([part, ROLE_SYSTEM_LIST]), Some(Hit::Other));
        }
        assert_eq!(classify_roles([ROLE_SYSTEM_TEXT; 3]), None);
    }

    #[test]
    fn lparam_points_are_signed_and_the_keyboard_marker_has_no_point() {
        for point in [
            POINT { x: 320, y: 480 },
            POINT { x: -1920, y: -45 },
            POINT { x: -1, y: 7 },
        ] {
            assert_eq!(context_menu_point(lparam_from_point(point)), Some(point));
        }
        assert_eq!(context_menu_point(-1), None);
        assert_eq!(context_menu_point(0xFFFF_FFFF), None);
    }

    #[test]
    fn the_keyboard_anchor_stays_inside_the_view() {
        let view = RECT {
            left: 100,
            top: 100,
            right: 500,
            bottom: 400,
        };
        let focus = POINT { x: 170, y: 150 };
        assert_eq!(keyboard_anchor(None, view), POINT { x: 300, y: 250 });
        assert_eq!(keyboard_anchor(Some(focus), view), focus);
        let scrolled_away = POINT { x: 170, y: 910 };
        assert_eq!(
            keyboard_anchor(Some(scrolled_away), view),
            POINT { x: 170, y: 400 }
        );
    }

    #[test]
    fn hover_tracker_coalesces_jitter_respects_dwell_and_resets() {
        let mut tracker = HoverTracker::default();
        assert!(tracker.on_move(POINT { x: 100, y: 100 }, 1_000, 3));
        // Deadzone jitter does not re-arm or bump timestamp.
        assert!(!tracker.on_move(POINT { x: 102, y: 98 }, 1_050, 3));
        assert_eq!(tracker.last_move_tick_ms, 1_000);
        // Moving outside deadzone while timer active updates timestamp without requesting SetTimer.
        assert!(!tracker.on_move(POINT { x: 120, y: 100 }, 1_070, 3));
        assert_eq!(tracker.last_move_tick_ms, 1_070);
        // Before dwell expires, asks for remaining wait.
        assert_eq!(tracker.on_timer(1_110, 120, 3), HoverAction::ReArm(80));
        // Once dwell elapses, requests prefetch and marks point to avoid duplicate fires.
        assert_eq!(
            tracker.on_timer(1_195, 120, 3),
            HoverAction::Prefetch(POINT { x: 120, y: 100 })
        );
        tracker.mark_prefetched(POINT { x: 120, y: 100 });
        assert!(tracker.on_move(POINT { x: 125, y: 100 }, 1_300, 3));
        assert_eq!(
            tracker.on_timer(1_450, 120, 3),
            HoverAction::Prefetch(POINT { x: 125, y: 100 })
        );
        tracker.mark_prefetched(POINT { x: 125, y: 100 });
        assert!(!tracker.on_move(POINT { x: 126, y: 100 }, 1_500, 3));
        assert!(!tracker.reset());
        assert!(tracker.last_pt.is_none());
    }
}
