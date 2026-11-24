//! Managed-settings posture for Claude Code containment.

use agentdesktop_core::model::ManagedSettingsSource;
use serde_json::Value;
use std::io;

/// Sources for managed settings (file tier and HKLM).
#[derive(Debug, Default)]
pub struct ManagedSources {
    pub hklm: Option<io::Result<String>>,
    pub file: Option<io::Result<String>>,
    pub drop_ins: Vec<(String, io::Result<String>)>,
}

/// Managed-settings posture snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedSettingsPosture {
    pub source: Option<ManagedSettingsSource>,
    pub hooks_locked: bool,
    pub mcp_locked: bool,
    pub error: Option<String>,
}

/// Models Windows: HKLM and the managed-settings file tier, Claude Code 2.1.273+.
/// Never reports a lock Claude Code does not apply; may report a lock as off where Claude Code fails closed.
pub fn managed_settings_state(sources: &ManagedSources) -> ManagedSettingsPosture {
    // Check HKLM
    let hklm_doc = match &sources.hklm {
        Some(Ok(content)) => {
            if content.trim().is_empty() {
                return ManagedSettingsPosture {
                    source: None,
                    hooks_locked: false,
                    mcp_locked: false,
                    error: Some("HKLM: broken (empty)".to_owned()),
                };
            }
            match serde_json::from_str::<Value>(content) {
                Ok(value) if value.is_object() => Some(value),
                _ => {
                    return ManagedSettingsPosture {
                        source: None,
                        hooks_locked: false,
                        mcp_locked: false,
                        error: Some("HKLM: broken (invalid JSON)".to_owned()),
                    };
                }
            }
        }
        Some(Err(err)) => {
            return ManagedSettingsPosture {
                source: None,
                hooks_locked: false,
                mcp_locked: false,
                error: Some(format!("HKLM: cannot read: {}", err)),
            };
        }
        None => None,
    };

    // Check file tier
    let file_doc = match &sources.file {
        Some(Ok(content)) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                Some(serde_json::json!({}))
            } else {
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(value) if value.is_object() => Some(value),
                    _ => {
                        return ManagedSettingsPosture {
                            source: None,
                            hooks_locked: false,
                            mcp_locked: false,
                            error: Some("file: broken (invalid JSON)".to_owned()),
                        };
                    }
                }
            }
        }
        Some(Err(err)) => {
            return ManagedSettingsPosture {
                source: None,
                hooks_locked: false,
                mcp_locked: false,
                error: Some(format!("file: cannot read: {}", err)),
            };
        }
        None => None,
    };

    // Merge drop-ins into file tier
    let mut file_merged = file_doc.unwrap_or_else(|| serde_json::json!({}));
    for (name, result) in &sources.drop_ins {
        match result {
            Ok(content) => {
                let trimmed = content.trim();
                let drop_in_value = if trimmed.is_empty() {
                    serde_json::json!({})
                } else {
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(value) if value.is_object() => value,
                        _ => {
                            return ManagedSettingsPosture {
                                source: None,
                                hooks_locked: false,
                                mcp_locked: false,
                                error: Some(format!("{name}: broken (invalid JSON)")),
                            };
                        }
                    }
                };
                if let (Some(base), Some(drop_in)) =
                    (file_merged.as_object_mut(), drop_in_value.as_object())
                {
                    for (key, value) in drop_in {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
            Err(err) => {
                return ManagedSettingsPosture {
                    source: None,
                    hooks_locked: false,
                    mcp_locked: false,
                    error: Some(format!("{name}: cannot read: {}", err)),
                };
            }
        }
    }

    // Determine if we're in merge mode
    let merge_mode = hklm_doc
        .as_ref()
        .and_then(|doc| {
            doc.get("managedSourcesBehavior")
                .and_then(Value::as_str)
                .map(|s| s == "merge")
        })
        .unwrap_or(false);

    // Helper to check if a document has policy keys
    let has_policy_key = |doc: &Value| -> bool {
        doc.as_object().is_some_and(|obj| {
            obj.iter().any(|(key, value)| {
                key != "managedSourcesBehavior"
                    && key != "wslInheritsWindowsSettings"
                    && !value.is_null()
            })
        })
    };

    // Determine the selected source
    let (selected_source, selected_doc) = if let Some(ref hklm) = hklm_doc {
        if has_policy_key(hklm) {
            (Some(ManagedSettingsSource::Hklm), hklm)
        } else if has_policy_key(&file_merged) {
            (Some(ManagedSettingsSource::File), &file_merged)
        } else {
            (None, &file_merged)
        }
    } else if has_policy_key(&file_merged) {
        (Some(ManagedSettingsSource::File), &file_merged)
    } else {
        (None, &file_merged)
    };

    // Check if policyHelper is present and not null
    if selected_doc
        .get("policyHelper")
        .is_some_and(|v| !v.is_null())
    {
        return ManagedSettingsPosture {
            source: selected_source,
            hooks_locked: false,
            mcp_locked: false,
            error: None,
        };
    }

    // Determine hooks_locked
    let hooks_locked = if merge_mode {
        // In merge mode, check both sources
        let hklm_hooks = hklm_doc
            .as_ref()
            .and_then(|doc| doc.get("allowManagedHooksOnly"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let file_hooks = file_merged
            .get("allowManagedHooksOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        hklm_hooks || file_hooks
    } else {
        selected_doc
            .get("allowManagedHooksOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };

    // Determine mcp_locked: flag must be true AND allowedMcpServers must be present (array, including empty)
    // Cross-source: check both sources for flag and list (merge mode or first-wins)
    let mcp_locked = {
        let hklm_flag = hklm_doc
            .as_ref()
            .and_then(|doc| doc.get("allowManagedMcpServersOnly"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let file_flag = file_merged
            .get("allowManagedMcpServersOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let has_flag = hklm_flag || file_flag;

        let hklm_list = hklm_doc
            .as_ref()
            .and_then(|doc| doc.get("allowedMcpServers"))
            .is_some_and(|v| v.is_array());
        let file_list = file_merged
            .get("allowedMcpServers")
            .is_some_and(|v| v.is_array());
        let has_list = hklm_list || file_list;

        has_flag && has_list
    };

    ManagedSettingsPosture {
        source: selected_source,
        hooks_locked,
        mcp_locked,
        error: None,
    }
}

fn read_managed_sources(
    root: Option<&std::path::Path>,
    hklm: impl FnOnce() -> Option<io::Result<String>>,
) -> ManagedSources {
    use std::fs;

    let hklm_value = hklm();

    let (file, drop_ins) = if let Some(root) = root {
        // Check if managed-settings.d directory is readable
        let drop_in_dir = root.join("managed-settings.d");
        if let Err(err) = fs::read_dir(&drop_in_dir)
            && err.kind() != io::ErrorKind::NotFound
        {
            // Directory exists but is unreadable - report as a drop-in error
            return ManagedSources {
                hklm: hklm_value,
                file: None,
                drop_ins: vec![("managed-settings.d".to_owned(), Err(err))],
            };
        }

        // Use discovery function to get file paths
        let paths = super::discovery::managed_settings_files(root);

        let mut base_file = None;
        let mut drop_ins = Vec::new();

        for (index, path) in paths.iter().enumerate() {
            if index == 0 {
                // First entry is always the base file
                base_file = match fs::read_to_string(path) {
                    Ok(content) => Some(Ok(content)),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                    Err(err) => Some(Err(err)),
                };
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let result = fs::read_to_string(path);
                drop_ins.push((name.to_owned(), result));
            }
        }

        (base_file, drop_ins)
    } else {
        (None, Vec::new())
    };

    ManagedSources {
        hklm: hklm_value,
        file,
        drop_ins,
    }
}

#[cfg(windows)]
fn read_hklm_settings(
    hive: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
) -> Option<io::Result<String>> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA};
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_READ, REG_EXPAND_SZ, REG_SZ, RRF_NOEXPAND, RRF_RT_ANY, RegCloseKey, RegGetValueW,
        RegOpenKeyExW,
    };

    let subkey_wide: Vec<u16> = OsStr::new(subkey)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = OsStr::new("Settings")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut key: HKEY = std::ptr::null_mut();
    let open_result = unsafe { RegOpenKeyExW(hive, subkey_wide.as_ptr(), 0, KEY_READ, &mut key) };

    if open_result != 0 {
        return if open_result == ERROR_FILE_NOT_FOUND {
            None
        } else {
            Some(Err(io::Error::from_raw_os_error(open_result as i32)))
        };
    }

    // Query the size and type first
    let mut value_type: u32 = 0;
    let mut required_size: u32 = 0;

    let query_result = unsafe {
        RegGetValueW(
            key,
            std::ptr::null(),
            value_name.as_ptr(),
            RRF_RT_ANY | RRF_NOEXPAND,
            &mut value_type,
            std::ptr::null_mut(),
            &mut required_size,
        )
    };

    if query_result != 0 {
        unsafe { RegCloseKey(key) };
        return if query_result == ERROR_FILE_NOT_FOUND {
            None
        } else {
            Some(Err(io::Error::from_raw_os_error(query_result as i32)))
        };
    }

    // Check if the type is REG_SZ or REG_EXPAND_SZ
    if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
        unsafe { RegCloseKey(key) };
        return Some(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HKLM value has wrong type: {}", value_type),
        )));
    }

    // Allocate buffer based on reported size, with room for possible growth
    let buffer_len = (required_size as usize / 2) + 1;
    let mut buffer = vec![0u16; buffer_len];
    let mut buffer_size = (buffer.len() * 2) as u32;

    let read_result = unsafe {
        RegGetValueW(
            key,
            std::ptr::null(),
            value_name.as_ptr(),
            RRF_RT_ANY | RRF_NOEXPAND,
            &mut value_type,
            buffer.as_mut_ptr() as *mut _,
            &mut buffer_size,
        )
    };

    unsafe { RegCloseKey(key) };

    if read_result != 0 {
        return if read_result == ERROR_MORE_DATA {
            // Retry with a larger buffer
            let mut large_buffer = vec![0u16; buffer_len * 2];
            let mut large_size = (large_buffer.len() * 2) as u32;

            let mut retry_key: HKEY = std::ptr::null_mut();
            let retry_open =
                unsafe { RegOpenKeyExW(hive, subkey_wide.as_ptr(), 0, KEY_READ, &mut retry_key) };

            if retry_open != 0 {
                return Some(Err(io::Error::from_raw_os_error(retry_open as i32)));
            }

            let retry_result = unsafe {
                RegGetValueW(
                    retry_key,
                    std::ptr::null(),
                    value_name.as_ptr(),
                    RRF_RT_ANY | RRF_NOEXPAND,
                    &mut value_type,
                    large_buffer.as_mut_ptr() as *mut _,
                    &mut large_size,
                )
            };

            unsafe { RegCloseKey(retry_key) };

            if retry_result != 0 {
                Some(Err(io::Error::from_raw_os_error(retry_result as i32)))
            } else {
                let len = (large_size / 2) as usize;
                if len > 0 && large_buffer[len - 1] == 0 {
                    large_buffer.truncate(len - 1);
                } else {
                    large_buffer.truncate(len);
                }
                Some(Ok(String::from_utf16_lossy(&large_buffer)))
            }
        } else {
            Some(Err(io::Error::from_raw_os_error(read_result as i32)))
        };
    }

    // Trim null terminator
    let len = (buffer_size / 2) as usize;
    if len > 0 && buffer[len - 1] == 0 {
        buffer.truncate(len - 1);
    } else {
        buffer.truncate(len);
    }

    Some(Ok(String::from_utf16_lossy(&buffer)))
}

/// Read managed sources from the real file system and registry.
pub fn managed_sources_now() -> ManagedSources {
    read_managed_sources(super::discovery::managed_root().as_deref(), || {
        #[cfg(windows)]
        {
            read_hklm_settings(
                windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
                r"SOFTWARE\Policies\ClaudeCode",
            )
        }
        #[cfg(not(windows))]
        {
            None
        }
    })
}

/// Read and compute the current managed-settings posture.
pub fn managed_posture_now() -> ManagedSettingsPosture {
    managed_settings_state(&managed_sources_now())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure function tests for managed_settings_state (cases 1-15)

    #[test]
    fn test_managed_settings_state_none() {
        let sources = ManagedSources::default();
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, None);
        assert!(!posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_file_only_with_both_locks() {
        let sources = ManagedSources {
            hklm: None,
            file: Some(Ok(r#"{"allowManagedHooksOnly":true,"allowManagedMcpServersOnly":true,"allowedMcpServers":[]}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File));
        assert!(posture.hooks_locked);
        assert!(posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_hklm_only_with_both_locks() {
        let sources = ManagedSources {
            hklm: Some(Ok(r#"{"allowManagedHooksOnly":true,"allowManagedMcpServersOnly":true,"allowedMcpServers":[]}"#.to_owned())),
            file: None,
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::Hklm));
        assert!(posture.hooks_locked);
        assert!(posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_both_present_first_wins_selects_hklm() {
        let sources = ManagedSources {
            hklm: Some(Ok(r#"{"model":"claude-3-5-sonnet"}"#.to_owned())),
            file: Some(Ok(r#"{"allowManagedHooksOnly":true}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::Hklm));
        assert!(!posture.hooks_locked); // HKLM selected, doesn't have hooks lock
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_file_with_locks_hklm_without_locks() {
        let sources = ManagedSources {
            hklm: Some(Ok(r#"{"model":"claude-3-5-sonnet"}"#.to_owned())),
            file: Some(Ok(r#"{"allowManagedHooksOnly":true,"allowManagedMcpServersOnly":true,"allowedMcpServers":[]}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::Hklm));
        assert!(!posture.hooks_locked); // from selected source (HKLM), which doesn't have the lock
        assert!(posture.mcp_locked); // cross-source: flag and list from file
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_merge_mode_with_locks_in_both() {
        let sources = ManagedSources {
            hklm: Some(Ok(
                r#"{"managedSourcesBehavior":"merge","allowManagedHooksOnly":true}"#.to_owned(),
            )),
            file: Some(Ok(
                r#"{"allowManagedMcpServersOnly":true,"allowedMcpServers":[]}"#.to_owned(),
            )),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::Hklm));
        assert!(posture.hooks_locked); // strictest under merge
        assert!(posture.mcp_locked); // cross-source
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_one_lock_only() {
        let sources = ManagedSources {
            hklm: None,
            file: Some(Ok(r#"{"allowManagedMcpServersOnly":true}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File));
        assert!(!posture.hooks_locked);
        assert!(!posture.mcp_locked); // flag present but no allowedMcpServers list
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_policy_helper_present() {
        let sources = ManagedSources {
            hklm: None,
            file: Some(Ok(r#"{"policyHelper":"/usr/bin/helper","allowManagedHooksOnly":true,"allowManagedMcpServersOnly":true,"allowedMcpServers":[]}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File));
        assert!(!posture.hooks_locked); // policyHelper present, so both locks false
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_allowed_mcp_servers_empty_array() {
        let sources = ManagedSources {
            hklm: None,
            file: Some(Ok(
                r#"{"allowManagedMcpServersOnly":true,"allowedMcpServers":[]}"#.to_owned(),
            )),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File));
        assert!(!posture.hooks_locked);
        assert!(posture.mcp_locked); // empty array still counts as having the list
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_hklm_control_keys_only() {
        let sources = ManagedSources {
            hklm: Some(Ok(r#"{"managedSourcesBehavior":"merge"}"#.to_owned())),
            file: Some(Ok(r#"{"allowManagedHooksOnly":true}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File)); // HKLM has no policy keys
        assert!(posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_null_policy_key_ignored() {
        let sources = ManagedSources {
            hklm: Some(Ok(r#"{"model":null}"#.to_owned())),
            file: Some(Ok(r#"{"allowManagedHooksOnly":true}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File)); // HKLM model:null is not a policy key
        assert!(posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_broken_hklm() {
        let sources = ManagedSources {
            hklm: Some(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access denied",
            ))),
            file: Some(Ok(r#"{"allowManagedHooksOnly":true}"#.to_owned())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, None); // broken admin document
        assert!(!posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert!(posture.error.is_some()); // error names the source
        assert!(
            posture.error.as_ref().unwrap().contains("HKLM")
                || posture.error.as_ref().unwrap().contains("hklm")
        );
    }

    #[test]
    fn test_managed_settings_state_empty_file() {
        let sources = ManagedSources {
            hklm: None,
            file: Some(Ok("  \n  ".to_owned())), // whitespace-only
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, None); // empty file = {}, no policy keys
        assert!(!posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_first_wins_with_flag_in_hklm_and_list_in_file() {
        let sources = ManagedSources {
            hklm: Some(Ok(r#"{"allowManagedMcpServersOnly":true}"#.to_owned())),
            file: Some(Ok(
                r#"{"allowedMcpServers":[{"serverUrl":"https://mcp.example.com/*"}]}"#.to_owned(),
            )),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::Hklm));
        assert!(!posture.hooks_locked);
        assert!(posture.mcp_locked); // cross-source: flag from HKLM, list from file
        assert_eq!(posture.error, None);
    }

    #[test]
    fn test_managed_settings_state_later_dropin_setting_overrides_earlier() {
        let sources = ManagedSources {
            hklm: None,
            file: None,
            drop_ins: vec![
                (
                    "50-first.json".to_owned(),
                    Ok(r#"{"allowManagedHooksOnly":true}"#.to_owned()),
                ),
                (
                    "60-second.json".to_owned(),
                    Ok(r#"{"allowManagedHooksOnly":false}"#.to_owned()),
                ),
            ],
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File)); // drop-ins are part of file tier
        assert!(!posture.hooks_locked); // later drop-in (60-second) overrides earlier (50-first)
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, None);
    }

    // Reader tests (R1, R2)

    #[test]
    fn test_read_managed_sources_with_dropins() {
        use std::fs;
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        // Create base file
        fs::write(root.join("managed-settings.json"), r#"{"model":"base"}"#).unwrap();

        // Create drop-ins directory
        let dropins_dir = root.join("managed-settings.d");
        fs::create_dir(&dropins_dir).unwrap();

        // Create two .json drop-ins
        fs::write(dropins_dir.join("10-first.json"), r#"{"key1":"val1"}"#).unwrap();
        fs::write(dropins_dir.join("20-second.json"), r#"{"key2":"val2"}"#).unwrap();

        // Create a hidden .json file (should be skipped)
        fs::write(
            dropins_dir.join(".50-agentdesktop.json"),
            r#"{"hidden":"yes"}"#,
        )
        .unwrap();

        // Create a .txt file (should be skipped)
        fs::write(dropins_dir.join("30-text.txt"), "not json").unwrap();

        let sources = read_managed_sources(Some(root), || None);

        assert!(sources.hklm.is_none());
        assert!(sources.file.is_some());
        assert_eq!(
            sources.file.as_ref().unwrap().as_ref().unwrap(),
            r#"{"model":"base"}"#
        );
        assert_eq!(sources.drop_ins.len(), 2);
        assert_eq!(sources.drop_ins[0].0, "10-first.json");
        assert_eq!(
            sources.drop_ins[0].1.as_ref().unwrap(),
            r#"{"key1":"val1"}"#
        );
        assert_eq!(sources.drop_ins[1].0, "20-second.json");
        assert_eq!(
            sources.drop_ins[1].1.as_ref().unwrap(),
            r#"{"key2":"val2"}"#
        );
    }

    #[test]
    fn test_read_managed_sources_missing_root() {
        use std::path::Path;
        let nonexistent = Path::new("/nonexistent/path/to/managed/settings");

        let sources = read_managed_sources(Some(nonexistent), || None);

        assert!(sources.hklm.is_none());
        assert!(sources.file.is_none());
        assert!(sources.drop_ins.is_empty());
    }

    // Windows-only tests (W1, W2)

    #[cfg(windows)]
    #[test]
    fn test_read_hklm_settings_reg_sz_and_reg_expand_sz() {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::Registry::*;

        // Generate random test key name
        let random_id: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let test_key_path = format!(r"Software\agentdesktop-test\{}", random_id);

        // Create temp test key
        let key_path_wide: Vec<u16> = OsStr::new(&test_key_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut hkey: HKEY = std::ptr::null_mut();
        let result = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key_path_wide.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_ALL_ACCESS,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "Failed to create test registry key");

        // Cleanup guard
        struct KeyGuard(HKEY, String);
        impl Drop for KeyGuard {
            fn drop(&mut self) {
                use std::ffi::OsStr;
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::System::Registry::*;
                unsafe {
                    RegCloseKey(self.0);
                    // Delete the test-specific key
                    let path_wide: Vec<u16> = OsStr::new(&self.1)
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();
                    RegDeleteKeyW(HKEY_CURRENT_USER, path_wide.as_ptr());

                    // Delete the parent agentdesktop-test key if it's empty
                    let parent_path = r"Software\agentdesktop-test";
                    let parent_wide: Vec<u16> = OsStr::new(parent_path)
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();
                    RegDeleteKeyW(HKEY_CURRENT_USER, parent_wide.as_ptr());
                }
            }
        }
        let _guard = KeyGuard(hkey, test_key_path.clone());

        // Test REG_SZ
        let sz_value = r#"{"test":"value"}"#;
        let sz_wide: Vec<u16> = OsStr::new(sz_value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let settings_wide: Vec<u16> = OsStr::new("Settings")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            RegSetValueExW(
                hkey,
                settings_wide.as_ptr(),
                0,
                REG_SZ,
                sz_wide.as_ptr() as *const u8,
                (sz_wide.len() * 2) as u32,
            )
        };

        let result_sz = read_hklm_settings(HKEY_CURRENT_USER, &test_key_path);
        assert!(result_sz.is_some());
        assert_eq!(result_sz.unwrap().unwrap(), sz_value);

        // Test REG_EXPAND_SZ
        let expand_value = r#"%TEMP%\test"#;
        let expand_wide: Vec<u16> = OsStr::new(expand_value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            RegSetValueExW(
                hkey,
                settings_wide.as_ptr(),
                0,
                REG_EXPAND_SZ,
                expand_wide.as_ptr() as *const u8,
                (expand_wide.len() * 2) as u32,
            )
        };

        let result_expand = read_hklm_settings(HKEY_CURRENT_USER, &test_key_path);
        assert!(result_expand.is_some());
        // Should be read back unexpanded
        assert_eq!(result_expand.unwrap().unwrap(), expand_value);
    }

    #[cfg(windows)]
    #[test]
    fn test_read_hklm_settings_reg_dword_and_absent() {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::Registry::*;

        // Generate random test key name
        let random_id: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let test_key_path = format!(r"Software\agentdesktop-test\{}", random_id);

        // Create temp test key
        let key_path_wide: Vec<u16> = OsStr::new(&test_key_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut hkey: HKEY = std::ptr::null_mut();
        let result = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key_path_wide.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_ALL_ACCESS,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "Failed to create test registry key");

        // Cleanup guard
        struct KeyGuard(HKEY, String);
        impl Drop for KeyGuard {
            fn drop(&mut self) {
                use std::ffi::OsStr;
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::System::Registry::*;
                unsafe {
                    RegCloseKey(self.0);
                    // Delete the test-specific key
                    let path_wide: Vec<u16> = OsStr::new(&self.1)
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();
                    RegDeleteKeyW(HKEY_CURRENT_USER, path_wide.as_ptr());

                    // Delete the parent agentdesktop-test key if it's empty
                    let parent_path = r"Software\agentdesktop-test";
                    let parent_wide: Vec<u16> = OsStr::new(parent_path)
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();
                    RegDeleteKeyW(HKEY_CURRENT_USER, parent_wide.as_ptr());
                }
            }
        }
        let _guard = KeyGuard(hkey, test_key_path.clone());

        // Test REG_DWORD (should return Err(InvalidData))
        let dword_value: u32 = 1;
        let settings_wide: Vec<u16> = OsStr::new("Settings")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            RegSetValueExW(
                hkey,
                settings_wide.as_ptr(),
                0,
                REG_DWORD,
                &dword_value as *const u32 as *const u8,
                4,
            )
        };

        let result_dword = read_hklm_settings(HKEY_CURRENT_USER, &test_key_path);
        assert!(result_dword.is_some());
        let err = result_dword.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Test absent value (should return None)
        unsafe {
            RegDeleteValueW(hkey, settings_wide.as_ptr());
        };

        let result_absent = read_hklm_settings(HKEY_CURRENT_USER, &test_key_path);
        assert!(result_absent.is_none());
    }

    // Writer tests for error handling via managed_settings_state (run on all platforms)

    #[test]
    fn hklm_permission_denied_gives_error_with_cause() {
        let sources = ManagedSources {
            hklm: Some(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access denied",
            ))),
            file: None,
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, None);
        assert!(!posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert!(posture.error.is_some());
        let err_msg = posture.error.unwrap();
        assert!(
            err_msg.starts_with("HKLM: cannot read: "),
            "error should start with 'HKLM: cannot read: ', got: {}",
            err_msg
        );
        assert!(
            err_msg.contains("access denied"),
            "error should contain the cause, got: {}",
            err_msg
        );
    }

    #[test]
    fn hklm_invalid_data_gives_error_with_cause() {
        let sources = ManagedSources {
            hklm: Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HKLM value has wrong type: 4",
            ))),
            file: None,
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, None);
        assert!(!posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert!(posture.error.is_some());
        let err_msg = posture.error.unwrap();
        assert!(
            err_msg.starts_with("HKLM: cannot read: "),
            "error should start with 'HKLM: cannot read: ', got: {}",
            err_msg
        );
        assert!(
            err_msg.contains("wrong type"),
            "error should contain the cause, got: {}",
            err_msg
        );
    }

    #[test]
    fn hklm_not_found_is_treated_as_absent() {
        let sources = ManagedSources {
            hklm: None,
            file: Some(Ok(r#"{"model":"x"}"#.to_string())),
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::File));
        assert!(posture.error.is_none());
    }

    #[test]
    fn hklm_empty_string_gives_broken_error() {
        let sources = ManagedSources {
            hklm: Some(Ok("".to_string())),
            file: None,
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, None);
        assert!(!posture.hooks_locked);
        assert!(!posture.mcp_locked);
        assert_eq!(posture.error, Some("HKLM: broken (empty)".to_string()));
    }

    #[test]
    fn large_hklm_value_processed_successfully() {
        let large_json = format!(r#"{{"key":"{}"}}"#, "x".repeat(50000));
        let sources = ManagedSources {
            hklm: Some(Ok(large_json)),
            file: None,
            drop_ins: Vec::new(),
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.source, Some(ManagedSettingsSource::Hklm));
        assert!(posture.error.is_none());
    }
}
