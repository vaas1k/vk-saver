use crate::export::index::Status;
use crate::export::scan::Section;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Bytes(u64),
    /// A file taken from an older export instead of a download.
    Reused,
    Total(u64),
    Item {
        section: Section,
        status: Status,
    },
    Paused {
        until_ms: u64,
    },
    Unzip {
        done: u64,
        total: u64,
    },
}

pub trait Progress {
    fn event(&self, e: Event);
}
