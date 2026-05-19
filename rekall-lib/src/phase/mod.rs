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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_display() {
        assert_eq!(Phase::Init.to_string(), "init");
        assert_eq!(Phase::Search.to_string(), "search");
        assert_eq!(Phase::Report.to_string(), "report");
    }

    #[test]
    fn phase_from_str() {
        assert_eq!("init".parse::<Phase>().unwrap(), Phase::Init);
        assert_eq!("LOAD".parse::<Phase>().unwrap(), Phase::Load);
        assert_eq!("Index".parse::<Phase>().unwrap(), Phase::Index);
    }

    #[test]
    fn phase_from_str_invalid() {
        let err = "foo".parse::<Phase>().unwrap_err();
        assert!(err.contains("Unknown phase"));
    }

    #[test]
    fn phase_all() {
        let all = Phase::all();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0], Phase::Init);
        assert_eq!(all[4], Phase::Report);
    }

    #[test]
    fn phase_roundtrip() {
        for phase in Phase::all() {
            let s = phase.to_string();
            let parsed: Phase = s.parse().unwrap();
            assert_eq!(*phase, parsed);
        }
    }
}
