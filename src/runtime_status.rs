use anyhow::Result;
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    Unknown,
    Pending,
    Executing,
    Addressing,
    Consultation,
    AwaitingHuman,
    Reviewing,
    Complete,
    Failed,
    Reset,
}

impl TaskStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Pending => "pending",
            Self::Executing => "executing",
            Self::Addressing => "addressing",
            Self::Consultation => "consultation",
            Self::AwaitingHuman => "awaiting_human",
            Self::Reviewing => "reviewing",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Reset => "reset",
        }
    }

    pub const fn clears_lease(self) -> bool {
        matches!(
            self,
            Self::Reset | Self::Reviewing | Self::Addressing | Self::Complete | Self::Failed
        )
    }

    pub const fn is_executor_ready(self) -> bool {
        matches!(self, Self::Pending | Self::Executing | Self::Addressing)
    }

    pub const fn is_executor_working(self) -> bool {
        matches!(self, Self::Executing | Self::Addressing)
    }

    pub const fn is_resettable(self) -> bool {
        !matches!(self, Self::Reset | Self::Complete | Self::Failed)
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Reset | Self::Complete | Self::Failed)
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "unknown" => Ok(Self::Unknown),
            "pending" => Ok(Self::Pending),
            "executing" => Ok(Self::Executing),
            "addressing" => Ok(Self::Addressing),
            "consultation" => Ok(Self::Consultation),
            "awaiting_human" => Ok(Self::AwaitingHuman),
            "reviewing" => Ok(Self::Reviewing),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            "reset" => Ok(Self::Reset),
            _ => anyhow::bail!("unknown task status: {value}"),
        }
    }
}
