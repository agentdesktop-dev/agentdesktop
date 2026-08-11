#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub mod windows;
