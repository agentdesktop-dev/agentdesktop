//! Windows registry reconcile target.
//!
//! Claude Desktop on Windows reads its managed configuration from flat
//! `REG_SZ` values directly under `HKLM\SOFTWARE\Policies\Claude`, never from
//! a file, so the reconciler plans registry value changes there. The planning
//! types are plain data and compile everywhere (the pure planning tests run on
//! every platform); only the functions that touch the live registry are
//! Windows-only.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// A registry key under a fixed root. The root is a type of its own so tests
/// can run the writer against a hive that needs no elevation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegistryKey {
    root: RegistryRoot,
    path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistryRoot {
    /// `HKEY_LOCAL_MACHINE`: machine policy, written only by the system daemon.
    LocalMachine,
    /// `HKEY_CURRENT_USER`: writable without elevation, for the Windows tests.
    #[cfg(test)]
    CurrentUser,
}

impl RegistryKey {
    pub(crate) fn local_machine(path: impl Into<String>) -> Self {
        Self {
            root: RegistryRoot::LocalMachine,
            path: path.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn current_user(path: impl Into<String>) -> Self {
        Self {
            root: RegistryRoot::CurrentUser,
            path: path.into(),
        }
    }

    /// The key as plans and logs display it, e.g. `HKLM\SOFTWARE\Policies\Claude`.
    pub(crate) fn display_path(&self) -> PathBuf {
        let root = match self.root {
            RegistryRoot::LocalMachine => "HKLM",
            #[cfg(test)]
            RegistryRoot::CurrentUser => "HKCU",
        };
        PathBuf::from(format!(r"{root}\{}", self.path))
    }
}

/// The values observed under a key: the `REG_SZ` data by value name, or
/// `None` for a value of any other type, which agentdesktop never writes and
/// never matches.
pub(crate) type RegistryValues = BTreeMap<String, Option<String>>;

/// One key's planned changes: each entry writes a `REG_SZ` value (`Some`) or
/// deletes it (`None`). `observed` is the whole key's state when the plan was
/// made, so the apply can refuse when the key changed in between, like a
/// planned file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RegistryKeyPlan {
    pub(crate) key: RegistryKey,
    pub(crate) observed: RegistryValues,
    pub(crate) changes: Vec<(String, Option<String>)>,
    /// Delete the key itself when nothing remains in it after the changes.
    pub(crate) delete_key_when_empty: bool,
}

#[cfg(windows)]
pub(crate) use self::windows::{apply, read_values};
#[cfg(all(windows, test))]
pub(crate) use self::windows::{delete_tree, key_exists};

/// A unique key under `HKCU`, writable without elevation and deleted on
/// drop: what every test uses in place of the live machine policy key,
/// which no test may read or write.
#[cfg(all(windows, test))]
pub(crate) struct TestPolicyKey(pub(crate) RegistryKey);

#[cfg(all(windows, test))]
impl TestPolicyKey {
    pub(crate) fn new() -> Self {
        Self(RegistryKey::current_user(format!(
            r"Software\agentdesktop-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        )))
    }
}

#[cfg(all(windows, test))]
impl Drop for TestPolicyKey {
    fn drop(&mut self) {
        delete_tree(&self.0);
    }
}

#[cfg(windows)]
mod windows {
    use super::{RegistryKey, RegistryKeyPlan, RegistryRoot, RegistryValues};
    use anyhow::Context;
    use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegEnumValueW, RegOpenKeyExW,
        RegQueryInfoKeyW, RegSetValueExW,
    };
    #[cfg(test)]
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};

    impl RegistryRoot {
        fn handle(self) -> HKEY {
            match self {
                RegistryRoot::LocalMachine => HKEY_LOCAL_MACHINE,
                #[cfg(test)]
                RegistryRoot::CurrentUser => HKEY_CURRENT_USER,
            }
        }
    }

    /// An open key handle, closed on drop.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by RegOpenKeyExW or RegCreateKeyExW.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn registry_error(status: u32) -> io::Error {
        io::Error::from_raw_os_error(status as i32)
    }

    /// Every value under `key`, or an empty map when the key does not exist.
    pub(crate) fn read_values(key: &RegistryKey) -> io::Result<RegistryValues> {
        let path = wide(&key.path);
        let mut handle = std::ptr::null_mut();
        // SAFETY: path is null-terminated and handle receives the opened key.
        let status =
            unsafe { RegOpenKeyExW(key.root.handle(), path.as_ptr(), 0, KEY_READ, &mut handle) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(RegistryValues::new());
        }
        if status != ERROR_SUCCESS {
            return Err(registry_error(status));
        }
        let key = Key(handle);
        let mut values = RegistryValues::new();
        let mut data = vec![0u8; 256];
        for index in 0.. {
            let Some((name, value)) = read_value(&key, index, &mut data)? else {
                break;
            };
            values.insert(name, value);
        }
        Ok(values)
    }

    /// The value at `index` under `key`, or `None` past the last one.
    /// `data` grows as needed and is reused across values.
    fn read_value(
        key: &Key,
        index: u32,
        data: &mut Vec<u8>,
    ) -> io::Result<Option<(String, Option<String>)>> {
        // One more than the registry's maximum value-name length, in UTF-16
        // units, so every name fits with its terminator.
        let mut name = [0u16; 16384];
        loop {
            let mut name_len = name.len() as u32;
            let mut value_type = 0u32;
            let mut data_len = data.len() as u32;
            // SAFETY: all buffers live across the call and their sizes are passed.
            let status = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null(),
                    &mut value_type,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            match status {
                ERROR_SUCCESS => {
                    let name = String::from_utf16_lossy(&name[..name_len as usize]);
                    let value =
                        (value_type == REG_SZ).then(|| decode_sz(&data[..data_len as usize]));
                    return Ok(Some((name, value)));
                }
                // The data buffer was too small; data_len has the needed size.
                ERROR_MORE_DATA => data.resize(data_len as usize, 0),
                ERROR_NO_MORE_ITEMS => return Ok(None),
                status => return Err(registry_error(status)),
            }
        }
    }

    /// Decodes `REG_SZ` bytes: UTF-16, usually with a trailing terminator.
    fn decode_sz(data: &[u8]) -> String {
        let mut units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        while units.last() == Some(&0) {
            units.pop();
        }
        String::from_utf16_lossy(&units)
    }

    /// Applies one key's planned changes. The caller has already verified
    /// that the key still matches `plan.observed`.
    pub(crate) fn apply(plan: &RegistryKeyPlan) -> anyhow::Result<()> {
        let display = plan.key.display_path();
        let path = wide(&plan.key.path);
        let mut handle = std::ptr::null_mut();
        let mut disposition = 0u32;
        // SAFETY: path is null-terminated and handle receives the created key.
        let status = unsafe {
            RegCreateKeyExW(
                plan.key.root.handle(),
                path.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_SET_VALUE,
                std::ptr::null(),
                &mut handle,
                &mut disposition,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(registry_error(status))
                .with_context(|| format!("open {}", display.display()));
        }
        let key = Key(handle);
        for (name, value) in &plan.changes {
            let value_name = wide(name);
            match value {
                Some(data) => {
                    let data = wide(data);
                    // SAFETY: the value name and data are null-terminated and
                    // the data length is its byte count.
                    let status = unsafe {
                        RegSetValueExW(
                            key.0,
                            value_name.as_ptr(),
                            0,
                            REG_SZ,
                            data.as_ptr().cast(),
                            (data.len() * 2) as u32,
                        )
                    };
                    if status != ERROR_SUCCESS {
                        return Err(registry_error(status))
                            .with_context(|| format!("write {}\\{name}", display.display()));
                    }
                }
                None => {
                    // SAFETY: the value name is null-terminated.
                    let status = unsafe { RegDeleteValueW(key.0, value_name.as_ptr()) };
                    if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
                        return Err(registry_error(status))
                            .with_context(|| format!("remove {}\\{name}", display.display()));
                    }
                }
            }
        }
        if plan.delete_key_when_empty && key_is_empty(&key)? {
            drop(key);
            // SAFETY: path is null-terminated; the handle is already closed.
            let status = unsafe { RegDeleteKeyW(plan.key.root.handle(), path.as_ptr()) };
            if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
                return Err(registry_error(status))
                    .with_context(|| format!("remove {}", display.display()));
            }
        }
        Ok(())
    }

    /// Whether the key has no values and no subkeys left.
    fn key_is_empty(key: &Key) -> io::Result<bool> {
        let mut subkeys = 0u32;
        let mut values = 0u32;
        // SAFETY: only the subkey and value counters are requested.
        let status = unsafe {
            RegQueryInfoKeyW(
                key.0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut subkeys,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut values,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(registry_error(status));
        }
        Ok(subkeys == 0 && values == 0)
    }

    /// Whether the key exists, for tests.
    #[cfg(test)]
    pub(crate) fn key_exists(key: &RegistryKey) -> io::Result<bool> {
        let path = wide(&key.path);
        let mut handle = std::ptr::null_mut();
        // SAFETY: path is null-terminated and handle receives the opened key.
        let status =
            unsafe { RegOpenKeyExW(key.root.handle(), path.as_ptr(), 0, KEY_READ, &mut handle) };
        match status {
            ERROR_SUCCESS => {
                drop(Key(handle));
                Ok(true)
            }
            ERROR_FILE_NOT_FOUND => Ok(false),
            status => Err(registry_error(status)),
        }
    }

    /// Deletes the key and everything under it, for test cleanup.
    #[cfg(test)]
    pub(crate) fn delete_tree(key: &RegistryKey) {
        let path = wide(&key.path);
        // SAFETY: path is null-terminated.
        unsafe {
            RegDeleteTreeW(key.root.handle(), path.as_ptr());
        }
    }
}
