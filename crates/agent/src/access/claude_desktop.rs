use std::{
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{
    AccessCategory, AccessCoverage, AccessCoverageStatus, AccessDecision, AccessEnforcement,
    AccessOperation, AccessSourceKind,
};
use serde_json::Value;

#[cfg(target_os = "macos")]
use super::claude_code;
use super::{CollectedAccess, configuration::capability, history_scan::HistoryAdapter};

#[cfg(target_os = "macos")]
pub(super) fn history_adapter(home: &Path) -> Option<HistoryAdapter> {
    Some(HistoryAdapter {
        kind: "claude-desktop",
        root: home.join("Library/Application Support/Claude/local-agent-mode-sessions"),
        include_file: None,
        workspace_for_file: None,
        inspect_runtime: claude_code::inspect_runtime,
        coverage_limitation: Some("computer-use access may be recorded elsewhere"),
    })
}

#[cfg(not(target_os = "macos"))]
pub(super) fn history_adapter(_home: &Path) -> Option<HistoryAdapter> {
    None
}

pub(super) fn inspect_configuration(home: &Path) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    let path = settings_path(home);
    let Ok(contents) = fs::read(&path) else {
        collected.coverage.push(AccessCoverage {
            source: AccessSourceKind::Configuration,
            status: AccessCoverageStatus::Unavailable,
            detail: "No readable Claude Desktop settings file was found".to_owned(),
        });
        return collected;
    };
    let Ok(document) = serde_json::from_slice::<Value>(&contents) else {
        collected.coverage.push(AccessCoverage {
            source: AccessSourceKind::Configuration,
            status: AccessCoverageStatus::Partial,
            detail: "Claude Desktop settings could not be parsed".to_owned(),
        });
        return collected;
    };
    if document
        .get("preferences")
        .and_then(Value::as_object)
        .and_then(|preferences| preferences.get("coworkWebSearchEnabled"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        collected.capabilities.push(capability(
            AccessCategory::Network,
            "hosted web search",
            vec![AccessOperation::Use],
            AccessDecision::Allow,
            AccessEnforcement::Harness,
            &path,
            None,
            "Claude Desktop Cowork web search enabled",
        ));
    }
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::Configuration,
        status: AccessCoverageStatus::Partial,
        detail: "Inspected Claude Desktop settings; per-session computer and Cowork grants may not be persisted here"
            .to_owned(),
    });
    collected
}

#[cfg(target_os = "macos")]
fn settings_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Claude/claude_desktop_config.json")
}

#[cfg(target_os = "linux")]
fn settings_path(home: &Path) -> PathBuf {
    home.join(".config/Claude/claude_desktop_config.json")
}

#[cfg(windows)]
fn settings_path(home: &Path) -> PathBuf {
    home.join("AppData/Roaming/Claude/claude_desktop_config.json")
}
