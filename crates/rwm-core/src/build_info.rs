pub const BUILD_ID: &str = env!("RWM_BUILD_ID");
pub const ABI_VERSION: u32 = 2;

pub fn versioned_dll_filename() -> String {
    format!("rwm_hook_{BUILD_ID}.dll")
}
