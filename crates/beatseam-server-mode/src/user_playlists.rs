use std::collections::HashSet;
use std::fmt;

use beatseam_api::encode_track_key;
use beatseam_api::mutations::MutationDto;
use beatseam_core::library_store::{LibraryStore, PlaylistRecord, StoreError};

use crate::apply_sync::USER_PLAYLIST_ID_PREFIX;
use crate::mutation_queue::{MutationQueue, MutationSender};

/// Why a playlist edit was refused (the message is shown to the person as is).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserPlaylistError {
    Message(String),
    Store(StoreError),
}

impl fmt::Display for UserPlaylistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(m) => f.write_str(m),
            Self::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for UserPlaylistError {}

impl From<StoreError> for UserPlaylistError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

fn refuse<T>(message: &str) -> Result<T, UserPlaylistError> {
    Err(UserPlaylistError::Message(message.to_owned()))
}

pub struct UserPlaylistDeps<'a, S: LibraryStore + 'static, A: MutationSender + 'static> {
    pub store: &'a S,
    pub queue: &'a MutationQueue<S, A>,
    /// The first root the server has told us about (where a new playlist is filed), if any.
    pub first_root_id: Option<String>,
    /// A fresh random id for a playlist created here (see [`random_id`](crate::random_id)).
    pub new_id: &'a dyn Fn() -> String,
}

/// Whether a local playlist id is one the user built in the app (editable), as opposed to a library `.m3u8` (the
/// server's).
pub fn is_user_playlist_id(id: &str) -> bool {
    id.starts_with(USER_PLAYLIST_ID_PREFIX)
}

fn server_id_of(local_id: &str) -> String {
    local_id.strip_prefix(USER_PLAYLIST_ID_PREFIX).unwrap_or(local_id).to_owned()
}

fn keys_of(file_ids: &[String]) -> Vec<String> {
    file_ids.iter().map(|f| encode_track_key(f)).collect()
}

fn unique(ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.iter().filter(|id| seen.insert(id.as_str())).cloned().collect()
}

/// Truncates to `max` UTF-16 code units, like JavaScript's `slice(0, 200)` (never splitting a surrogate pair here: a pair
/// that straddles the limit is dropped whole).
fn limit_name(name: &str) -> String {
    let mut units = 0;
    name.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 200
        })
        .collect()
}

async fn find<S: LibraryStore>(store: &S, id: &str) -> Result<PlaylistRecord, UserPlaylistError> {
    match store.list_all_playlists().await?.into_iter().find(|p| p.id == id) {
        Some(found) => Ok(found),
        None => refuse("That playlist no longer exists"),
    }
}

impl<S: LibraryStore + 'static, A: MutationSender + 'static> UserPlaylistDeps<'_, S, A> {
    async fn enqueue(&self, mutation: MutationDto) {
        self.queue.enqueue(mutation).await;
    }

    /// Edits to a user's own playlists. Each applies to the local library at once (so the screen reflects it with no
    /// waiting, and the app works offline) and is queued for the server; the next check-in brings back the server's version
    /// of the result, which is what the other devices see too.
    pub async fn create(&self, name: &str, file_ids: &[String]) -> Result<PlaylistRecord, UserPlaylistError> {
        let Some(root_id) = self.first_root_id.clone() else { return refuse("Sync with the server before creating a playlist") };
        let trimmed = beatseam_core::js_compat::js_trim(name);
        if trimmed.is_empty() {
            return refuse("Give the playlist a name");
        }
        let server_id = (self.new_id)();
        let id = format!("{USER_PLAYLIST_ID_PREFIX}{server_id}");
        let record = PlaylistRecord { id: id.clone(), root_id, file_id: id, name: limit_name(trimmed), track_file_ids: file_ids.to_vec() };
        self.store.upsert_playlist(record.clone()).await?;
        self.enqueue(MutationDto::PlaylistCreate { op_id: String::new(), id: server_id, name: record.name.clone(), track_ids: keys_of(file_ids) }).await;
        Ok(record)
    }

    /// Adds tracks that aren't already in the playlist, at the end or the start.
    pub async fn add_tracks(&self, id: &str, file_ids: &[String], at_start: bool) -> Result<PlaylistRecord, UserPlaylistError> {
        let playlist = find(self.store, id).await?;
        let present: HashSet<&str> = playlist.track_file_ids.iter().map(String::as_str).collect();
        let additions: Vec<String> = unique(file_ids).into_iter().filter(|f| !present.contains(f.as_str())).collect();
        if additions.is_empty() {
            return Ok(playlist);
        }
        let track_file_ids = if at_start { [additions.clone(), playlist.track_file_ids.clone()].concat() } else { [playlist.track_file_ids.clone(), additions.clone()].concat() };
        let updated = PlaylistRecord { track_file_ids: track_file_ids.clone(), ..playlist };
        self.store.upsert_playlist(updated.clone()).await?;
        // Adding at the end is a set-like operation that merges with another device's edits; the start has no such
        // operation, so it replaces the order (the last writer wins).
        if at_start {
            self.enqueue(MutationDto::PlaylistSetTracks { op_id: String::new(), id: server_id_of(id), track_ids: keys_of(&track_file_ids) }).await;
        } else {
            self.enqueue(MutationDto::PlaylistAddTracks { op_id: String::new(), id: server_id_of(id), track_ids: keys_of(&additions) }).await;
        }
        Ok(updated)
    }

    pub async fn remove_tracks(&self, id: &str, file_ids: &[String]) -> Result<PlaylistRecord, UserPlaylistError> {
        let playlist = find(self.store, id).await?;
        let removing: HashSet<&str> = file_ids.iter().map(String::as_str).collect();
        let updated = PlaylistRecord { track_file_ids: playlist.track_file_ids.iter().filter(|f| !removing.contains(f.as_str())).cloned().collect(), ..playlist };
        self.store.upsert_playlist(updated.clone()).await?;
        self.enqueue(MutationDto::PlaylistRemoveTracks { op_id: String::new(), id: server_id_of(id), track_ids: keys_of(file_ids) }).await;
        Ok(updated)
    }

    /// Replaces the order (a reorder).
    pub async fn set_tracks(&self, id: &str, file_ids: &[String]) -> Result<PlaylistRecord, UserPlaylistError> {
        let playlist = find(self.store, id).await?;
        let updated = PlaylistRecord { track_file_ids: file_ids.to_vec(), ..playlist };
        self.store.upsert_playlist(updated.clone()).await?;
        self.enqueue(MutationDto::PlaylistSetTracks { op_id: String::new(), id: server_id_of(id), track_ids: keys_of(file_ids) }).await;
        Ok(updated)
    }

    pub async fn rename(&self, id: &str, name: &str) -> Result<PlaylistRecord, UserPlaylistError> {
        let trimmed = beatseam_core::js_compat::js_trim(name);
        if trimmed.is_empty() {
            return refuse("Give the playlist a name");
        }
        let playlist = find(self.store, id).await?;
        let updated = PlaylistRecord { name: limit_name(trimmed), ..playlist };
        self.store.upsert_playlist(updated.clone()).await?;
        self.enqueue(MutationDto::PlaylistRename { op_id: String::new(), id: server_id_of(id), name: updated.name.clone() }).await;
        Ok(updated)
    }

    pub async fn delete(&self, id: &str) -> Result<(), UserPlaylistError> {
        self.store.delete_playlist(id).await?;
        self.enqueue(MutationDto::PlaylistDelete { op_id: String::new(), id: server_id_of(id) }).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;

    use beatseam_api::ApiClientError;
    use beatseam_api::mutations::MutationsResponseDto;
    use beatseam_core::http::block_on;
    use beatseam_core::library_store::MemoryLibraryStore;

    use super::*;

    struct Never;

    impl MutationSender for Never {
        async fn send_mutations(&self, _m: Vec<MutationDto>) -> Result<MutationsResponseDto, ApiClientError> {
            unreachable!("signed out in these tests")
        }
    }

    struct Fixture {
        store: Arc<MemoryLibraryStore>,
        queue: MutationQueue<MemoryLibraryStore, Never>,
        counter: std::cell::Cell<u32>,
    }

    fn fixture() -> Fixture {
        let store = Arc::new(MemoryLibraryStore::new());
        let queue = block_on(MutationQueue::open(store.clone(), || None, || "op".into(), Box::new(|_f: std::pin::Pin<Box<dyn Future<Output = ()>>>| {}))).unwrap();
        Fixture { store, queue, counter: Default::default() }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    impl Fixture {
        fn deps<'a>(&'a self, root: Option<&str>, new_id: &'a dyn Fn() -> String) -> UserPlaylistDeps<'a, MemoryLibraryStore, Never> {
            UserPlaylistDeps { store: &*self.store, queue: &self.queue, first_root_id: root.map(str::to_owned), new_id }
        }

        fn create(&self, name: &str, tracks: &[&str]) -> PlaylistRecord {
            self.counter.set(self.counter.get() + 1);
            let n = self.counter.get();
            let new_id = move || format!("id{n}");
            block_on(self.deps(Some("root"), &new_id).create(name, &ids(tracks))).unwrap()
        }

        fn deps_now(&self) -> UserPlaylistDeps<'_, MemoryLibraryStore, Never> {
            self.deps(Some("root"), &|| "unused".into())
        }

        fn queued(&self) -> Vec<MutationDto> {
            self.queue.pending_mutations()
        }

        fn tracks_of(&self, id: &str) -> Vec<String> {
            self.store.snapshot_playlists().into_iter().find(|p| p.id == id).unwrap().track_file_ids
        }
    }

    #[test]
    fn creating_files_it_under_the_first_root_and_queues_it_with_encoded_keys() {
        let f = fixture();
        let record = f.create("  Road trip  ", &["Music/a.mp3", "Music/b.mp3"]);
        assert_eq!((record.id.as_str(), record.file_id.as_str(), record.root_id.as_str(), record.name.as_str()), ("user:id1", "user:id1", "root", "Road trip"));
        assert_eq!(f.tracks_of("user:id1"), ["Music/a.mp3", "Music/b.mp3"]);
        assert!(matches!(&f.queued()[..], [MutationDto::PlaylistCreate { id, name, track_ids, .. }] if id == "id1" && name == "Road trip" && *track_ids == keys_of(&ids(&["Music/a.mp3", "Music/b.mp3"]))));
    }

    #[test]
    fn creating_needs_a_synced_root_and_a_name_and_caps_the_name_at_200() {
        let f = fixture();
        assert_eq!(block_on(f.deps(None, &|| "x".into()).create("n", &[])).unwrap_err().to_string(), "Sync with the server before creating a playlist");
        assert_eq!(block_on(f.deps_now().create(" \u{feff} ", &[])).unwrap_err().to_string(), "Give the playlist a name");
        assert_eq!(f.create(&"é".repeat(300), &[]).name.chars().count(), 200);
        assert!(f.queued().len() == 1, "the refused ones queued nothing");
    }

    #[test]
    fn adding_at_the_end_is_a_set_like_add_of_only_the_new_unique_tracks() {
        let f = fixture();
        f.create("P", &["a"]);
        let updated = block_on(f.deps_now().add_tracks("user:id1", &ids(&["a", "b", "b", "c"]), false)).unwrap();
        assert_eq!(updated.track_file_ids, ["a", "b", "c"]);
        assert!(matches!(&f.queued()[1], MutationDto::PlaylistAddTracks { id, track_ids, .. } if id == "id1" && *track_ids == keys_of(&ids(&["b", "c"]))));
    }

    #[test]
    fn adding_at_the_start_replaces_the_order_and_adding_nothing_new_queues_nothing() {
        let f = fixture();
        f.create("P", &["a"]);
        let updated = block_on(f.deps_now().add_tracks("user:id1", &ids(&["x", "y"]), true)).unwrap();
        assert_eq!(updated.track_file_ids, ["x", "y", "a"]);
        assert!(matches!(&f.queued()[1], MutationDto::PlaylistSetTracks { track_ids, .. } if *track_ids == keys_of(&ids(&["x", "y", "a"]))));
        let before = f.queued().len();
        block_on(f.deps_now().add_tracks("user:id1", &ids(&["a"]), false)).unwrap();
        assert_eq!(f.queued().len(), before);
    }

    #[test]
    fn removing_reordering_renaming_and_deleting_apply_locally_and_queue() {
        let f = fixture();
        f.create("P", &["a", "b", "c"]);
        assert_eq!(block_on(f.deps_now().remove_tracks("user:id1", &ids(&["b"]))).unwrap().track_file_ids, ["a", "c"]);
        assert_eq!(block_on(f.deps_now().set_tracks("user:id1", &ids(&["c", "a"]))).unwrap().track_file_ids, ["c", "a"]);
        assert_eq!(block_on(f.deps_now().rename("user:id1", " Renamed ")).unwrap().name, "Renamed");
        block_on(f.deps_now().delete("user:id1")).unwrap();
        assert!(f.store.snapshot_playlists().is_empty());
        let kinds: Vec<&str> = f
            .queued()
            .iter()
            .map(|m| match m {
                MutationDto::PlaylistCreate { .. } => "create",
                MutationDto::PlaylistRemoveTracks { .. } => "remove",
                MutationDto::PlaylistSetTracks { .. } => "set",
                MutationDto::PlaylistRename { .. } => "rename",
                MutationDto::PlaylistDelete { .. } => "delete",
                _ => "?",
            })
            .collect();
        assert_eq!(kinds, ["create", "remove", "set", "rename", "delete"]);
    }

    #[test]
    fn editing_a_playlist_that_is_gone_is_refused_and_a_blank_rename_too() {
        let f = fixture();
        for result in [block_on(f.deps_now().remove_tracks("user:nope", &[])), block_on(f.deps_now().set_tracks("user:nope", &[])), block_on(f.deps_now().rename("user:nope", "x"))] {
            assert_eq!(result.unwrap_err().to_string(), "That playlist no longer exists");
        }
        f.create("P", &[]);
        assert_eq!(block_on(f.deps_now().rename("user:id1", "  ")).unwrap_err().to_string(), "Give the playlist a name");
    }

    #[test]
    fn only_prefixed_ids_are_the_users_own() {
        assert!(is_user_playlist_id("user:abc") && !is_user_playlist_id("Music/Mix.m3u8"));
        assert_eq!(server_id_of("user:abc"), "abc");
    }
}
