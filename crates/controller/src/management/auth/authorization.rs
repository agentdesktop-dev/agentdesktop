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
}

/// Read access to fleet, configuration, settings and usage views.
pub const ADMIN: &str = "admin";
pub const DEVICES: &str = "devices";

/// Every operation checked by the management handlers. Local access grants all of
/// them; providers with their own permission catalogs can build on this list.
pub const OPERATIONS: &[(Verb, &str)] = &[(Verb::Read, ADMIN), (Verb::Delete, DEVICES)];

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
