use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxCapability {
    Read,
    Write,
    Network,
    Exec,
    Unknown,
}

impl SandboxCapability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Network => "network",
            Self::Exec => "exec",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPermissionDenied {
    pub tool_name: String,
    pub execution_kind: String,
    pub capability: SandboxCapability,
    pub target: String,
    pub diagnostic: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDenied {
    pub approval_id: String,
    pub tool_name: String,
    pub reason: String,
}

#[derive(Debug, Error)]
pub enum FrameworkError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error(
        "sandbox permission denied: tool={tool_name} execution_kind={execution_kind} capability={} target={} diagnostic={}",
        .capability.as_str(),
        target,
        diagnostic
    )]
    SandboxPermissionDenied {
        tool_name: String,
        execution_kind: String,
        capability: SandboxCapability,
        target: String,
        diagnostic: String,
    },

    #[error("approval denied: tool={tool_name} approval_id={approval_id} reason={reason}")]
    ApprovalDenied {
        approval_id: String,
        tool_name: String,
        reason: String,
    },

    #[error("configuration error: {0}")]
    Config(String),
}

impl FrameworkError {
    pub fn sandbox_permission_denied(details: SandboxPermissionDenied) -> Self {
        Self::SandboxPermissionDenied {
            tool_name: details.tool_name,
            execution_kind: details.execution_kind,
            capability: details.capability,
            target: details.target,
            diagnostic: details.diagnostic,
        }
    }

    pub fn approval_denied(details: ApprovalDenied) -> Self {
        Self::ApprovalDenied {
            approval_id: details.approval_id,
            tool_name: details.tool_name,
            reason: details.reason,
        }
    }

    pub fn as_sandbox_permission_denied(&self) -> Option<SandboxPermissionDenied> {
        match self {
            Self::SandboxPermissionDenied {
                tool_name,
                execution_kind,
                capability,
                target,
                diagnostic,
            } => Some(SandboxPermissionDenied {
                tool_name: tool_name.clone(),
                execution_kind: execution_kind.clone(),
                capability: capability.clone(),
                target: target.clone(),
                diagnostic: diagnostic.clone(),
            }),
            _ => None,
        }
    }
}
