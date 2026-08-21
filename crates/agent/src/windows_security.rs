use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

use anyhow::Context;
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR,
    },
};

pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> anyhow::Result<Self> {
        let descriptor_text = OsStr::new(sddl)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: descriptor_text is null-terminated and Windows initializes
        // descriptor with LocalAlloc memory on success.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor_text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(std::io::Error::last_os_error())
                .context("create Windows security descriptor");
        }
        Ok(Self(descriptor))
    }

    pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(self.0);
        }
    }
}
