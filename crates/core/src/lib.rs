//! Ядро FileHunter (filecat-core): индекс файлов, сканеры, поиск и дубликаты.

pub mod dupes;
pub mod dupfind;
pub mod index;
pub mod scan;
pub mod remote;
pub mod safety;
pub mod search;
pub mod throttle;
pub mod util;
pub mod volumes;

pub use index::{Entry, FileRef, Index, Segment, SourceInfo, SourceKind};
