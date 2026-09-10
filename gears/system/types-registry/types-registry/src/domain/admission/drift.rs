//! Shared revision-vector drift types.

use toolkit_macros::domain_model;

/// Which side of the candidate an entry stands on.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum VectorRole {
    /// Evaluation consumed this entity's authored document while building the transient store.
    Dependency,
    /// The commit refresh consumes this dependent's artifacts.
    Dependent,
}

impl VectorRole {
    /// The word a drift message uses.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Dependency => "dependency",
            Self::Dependent => "dependent",
        }
    }
}

impl std::fmt::Display for VectorRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The first difference between a recorded vector and a freshly-derived one.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VectorDrift {
    /// Not there when evaluation looked.
    Appeared { gts_id: String, role: VectorRole },
    /// There when evaluation looked, gone now.
    Vanished { gts_id: String, role: VectorRole },
    /// A new revision landed: `resource_version` moved.
    Moved {
        gts_id: String,
        role: VectorRole,
        recorded: i64,
        found: i64,
    },
    /// Someone else's commit re-materialized this dependent's artifacts.
    Refreshed { gts_id: String },
    /// An artifact write lost its compare-and-swap; re-evaluation resolves the cause.
    CurrentProjectionMoved { gts_id: String },
}

impl std::fmt::Display for VectorDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Appeared { gts_id, role } => {
                write!(f, "{role} '{gts_id}' appeared after evaluation")
            }
            Self::Vanished { gts_id, role } => {
                write!(f, "{role} '{gts_id}' disappeared after evaluation")
            }
            Self::Moved {
                gts_id,
                role,
                recorded,
                found,
            } => write!(
                f,
                "{role} '{gts_id}' moved from resource_version {recorded} to {found} after \
                 evaluation"
            ),
            Self::Refreshed { gts_id } => write!(
                f,
                "dependent '{gts_id}' had its effective artifacts refreshed after evaluation"
            ),
            Self::CurrentProjectionMoved { gts_id } => write!(
                f,
                "'{gts_id}' had its current state moved between the read this commit's \
                 artifacts were computed against and the write"
            ),
        }
    }
}
