//! Turning captured requests into things other tools read: cURL commands, HAR files, and the
//! lines of `traffic-police tail`.

pub mod curl;
pub mod har;
pub mod tail;
