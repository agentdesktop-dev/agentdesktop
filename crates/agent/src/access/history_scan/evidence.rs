use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use agentdesktop_core::model::{
    AccessCapability, AccessCategory, AccessConfidence, AccessDecision, AccessEnforcement,
    AccessObservation, AccessOperation, AccessSourceKind,
};
use serde_json::Value;

use super::{HistoryAdapter, MAX_HISTORY_OBSERVATIONS, MAX_HISTORY_RUNTIME_CAPABILITIES};
use crate::access::{host_from_url, safe_command_identifier, source};

#[expect(clippy::too_many_arguments)]
pub(super) fn visit_value(
    adapter: &HistoryAdapter,
    value: &Value,
    workspace: Option<&Path>,
    source_path: &Path,
    timestamp: Option<u64>,
    seen: &mut BTreeSet<String>,
    observations: &mut ObservationCollector,
    runtime: &mut RuntimeCollector,
) {
    match value {
        Value::Object(object) => {
            (adapter.inspect_runtime)(object, workspace, runtime);
            if let Some((name, input, id)) = tool_call(object) {
                let unique = id
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{name}:{}", input));
                if seen.insert(unique) {
                    inspect_tool_call(name, input, workspace, source_path, timestamp, observations);
                }
            }
            for child in object.values() {
                visit_value(
                    adapter,
                    child,
                    workspace,
                    source_path,
                    timestamp,
                    seen,
                    observations,
                    runtime,
                );
            }
        }
        Value::Array(values) => {
            for child in values {
                visit_value(
                    adapter,
                    child,
                    workspace,
                    source_path,
                    timestamp,
                    seen,
                    observations,
                    runtime,
                );
            }
        }
        _ => {}
    }
}

pub(in crate::access) fn permission_mode(mode: &str) -> (AccessDecision, AccessEnforcement) {
    match mode {
        "auto" | "autopilot" => (AccessDecision::AutoReview, AccessEnforcement::Harness),
        "bypassPermissions" | "allowAll" | "yolo" => {
            (AccessDecision::Allow, AccessEnforcement::None)
        }
        "default" | "manual" | "on-request" => (AccessDecision::Ask, AccessEnforcement::Harness),
        _ => (AccessDecision::Unknown, AccessEnforcement::Harness),
    }
}

pub(in crate::access) fn runtime_capability(
    category: AccessCategory,
    resource: &str,
    operations: Vec<AccessOperation>,
    decision: AccessDecision,
    enforcement: AccessEnforcement,
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
        source: source(AccessSourceKind::History, None),
        rule: None,
        detail: Some(detail.to_owned()),
    }
}

#[derive(Default)]
pub(in crate::access) struct RuntimeCollector {
    capabilities: BTreeSet<AccessCapability>,
    limited: bool,
}

impl RuntimeCollector {
    pub(in crate::access) fn record(&mut self, capability: AccessCapability) {
        if self.capabilities.len() >= MAX_HISTORY_RUNTIME_CAPABILITIES
            && !self.capabilities.contains(&capability)
        {
            self.limited = true;
            return;
        }
        self.capabilities.insert(capability);
    }

    pub(in crate::access) fn extend(
        &mut self,
        capabilities: impl IntoIterator<Item = AccessCapability>,
    ) {
        for capability in capabilities {
            self.record(capability);
        }
    }

    pub(in crate::access) fn finish(self) -> (Vec<AccessCapability>, bool) {
        (self.capabilities.into_iter().collect(), self.limited)
    }
}

fn tool_call(object: &serde_json::Map<String, Value>) -> Option<(&str, &Value, Option<&str>)> {
    if object.get("type").and_then(Value::as_str) == Some("tool_use") {
        return Some((
            object.get("name")?.as_str()?,
            object.get("input")?,
            object.get("id").and_then(Value::as_str),
        ));
    }
    if matches!(
        object.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    ) {
        return Some((
            object.get("name")?.as_str()?,
            object.get("arguments").or_else(|| object.get("input"))?,
            object
                .get("call_id")
                .or_else(|| object.get("id"))
                .and_then(Value::as_str),
        ));
    }
    if object.contains_key("arguments") && object.contains_key("name") {
        return Some((
            object.get("name")?.as_str()?,
            object.get("arguments")?,
            object
                .get("id")
                .or_else(|| object.get("toolCallId"))
                .and_then(Value::as_str),
        ));
    }
    None
}

fn inspect_tool_call(
    name: &str,
    raw_input: &Value,
    workspace: Option<&Path>,
    source_path: &Path,
    timestamp: Option<u64>,
    observations: &mut ObservationCollector,
) {
    let parsed;
    let input = if let Some(encoded) = raw_input.as_str() {
        parsed = serde_json::from_str::<Value>(encoded).unwrap_or(Value::Null);
        &parsed
    } else {
        raw_input
    };
    let workspace = input
        .get("workdir")
        .or_else(|| input.get("cwd"))
        .and_then(path_value)
        .map(PathBuf::from)
        .or_else(|| workspace.map(Path::to_path_buf));

    if is_read_tool(name) || is_write_tool(name) {
        let operation = if is_write_tool(name) {
            AccessOperation::Write
        } else {
            AccessOperation::Read
        };
        for value in tool_paths(input) {
            let Some(path) = normalized_tool_path(value, workspace.as_deref()) else {
                continue;
            };
            observations.record(
                AccessCategory::Filesystem,
                path.display().to_string(),
                operation.clone(),
                workspace.clone(),
                AccessConfidence::High,
                source_path,
                timestamp,
            );
        }
    }

    if is_shell_tool(name)
        && let Some(command) = input.get("command").and_then(command_text)
    {
        observations.record(
            AccessCategory::Execution,
            safe_command_identifier(&command),
            AccessOperation::Execute,
            workspace.clone(),
            AccessConfidence::High,
            source_path,
            timestamp,
        );
        for host in hosts_from_text(&command) {
            observations.record(
                AccessCategory::Network,
                host,
                AccessOperation::Connect,
                workspace.clone(),
                AccessConfidence::Heuristic,
                source_path,
                timestamp,
            );
        }
    }

    let browser = is_browser_tool(name);
    if is_web_tool(name) || browser {
        for url in tool_urls(input) {
            let Some(host) = host_from_url(url) else {
                continue;
            };
            observations.record(
                if browser {
                    AccessCategory::Browser
                } else {
                    AccessCategory::Network
                },
                host,
                AccessOperation::Connect,
                workspace.clone(),
                AccessConfidence::High,
                source_path,
                timestamp,
            );
        }
    }
    if is_web_search_tool(name) {
        observations.record(
            AccessCategory::Network,
            "hosted web search".to_owned(),
            AccessOperation::Use,
            workspace.clone(),
            AccessConfidence::High,
            source_path,
            timestamp,
        );
    }
    if let Some(resource) = mcp_resource(name) {
        observations.record(
            AccessCategory::ExternalService,
            resource,
            AccessOperation::Use,
            workspace,
            AccessConfidence::High,
            source_path,
            timestamp,
        );
    }
}

fn is_read_tool(name: &str) -> bool {
    matches!(
        name,
        "Read"
            | "ReadFile"
            | "Glob"
            | "Grep"
            | "copilot_readFile"
            | "file_search"
            | "get_errors"
            | "glob"
            | "grep_search"
            | "list_dir"
            | "list_directory"
            | "read_file"
            | "read_many_files"
            | "semantic_search"
    )
}

fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "ApplyPatch"
            | "Delete"
            | "Edit"
            | "StrReplace"
            | "Write"
            | "apply_patch"
            | "create_file"
            | "insert_edit_into_file"
            | "replace"
            | "replace_string_in_file"
            | "write_file"
    )
}

fn is_shell_tool(name: &str) -> bool {
    matches!(
        name,
        "Bash"
            | "PowerShell"
            | "Shell"
            | "exec_command"
            | "run_in_terminal"
            | "run_shell_command"
            | "shell"
    )
}

fn is_web_tool(name: &str) -> bool {
    matches!(name, "WebFetch" | "fetch_webpage" | "web_fetch")
}

fn is_web_search_tool(name: &str) -> bool {
    matches!(name, "WebSearch" | "google_web_search" | "web_search")
}

fn is_browser_tool(name: &str) -> bool {
    name.contains("browser_")
        || matches!(
            name,
            "navigate_page" | "open_browser_page" | "run_playwright_code"
        )
}

fn mcp_resource(name: &str) -> Option<String> {
    let name = name.strip_prefix("mcp__")?;
    let mut parts = name.split("__");
    let server = parts.next()?;
    let tool = parts.next();
    Some(match tool {
        Some(tool) => format!("mcp:{server}/{tool}"),
        None => format!("mcp:{server}"),
    })
}

fn tool_paths(input: &Value) -> Vec<&str> {
    let mut values = Vec::new();
    for key in [
        "filePath",
        "file_path",
        "notebook_path",
        "path",
        "dir_path",
        "uri",
    ] {
        if let Some(value) = input.get(key).and_then(path_value) {
            values.push(value);
        }
    }
    for key in ["filePaths", "include"] {
        if let Some(paths) = input.get(key).and_then(Value::as_array) {
            values.extend(paths.iter().filter_map(path_value));
        }
    }
    values
}

fn tool_urls(input: &Value) -> Vec<&str> {
    let mut values = Vec::new();
    if let Some(url) = input.get("url").and_then(Value::as_str) {
        values.push(url);
    }
    if let Some(urls) = input.get("urls").and_then(Value::as_array) {
        values.extend(urls.iter().filter_map(Value::as_str));
    }
    values
}

fn path_value(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("fsPath").and_then(Value::as_str))
        .or_else(|| value.get("path").and_then(Value::as_str))
}

fn normalized_tool_path(value: &str, workspace: Option<&Path>) -> Option<PathBuf> {
    if value.starts_with("file:") {
        return url::Url::parse(value).ok()?.to_file_path().ok();
    }
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace?.join(path)
    };
    Some(lexically_normalize(&path))
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn command_text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned).or_else(|| {
        value.as_array().map(|parts| {
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
    })
}

fn hosts_from_text(value: &str) -> Vec<String> {
    let mut hosts = BTreeSet::new();
    for scheme in ["https://", "http://"] {
        let mut remainder = value;
        while let Some(index) = remainder.find(scheme) {
            let candidate = &remainder[index..];
            let end = candidate
                .find(|character: char| {
                    character.is_whitespace() || "\"'<>[]{}(),;".contains(character)
                })
                .unwrap_or(candidate.len());
            if let Some(host) = host_from_url(&candidate[..end]) {
                hosts.insert(host);
            }
            remainder = &candidate[scheme.len()..];
        }
    }
    hosts.into_iter().collect()
}

pub(super) fn workspace_from_record(value: &Value) -> Option<PathBuf> {
    value
        .get("cwd")
        .and_then(path_value)
        .or_else(|| value.get("payload")?.get("cwd").and_then(path_value))
        .map(PathBuf::from)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ObservationKey {
    category: AccessCategory,
    resource: String,
    operation: Option<AccessOperation>,
    workspace: Option<PathBuf>,
}

struct ObservationValue {
    count: u64,
    confidence: AccessConfidence,
    evidence_updated_at_unix_ms: Option<u64>,
    operations: BTreeSet<AccessOperation>,
    resources: BTreeSet<String>,
    sources: BTreeSet<PathBuf>,
    workspaces: BTreeSet<PathBuf>,
}

#[derive(Default)]
pub(super) struct ObservationCollector {
    values: BTreeMap<ObservationKey, ObservationValue>,
    limited: bool,
}

impl ObservationCollector {
    #[expect(clippy::too_many_arguments)]
    fn record(
        &mut self,
        category: AccessCategory,
        resource: String,
        operation: AccessOperation,
        workspace: Option<PathBuf>,
        confidence: AccessConfidence,
        source_path: &Path,
        timestamp: Option<u64>,
    ) {
        let category = match category {
            AccessCategory::Browser => AccessCategory::Network,
            category => category,
        };
        let heuristic = confidence == AccessConfidence::Heuristic;
        let grouped = matches!(
            category,
            AccessCategory::Execution | AccessCategory::Filesystem
        );
        let grouped_resource = if category == AccessCategory::Filesystem {
            filesystem_group(&resource, workspace.as_deref())
        } else {
            resource.clone()
        };
        let key = ObservationKey {
            category,
            resource: grouped_resource,
            operation: (!grouped).then(|| operation.clone()),
            workspace: (!grouped).then(|| workspace.clone()).flatten(),
        };
        if !self.values.contains_key(&key) && self.values.len() >= MAX_HISTORY_OBSERVATIONS {
            self.limited = true;
            return;
        }
        let entry = self.values.entry(key).or_insert_with(|| ObservationValue {
            count: 0,
            confidence,
            evidence_updated_at_unix_ms: timestamp,
            operations: BTreeSet::new(),
            resources: BTreeSet::new(),
            sources: BTreeSet::new(),
            workspaces: BTreeSet::new(),
        });
        entry.count = entry.count.saturating_add(1);
        if heuristic {
            entry.confidence = AccessConfidence::Heuristic;
        }
        entry.evidence_updated_at_unix_ms = entry.evidence_updated_at_unix_ms.max(timestamp);
        entry.operations.insert(operation);
        entry.resources.insert(resource);
        entry.sources.insert(source_path.to_path_buf());
        if let Some(workspace) = workspace {
            entry.workspaces.insert(workspace);
        }
    }

    pub(super) fn finish(self) -> (Vec<AccessObservation>, bool) {
        self.finish_with_limit(MAX_HISTORY_OBSERVATIONS)
    }

    fn finish_with_limit(self, limit: usize) -> (Vec<AccessObservation>, bool) {
        let collection_limited = self.limited;
        let mut observations: Vec<_> = self
            .values
            .into_iter()
            .map(|(key, value)| {
                let workspace = if value.workspaces.len() == 1 {
                    value.workspaces.first().cloned()
                } else {
                    key.workspace
                };
                let source_path = (value.sources.len() == 1)
                    .then(|| value.sources.first().cloned())
                    .flatten();
                AccessObservation {
                    category: key.category,
                    resource: key.resource,
                    operations: value.operations.into_iter().collect(),
                    workspace,
                    count: value.count,
                    session_count: value.sources.len().try_into().unwrap_or(u64::MAX),
                    resource_count: value.resources.len().try_into().unwrap_or(u64::MAX),
                    workspace_count: value.workspaces.len().try_into().unwrap_or(u64::MAX),
                    evidence_updated_at_unix_ms: value.evidence_updated_at_unix_ms,
                    confidence: value.confidence,
                    source: source(AccessSourceKind::History, source_path),
                }
            })
            .collect();
        observations.sort_by(|left, right| {
            right
                .evidence_updated_at_unix_ms
                .cmp(&left.evidence_updated_at_unix_ms)
                .then_with(|| right.count.cmp(&left.count))
                .then_with(|| left.cmp(right))
        });
        let limited = collection_limited || observations.len() > limit;
        observations.truncate(limit);
        (observations, limited)
    }
}

fn filesystem_group(resource: &str, workspace: Option<&Path>) -> String {
    let path = Path::new(resource);
    if let Some(workspace) = workspace
        && path.starts_with(workspace)
    {
        return workspace.display().to_string();
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
    };

    use agentdesktop_core::model::{
        AccessCategory, AccessConfidence, AccessDecision, AccessEnforcement, AccessOperation,
    };
    use serde_json::json;

    use super::{
        ObservationCollector, RuntimeCollector, hosts_from_text, normalized_tool_path,
        runtime_capability, visit_value,
    };
    use crate::access::history_scan::HistoryAdapter;

    fn ignore_runtime(
        _object: &serde_json::Map<String, serde_json::Value>,
        _workspace: Option<&Path>,
        _collected: &mut RuntimeCollector,
    ) {
    }

    fn test_adapter() -> HistoryAdapter {
        HistoryAdapter {
            kind: "test",
            root: Path::new("unused").to_path_buf(),
            include_file: None,
            workspace_for_file: None,
            inspect_runtime: ignore_runtime,
            coverage_limitation: None,
        }
    }

    #[test]
    fn extracts_only_normalized_resources_from_tool_calls() {
        let workspace = std::env::temp_dir().join("agentdesktop-history-workspace");
        let value = json!({
            "toolCalls": [
                {
                    "id": "read-1",
                    "name": "read_file",
                    "arguments": "{\"filePath\":\"src/../src/main.rs\"}"
                },
                {
                    "id": "shell-1",
                    "name": "run_in_terminal",
                    "arguments": "{\"command\":\"TOKEN=super-secret curl https://api.example.com/private?token=super-secret\"}"
                }
            ]
        });
        let mut observations = ObservationCollector::default();
        visit_value(
            &test_adapter(),
            &value,
            Some(&workspace),
            Path::new("history.jsonl"),
            Some(10),
            &mut BTreeSet::new(),
            &mut observations,
            &mut RuntimeCollector::default(),
        );
        let (observations, _) = observations.finish();
        let filesystem = observations
            .iter()
            .find(|observation| {
                observation.category == AccessCategory::Filesystem
                    && observation.operations.contains(&AccessOperation::Read)
                    && observation.confidence == AccessConfidence::High
            })
            .expect("read_file should produce a high-confidence filesystem observation");

        assert_eq!(Path::new(&filesystem.resource), workspace.as_path());
        assert_eq!(filesystem.resource_count, 1);
        assert!(observations.iter().any(|observation| {
            observation.category == AccessCategory::Network
                && observation.resource == "api.example.com"
                && observation.confidence == AccessConfidence::Heuristic
        }));
        assert!(!format!("{observations:?}").contains("super-secret"));
    }

    #[test]
    fn normalizes_relative_tool_paths_with_target_separators() {
        let workspace = std::env::temp_dir().join("agentdesktop-history-workspace");
        let expected = workspace.join("src").join("main.rs");

        assert_eq!(
            normalized_tool_path("src/../src/main.rs", Some(&workspace)).as_deref(),
            Some(expected.as_path())
        );
    }

    #[test]
    fn extracts_hosts_without_paths_or_credentials() {
        assert_eq!(
            hosts_from_text(
                "curl https://user:secret@example.com/private && wget http://localhost:8080/a"
            ),
            vec!["example.com", "localhost"]
        );
    }

    #[test]
    fn observation_output_is_bounded() {
        let mut observations = ObservationCollector::default();
        for (index, timestamp) in [10, 30, 20].into_iter().enumerate() {
            observations.record(
                AccessCategory::Network,
                format!("host-{index}.example.com"),
                AccessOperation::Connect,
                None,
                AccessConfidence::High,
                Path::new("history.jsonl"),
                Some(timestamp),
            );
        }

        let (observations, limited) = observations.finish_with_limit(2);

        assert!(limited);
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].evidence_updated_at_unix_ms, Some(30));
        assert_eq!(observations[1].evidence_updated_at_unix_ms, Some(20));
    }

    #[test]
    fn groups_commands_across_sessions_and_filesystem_by_workspace() {
        let workspace = PathBuf::from("/workspace/project");
        let other_workspace = PathBuf::from("/workspace/other");
        let mut observations = ObservationCollector::default();
        observations.record(
            AccessCategory::Execution,
            "cd".to_owned(),
            AccessOperation::Execute,
            Some(workspace.clone()),
            AccessConfidence::High,
            Path::new("session-1.jsonl"),
            Some(10),
        );
        observations.record(
            AccessCategory::Execution,
            "cd".to_owned(),
            AccessOperation::Execute,
            Some(other_workspace),
            AccessConfidence::High,
            Path::new("session-2.jsonl"),
            Some(20),
        );
        observations.record(
            AccessCategory::Filesystem,
            workspace.join("src/a.rs").display().to_string(),
            AccessOperation::Read,
            Some(workspace.clone()),
            AccessConfidence::High,
            Path::new("session-1.jsonl"),
            Some(10),
        );
        observations.record(
            AccessCategory::Filesystem,
            workspace.join("src/b.rs").display().to_string(),
            AccessOperation::Write,
            Some(workspace.clone()),
            AccessConfidence::High,
            Path::new("session-2.jsonl"),
            Some(20),
        );

        let (observations, limited) = observations.finish();

        assert!(!limited);
        assert_eq!(observations.len(), 2);
        let command = observations
            .iter()
            .find(|observation| observation.category == AccessCategory::Execution)
            .unwrap();
        assert_eq!(command.resource, "cd");
        assert_eq!(command.operations, vec![AccessOperation::Execute]);
        assert_eq!(command.count, 2);
        assert_eq!(command.session_count, 2);
        assert_eq!(command.resource_count, 1);
        assert_eq!(command.workspace_count, 2);
        assert!(command.workspace.is_none());
        assert!(command.source.path.is_none());

        let filesystem = observations
            .iter()
            .find(|observation| observation.category == AccessCategory::Filesystem)
            .unwrap();
        assert_eq!(Path::new(&filesystem.resource), workspace);
        assert_eq!(
            filesystem.operations,
            vec![AccessOperation::Read, AccessOperation::Write]
        );
        assert_eq!(filesystem.count, 2);
        assert_eq!(filesystem.session_count, 2);
        assert_eq!(filesystem.resource_count, 2);
        assert_eq!(filesystem.workspace_count, 1);
        assert_eq!(filesystem.workspace.as_deref(), Some(workspace.as_path()));
        assert!(filesystem.source.path.is_none());
    }

    #[test]
    fn groups_browser_and_network_observations_by_host_and_workspace() {
        let workspace = PathBuf::from("/workspace/project");
        let mut observations = ObservationCollector::default();
        for (category, confidence, source) in [
            (
                AccessCategory::Network,
                AccessConfidence::Heuristic,
                "session-1.jsonl",
            ),
            (
                AccessCategory::Browser,
                AccessConfidence::High,
                "session-1.jsonl",
            ),
            (
                AccessCategory::Browser,
                AccessConfidence::High,
                "session-2.jsonl",
            ),
        ] {
            observations.record(
                category,
                "localhost".to_owned(),
                AccessOperation::Connect,
                Some(workspace.clone()),
                confidence,
                Path::new(source),
                Some(20),
            );
        }

        let (observations, limited) = observations.finish();

        assert!(!limited);
        assert_eq!(observations.len(), 1);
        let localhost = &observations[0];
        assert_eq!(localhost.category, AccessCategory::Network);
        assert_eq!(localhost.resource, "localhost");
        assert_eq!(localhost.operations, vec![AccessOperation::Connect]);
        assert_eq!(localhost.count, 3);
        assert_eq!(localhost.session_count, 2);
        assert_eq!(localhost.workspace_count, 1);
        assert_eq!(localhost.confidence, AccessConfidence::Heuristic);
    }

    #[test]
    fn observation_collection_is_bounded_while_scanning() {
        let mut observations = ObservationCollector::default();
        for index in 0..=super::MAX_HISTORY_OBSERVATIONS {
            observations.record(
                AccessCategory::Network,
                format!("host-{index}.example.com"),
                AccessOperation::Connect,
                None,
                AccessConfidence::High,
                Path::new("history.jsonl"),
                Some(10),
            );
        }

        assert_eq!(observations.values.len(), super::MAX_HISTORY_OBSERVATIONS);
        let (observations, limited) = observations.finish();
        assert!(limited);
        assert_eq!(observations.len(), super::MAX_HISTORY_OBSERVATIONS);
    }

    #[test]
    fn runtime_control_collection_is_bounded_while_scanning() {
        let mut runtime = RuntimeCollector::default();
        for index in 0..=super::MAX_HISTORY_RUNTIME_CAPABILITIES {
            runtime.record(runtime_capability(
                AccessCategory::Execution,
                "shell commands",
                vec![AccessOperation::Execute],
                AccessDecision::Ask,
                AccessEnforcement::Harness,
                Some(Path::new(&format!("/workspace/{index}"))),
                "Recorded approval policy",
            ));
        }

        assert_eq!(
            runtime.capabilities.len(),
            super::MAX_HISTORY_RUNTIME_CAPABILITIES
        );
        let (capabilities, limited) = runtime.finish();
        assert!(limited);
        assert_eq!(capabilities.len(), super::MAX_HISTORY_RUNTIME_CAPABILITIES);
    }
}
