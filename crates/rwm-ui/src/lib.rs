//! Classic-density Windows 11 popup menu for RustWindowsMenu, drawn with Direct2D and DirectWrite
//! and falling back to double-buffered GDI.

pub mod popup;
pub mod renderer;
pub mod theme;

/// Posted to the root popup window whenever hydrated COM shell extension groups are ready.
pub const WM_APP_HYDRATE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 0x42;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
