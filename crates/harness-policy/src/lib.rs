use std::fmt;

use harness_core::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Permission {
    ReadWorkspace,
    WriteWorkspace,
    ExecuteCommand,
    AccessNetwork,
}

impl fmt::Display for Permission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::ReadWorkspace => "read-workspace",
            Self::WriteWorkspace => "write-workspace",
            Self::ExecuteCommand => "execute-command",
            Self::AccessNetwork => "access-network",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyDecision {
    Allow,
    Deny,
}

pub trait Policy: Send + Sync {
    fn check(&self, permission: Permission) -> PolicyDecision;
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DenyAllPolicy;

impl Policy for DenyAllPolicy {
    fn check(&self, _permission: Permission) -> PolicyDecision {
        PolicyDecision::Deny
    }
}

pub fn authorize(policy: &dyn Policy, permission: Permission) -> Result<(), Error> {
    let capability = permission.to_string();
    match policy.check(permission) {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Deny => Err(Error::PermissionDenied { capability }),
    }
}
