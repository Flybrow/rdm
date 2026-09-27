//! Pure domain model: no I/O, no async.

mod download;
mod file_kind;
mod segment;

pub use download::{Download, DownloadError, DownloadId, Source, Status};
pub use file_kind::{Category, default_captured, is_capturable};
pub use segment::{Segment, plan_segments};

pub const DEFAULT_CONNECTIONS: u8 = 32;
pub const MAX_CONNECTIONS: u8 = 64;
