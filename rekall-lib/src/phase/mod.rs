pub mod index;
pub mod init;
pub mod load;
pub mod report;
pub mod search;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Init,
    Load,
    Index,
    Search,
    Report,
}

impl Phase {
    pub fn all() -> &'static [Phase] {
        &[
            Phase::Init,
            Phase::Load,
            Phase::Index,
            Phase::Search,
            Phase::Report,
        ]
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Init => write!(f, "init"),
            Phase::Load => write!(f, "load"),
            Phase::Index => write!(f, "index"),
            Phase::Search => write!(f, "search"),
            Phase::Report => write!(f, "report"),
        }
    }
}

impl std::str::FromStr for Phase {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "init" => Ok(Phase::Init),
            "load" => Ok(Phase::Load),
            "index" => Ok(Phase::Index),
            "search" => Ok(Phase::Search),
            "report" => Ok(Phase::Report),
            _ => Err(format!(
                "Unknown phase '{s}'. Valid: init, load, index, search, report"
            )),
        }
    }
}
