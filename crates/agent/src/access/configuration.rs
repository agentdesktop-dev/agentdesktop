use std::path::Path;

use agentdesktop_core::model::{
    AccessCapability, AccessCategory, AccessCoverage, AccessCoverageStatus, AccessDecision,
    AccessEnforcement, AccessOperation, AccessSourceKind,
};

use super::{CollectedAccess, claude_code, claude_desktop, codex, opencode, source, vscode};

pub(super) fn inspect(kind: &str, home: &Path) -> CollectedAccess {
    match kind {
        "vscode" => vscode::inspect_configuration(home),
        "claude-code" => claude_code::inspect_configuration(home),
        "claude-desktop" => claude_desktop::inspect_configuration(home),
        "codex" => codex::inspect_configuration(home),
        "opencode" => opencode::inspect_configuration(home),
        _ => unsupported(kind, "No configuration access adapter is available"),
    }
}

fn unsupported(kind: &str, detail: &str) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::Configuration,
        status: AccessCoverageStatus::Unsupported,
        detail: format!("{kind}: {detail}"),
    });
    collected
}

#[expect(clippy::too_many_arguments)]
pub(super) fn capability(
    category: AccessCategory,
    resource: &str,
    operations: Vec<AccessOperation>,
    decision: AccessDecision,
    enforcement: AccessEnforcement,
    path: &Path,
    workspace: Option<&Path>,
    detail: &str,
) -> AccessCapability {
    AccessCapability {
        category,
        resource: resource.to_owned(),
        operations,
        decision,
        enforcement,
        workspace: workspace.map(Path::to_path_buf),
        source: source(AccessSourceKind::Configuration, Some(path.to_path_buf())),
        rule: None,
        detail: Some(detail.to_owned()),
    }
}

pub(super) fn default_capability(
    category: AccessCategory,
    resource: &str,
    operations: Vec<AccessOperation>,
    decision: AccessDecision,
    enforcement: AccessEnforcement,
    detail: &str,
) -> AccessCapability {
    AccessCapability {
        category,
        resource: resource.to_owned(),
        operations,
        decision,
        enforcement,
        workspace: None,
        source: source(AccessSourceKind::Default, None),
        rule: None,
        detail: Some(detail.to_owned()),
    }
}
