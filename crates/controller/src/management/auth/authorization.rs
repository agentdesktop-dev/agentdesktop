/// Policy verb and its request-safety semantics. New verbs must explicitly define
/// whether they mutate; callers cannot independently disable mutation protections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Read,
    List,
    Create,
    Update,
    Delete,
}

impl Verb {
    pub fn is_mutation(self) -> bool {
        match self {
            Self::Read | Self::List => false,
            Self::Create | Self::Update | Self::Delete => true,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::List => "list",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Clone, Copy, Debug)]
/// Domain-level policy question asked by handlers and answered by the authentication provider.
pub struct AccessRequest<'a> {
    pub verb: Verb,
    /// Policy category, e.g. `devices` or `rbac.policy`, not an HTTP path.
    /// Existing `admin` permissions cover several management resource categories.
    pub resource_class: &'a str,
    /// Concrete resource ID, or `None` for collection/global operations.
    /// Resource-scoped implementations must distinguish these cases explicitly.
    pub resource: Option<&'a str>,
}

impl AccessRequest<'_> {
    /// Stable name used by the RBAC catalog and UI capability lists.
    pub fn permission(&self) -> String {
        format!("{}.{}", self.resource_class, self.verb.as_str())
    }

    /// Whether `admin.read`, the umbrella read permission, grants this operation:
    /// every non-mutating operation in `OPERATIONS`, but no mutation and nothing
    /// outside it (such as an extension's own reads).
    pub fn covered_by_admin_read(&self) -> bool {
        !self.verb.is_mutation()
            && OPERATIONS
                .iter()
                .any(|&(verb, class)| verb == self.verb && class == self.resource_class)
    }
}

/// The management console. `admin.read` admits a user to it and is the umbrella read
/// permission: it also grants every other read in `OPERATIONS`
/// (see `AccessRequest::covered_by_admin_read`).
pub const ADMIN: &str = "admin";
pub const DEVICES: &str = "devices";
/// The management audit log.
pub const AUDIT: &str = "audit";
pub const DAEMON_CONFIG: &str = "daemon-config";
pub const SETTINGS: &str = "settings";
pub const USAGE: &str = "usage";

/// Every operation checked by the management handlers. Local access grants all of
/// them; providers with their own permission catalogs can build on this list.
/// `admin.read` covers every read here, so a new read is granted to all its holders.
pub const OPERATIONS: &[(Verb, &str)] = &[
    (Verb::Read, ADMIN),
    (Verb::List, DEVICES),
    (Verb::Read, DEVICES),
    (Verb::Delete, DEVICES),
    (Verb::Read, DAEMON_CONFIG),
    (Verb::Read, SETTINGS),
    (Verb::Read, USAGE),
    (Verb::Read, AUDIT),
];

/// Permission names for `OPERATIONS`, as reported to the UI.
pub fn permission_names() -> Vec<String> {
    OPERATIONS
        .iter()
        .map(|&(verb, resource_class)| {
            AccessRequest {
                verb,
                resource_class,
                resource: None,
            }
            .permission()
        })
        .collect()
}
