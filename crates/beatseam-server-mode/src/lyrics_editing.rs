use std::future::Future;

use beatseam_api::ApiClientError;
use beatseam_core::file_access::FileRef;

/// What the lyrics editor shows for a track: the file assigned to it and every file it could be assigned.
#[derive(Clone, Debug, PartialEq)]
pub struct LyricsSource {
    /// `None` = no file assigned.
    pub assigned: Option<String>,
    pub candidates: Vec<FileRef>,
}

/// One line to write back: `None` time for an untimed line.
#[derive(Clone, Debug, PartialEq)]
pub struct EditedLine {
    pub time_seconds: Option<f64>,
    pub text: String,
}

/// Changing a track's lyrics on the server (admins only): which .lrc file it uses, and the contents of that file. What the
/// lyrics screens use in place of reading and writing .lrc files themselves when the library comes from a server.
pub trait ServerLyricsEditing {
    fn source(&self, track_file_id: &str) -> impl Future<Output = Result<LyricsSource, ApiClientError>>;
    fn assign(&self, track_file_id: &str, lrc_file_id: Option<&str>) -> impl Future<Output = Result<(), ApiClientError>>;
    /// Writes the lines into the assigned file.
    fn save(&self, track_file_id: &str, lines: &[EditedLine]) -> impl Future<Output = Result<(), ApiClientError>>;
}
