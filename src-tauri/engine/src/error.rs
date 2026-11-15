#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    NoSpace(u64),
    Storage(String),
    Cancelled,
    NotExport,
    Busy,
    Link,
    Throttled,
    Network,
    BadZip(&'static str),
}

impl Stop {
    pub fn kind(&self) -> &'static str {
        match self {
            Stop::NoSpace(_) => "no_space",
            Stop::Storage(_) => "storage",
            Stop::Cancelled => "cancelled",
            Stop::NotExport => "not_export",
            Stop::Busy => "busy",
            Stop::Link => "link",
            Stop::Throttled => "throttled",
            Stop::Network => "network",
            Stop::BadZip(_) => "bad_zip",
        }
    }
}
