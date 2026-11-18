//! Finding, indexing, assigning and loading `.lrc` files: `scanLyricsRoot`, `loadAssignedLyrics` and
//! `ensureLyricsAssignment` from the TypeScript, around one [`LyricsLoader`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::lrc::{ParsedLyrics, parse_lrc};
use super::match_lyrics::{LyricsCandidate, find_auto_lyrics_match};
use super::match_translation_lines::match_translation_lines;
use crate::file_access::{CallOptions, FileAccess, FileAccessError, FileRef, LyricsAnswer};
use crate::js_compat::{js_number, js_string_cmp};
use crate::library_scan::{LibraryError, walk_directory};
use crate::library_store::{LibraryStore, LyricsScope};

const LRC_EXTENSION: &str = ".lrc";

/// The lyrics-fetcher service's convention for a translation sibling file: `<track>.<ISO 639-1 code>.lrc` next to the
/// native `<track>.lrc` - e.g. `Dark Horse ft. Juicy J.en.lrc` alongside `Dark Horse ft. Juicy J.lrc`. Only English is
/// looked up here, per the product decision to show an English translation line specifically, not any available language.
const TRANSLATION_LANGUAGE_CODE: &str = "en";

/// How long a scope scan is reused (see [`LyricsLoader`]).
const SCAN_CACHE_TTL_MS: u64 = 5000;

/// A lyrics file found while walking a scope, tagged with which scope's root id it came from - every configured scope is
/// flattened into one list, so this is the only way a caller can tell which root to read/write a given file through
/// afterwards.
#[derive(Clone, Debug, PartialEq)]
pub struct LyricsFileRef {
    pub file: FileRef,
    pub root_id: String,
}

/// Finds every `.lrc` file under a lyrics scope - the same recursive walk a library scan uses for a music root's
/// audio/playlists, just filtered down to sidecar lyrics files. A scope is a (root, path) pair rather than always a whole
/// root of its own (see [`LyricsScope`]), so this accepts an optional `start_path` to walk from a subfolder of an
/// already-granted root instead of requiring a brand-new OS-level grant just for lyrics.
pub async fn scan_lyrics_root<F: FileAccess>(file_access: &F, root_id: &str, start_path: Option<&str>) -> Result<Vec<FileRef>, LibraryError> {
    let walked = walk_directory(file_access, root_id, start_path, None, None).await?;
    Ok(walked.files.into_iter().filter(|f| f.name.to_lowercase().ends_with(LRC_EXTENSION)).collect())
}

/// `${rootId}:${relativePath}` of every scope, sorted (JavaScript's default order) and joined with `|` - the cache key
/// and the suffix of the persisted index's setting key, so it must match the TypeScript exactly.
pub fn lyrics_scopes_cache_key(scopes: &[LyricsScope]) -> String {
    let mut parts: Vec<String> = scopes.iter().map(|s| format!("{}:{}", s.root_id, s.relative_path)).collect();
    parts.sort_by(|a, b| js_string_cmp(a, b));
    parts.join("|")
}

/// The store setting under which a whole-library match pass writes its authoritative post-walk file list, for
/// [`LyricsLoader::get_lyrics_file_index`] to read back.
pub fn lyrics_file_index_setting_key(scopes: &[LyricsScope]) -> String {
    format!("lyricsFileIndex:{}", lyrics_scopes_cache_key(scopes))
}

/// `JSON.stringify(files)` of the TypeScript's `LyricsFileRef[]`: the file's fields in the adapter's order, then `rootId`.
/// The same setting is read by both implementations, so the shape is a compatibility contract.
pub fn encode_lyrics_file_index(files: &[LyricsFileRef]) -> String {
    let string = |s: &str| serde_json::to_string(s).expect("a string always serializes");
    let items: Vec<String> = files
        .iter()
        .map(|f| {
            format!(
                r#"{{"id":{},"name":{},"relativePath":{},"sizeBytes":{},"lastModifiedMs":{},"rootId":{}}}"#,
                string(&f.file.id),
                string(&f.file.name),
                string(&f.file.relative_path),
                f.file.size_bytes,
                js_number(f.file.last_modified_ms),
                string(&f.root_id),
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// The inverse of [`encode_lyrics_file_index`]. `None` for anything that isn't an array of complete entries - a corrupt
/// or old-shape value (from before `rootId` was recorded, say) is treated as "no index", which makes the caller re-walk
/// and overwrite it. (The TypeScript only fell back on unparseable JSON and trusted any parsed value; being stricter
/// can only cost a re-walk.)
pub fn decode_lyrics_file_index(raw: &str) -> Option<Vec<LyricsFileRef>> {
    let Value::Array(items) = serde_json::from_str::<Value>(raw).ok()? else { return None };
    items
        .iter()
        .map(|v| {
            let text = |k: &str| v.get(k)?.as_str().map(str::to_owned);
            Some(LyricsFileRef {
                file: FileRef { id: text("id")?, name: text("name")?, relative_path: text("relativePath")?, size_bytes: v.get("sizeBytes")?.as_u64()?, last_modified_ms: v.get("lastModifiedMs")?.as_f64()? },
                root_id: text("rootId")?,
            })
        })
        .collect()
}

/// `Track.lrc` -> `Track.en.lrc`; `None` for a name that doesn't end in `.lrc` (shouldn't happen for anything a scope scan
/// returns, but keeps this total).
fn translation_file_name(native_name: &str) -> Option<String> {
    let stem_chars = native_name.chars().count().checked_sub(LRC_EXTENSION.len())?;
    native_name.to_lowercase().ends_with(LRC_EXTENSION).then(|| format!("{}.{TRANSLATION_LANGUAGE_CODE}.lrc", native_name.chars().take(stem_chars).collect::<String>()))
}

/// Resolves, caches and loads a library's lyrics. Create one per file access and reuse it: the scope-scan cache lives in
/// the instance (the TypeScript keyed it on the file access object's identity), so a fresh loader per call would never
/// get a hit.
///
/// Every lyrics-aware call site (loading on every track change, the lyrics screen, the auto-match passes) walks the same
/// small set of scopes, so without a short-lived cache a track-to-track skip re-walked the whole lyrics folder from
/// scratch each time - this collapses repeats within a short window into one real scan. Self-expiring rather than
/// explicitly invalidated: cheap, and a lyrics folder changing on disk is rare enough that a few seconds of staleness
/// right after is an acceptable trade for not tracking every scope add/remove call site.
pub struct LyricsLoader<F, S> {
    file_access: Arc<F>,
    store: Arc<S>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    cache: Mutex<HashMap<String, (Vec<LyricsFileRef>, u64)>>,
}

impl<F: FileAccess, S: LibraryStore> LyricsLoader<F, S> {
    pub fn new(file_access: Arc<F>, store: Arc<S>, now_ms: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self { file_access, store, now_ms: Arc::new(now_ms), cache: Mutex::new(HashMap::new()) }
    }

    pub fn file_access(&self) -> &Arc<F> {
        &self.file_access
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// Every `.lrc` file across every configured lyrics scope - the candidate pool for both auto-match and a manual
    /// picker. This is the REAL, disk-walking scan (cached for a few seconds); the whole-library match pass is the only
    /// caller that should reach for it directly (it then persists the result for everyone else to read back through
    /// [`get_lyrics_file_index`](Self::get_lyrics_file_index)).
    pub async fn scan_all_lyrics_scopes(&self, scopes: &[LyricsScope]) -> Result<Vec<LyricsFileRef>, LibraryError> {
        let key = lyrics_scopes_cache_key(scopes);
        let now = (self.now_ms)();
        if let Some((files, fetched_at)) = self.cache.lock().unwrap().get(&key) {
            if now.saturating_sub(*fetched_at) < SCAN_CACHE_TTL_MS {
                return Ok(files.clone());
            }
        }
        let mut files = Vec::new();
        for scope in scopes {
            let found = scan_lyrics_root(&*self.file_access, &scope.root_id, Some(&scope.relative_path)).await?;
            files.extend(found.into_iter().map(|file| LyricsFileRef { file, root_id: scope.root_id.clone() }));
        }
        self.cache.lock().unwrap().insert(key, (files.clone(), now));
        Ok(files)
    }

    /// The lyrics-file candidate pool, trusting the match pass's already-persisted result instead of re-walking disk.
    /// Before this, every track switch re-walked the whole configured lyrics folder from scratch - on Android each folder
    /// level in that walk is a real storage-provider IPC round-trip, so a library with hundreds of lyrics files made
    /// opening a different track visibly stall on a spinner even though the match pass had already walked the exact same
    /// folder moments earlier at launch. Falls back to a real walk (and persists its own result, so the next caller
    /// doesn't pay for it either) if nothing's been persisted yet - e.g. a scope just added, before the background match
    /// pass has had a chance to run once.
    pub async fn get_lyrics_file_index(&self, scopes: &[LyricsScope]) -> Result<Vec<LyricsFileRef>, LibraryError> {
        let key = lyrics_file_index_setting_key(scopes);
        if let Some(index) = self.store.get_setting(&key).await?.as_deref().filter(|p| !p.is_empty()).and_then(decode_lyrics_file_index) {
            return Ok(index);
        }
        let files = self.scan_all_lyrics_scopes(scopes).await?;
        self.store.put_setting(&key, &encode_lyrics_file_index(&files)).await?;
        Ok(files)
    }

    /// Resolves and parses whatever `.lrc` file is currently assigned to a track, or `None` if none is assigned (or the
    /// assigned file can no longer be found - e.g. its scope was removed since). There's no persisted file reference for an
    /// assigned lrc file, only its id, so this looks it up in the file index.
    ///
    /// The assigned file is always treated as the native/primary language - the lyrics-fetcher service only ever
    /// auto-matches/assigns the plain `<track>.lrc` name, never a `<track>.en.lrc` translation sibling. If a same-named
    /// translation sibling exists and is itself synced, each native line gets that sibling's closest-timestamped line
    /// attached as its `translation` - a translation that turns out to be unsynced plain text is silently ignored, since
    /// there would be no reliable way to line it up per-line.
    pub async fn load_assigned_lyrics(&self, scopes: &[LyricsScope], track_file_id: &str) -> Result<Option<ParsedLyrics>, LibraryError> {
        // A source that already has the lyrics parsed (the server) skips the file hunt entirely.
        match self.file_access.load_lyrics(track_file_id).await? {
            LyricsAnswer::Lyrics(lyrics) => return Ok(Some(lyrics)),
            LyricsAnswer::NoLyrics => return Ok(None),
            LyricsAnswer::NoSay => {}
        }

        let Some(lrc_file_id) = self.store.get_lyrics_assignment(track_file_id).await? else { return Ok(None) };
        let files = self.get_lyrics_file_index(scopes).await?;
        let Some(file) = files.iter().find(|f| f.file.id == lrc_file_id) else { return Ok(None) };

        let native = parse_lrc(&self.file_access.read_file_text(&file.file, CallOptions::default()).await?);

        let translation_name = translation_file_name(&file.file.name).map(|n| n.to_lowercase());
        let translation_file = translation_name.and_then(|name| files.iter().find(|f| f.file.name.to_lowercase() == name));
        let Some(translation_file) = translation_file else { return Ok(Some(native)) };

        let translation = parse_lrc(&self.file_access.read_file_text(&translation_file.file, CallOptions::default()).await?);
        Ok(Some(match_translation_lines(&native, &translation)))
    }

    /// Resolves a track's assigned `.lrc` file to its file reference and the root id it lives under - what the lyrics sync
    /// screen needs to write the hand-synced result back with `write_file_text(root_id, relative_path, ...)`, neither of
    /// which [`load_assigned_lyrics`](Self::load_assigned_lyrics)' parsed lyrics carry.
    pub async fn resolve_assigned_lyrics_file(&self, scopes: &[LyricsScope], track_file_id: &str) -> Result<Option<(String, FileRef)>, LibraryError> {
        let Some(lrc_file_id) = self.store.get_lyrics_assignment(track_file_id).await? else { return Ok(None) };
        let files = self.get_lyrics_file_index(scopes).await?;
        Ok(files.into_iter().find(|f| f.file.id == lrc_file_id).map(|f| (f.root_id, f.file)))
    }

    /// Resolves an auto-match lyrics assignment for a single track, a no-op if it already has one (manual or previously
    /// auto-matched). This is the critical-path counterpart to the whole-library, idle-chunked background pass. Reads the
    /// persisted index rather than walking disk itself, so on any launch after the first this is just a settings read plus
    /// the find/match work, not a real directory walk - important given this runs on startup restore for just the track
    /// being restored.
    pub async fn ensure_lyrics_assignment(&self, scopes: &[LyricsScope], track_file_id: &str, track_file_name: &str) -> Result<(), LibraryError> {
        if scopes.is_empty() || self.store.get_lyrics_assignment(track_file_id).await?.is_some() {
            return Ok(());
        }
        let files = match self.get_lyrics_file_index(scopes).await {
            Ok(files) => files,
            // A lapsed browser grant can't be re-requested off this non-gesture call - skip quietly rather than crash the
            // startup restore path.
            Err(LibraryError::FileAccess(FileAccessError::PermissionPending { .. })) => return Ok(()),
            Err(e) => return Err(e),
        };
        let candidates: Vec<LyricsCandidate> = files.iter().map(|f| LyricsCandidate { file_id: f.file.id.clone(), name: f.file.name.clone() }).collect();
        if let Some(found) = find_auto_lyrics_match(track_file_name, &candidates) {
            self.store.put_lyrics_assignment(track_file_id, Some(found.file_id.clone())).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::file_access::{DirectoryEntry, GrantedRoot, MemoryFileAccess, RootKind};
    use crate::http::block_on;
    use crate::library_store::MemoryLibraryStore;

    const SCOPE: fn() -> LyricsScope = || LyricsScope { root_id: "root".into(), relative_path: String::new() };
    const LRC: &str = "[00:01.00]Hello\n[00:02.00]World";

    fn loader(files: &[(&str, &str)]) -> (LyricsLoader<MemoryFileAccess, MemoryLibraryStore>, Arc<AtomicU64>) {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        (LyricsLoader::new(Arc::new(MemoryFileAccess::new(files.iter().copied())), Arc::new(MemoryLibraryStore::new()), move || c.load(Ordering::SeqCst)), clock)
    }

    fn assign(l: &LyricsLoader<MemoryFileAccess, MemoryLibraryStore>, track: &str, lrc: &str) {
        block_on(l.store().put_lyrics_assignment(track, Some(lrc.into()))).unwrap();
    }

    // ---- scanning and the index ----

    #[test]
    fn combines_candidates_across_every_configured_scope() {
        let (l, _) = loader(&[("a.lrc", LRC), ("b.lrc", LRC), ("song.mp3", "not lyrics"), ("Other/c.LRC", LRC)]);
        let mut names: Vec<_> = block_on(l.scan_all_lyrics_scopes(&[SCOPE()])).unwrap().into_iter().map(|f| f.file.name).collect();
        names.sort();
        assert_eq!(names, ["a.lrc", "b.lrc", "c.LRC"], "recursive and case-insensitive on the extension");
        let only_other = block_on(l.scan_all_lyrics_scopes(&[LyricsScope { root_id: "root".into(), relative_path: "Other".into() }])).unwrap();
        assert_eq!(only_other.len(), 1);
    }

    #[test]
    fn a_scan_is_reused_for_five_seconds_then_redone() {
        let (l, clock) = loader(&[("a.lrc", LRC)]);
        assert_eq!(block_on(l.scan_all_lyrics_scopes(&[SCOPE()])).unwrap().len(), 1);
        l.file_access().add_file("b.lrc", LRC);
        clock.store(1_000 + 4_999, Ordering::SeqCst);
        assert_eq!(block_on(l.scan_all_lyrics_scopes(&[SCOPE()])).unwrap().len(), 1, "still cached");
        clock.store(1_000 + 5_000, Ordering::SeqCst);
        assert_eq!(block_on(l.scan_all_lyrics_scopes(&[SCOPE()])).unwrap().len(), 2, "expired");
    }

    #[test]
    fn the_cache_and_setting_keys_match_the_typescript_format() {
        let scopes = [LyricsScope { root_id: "z".into(), relative_path: "Lyr ics".into() }, LyricsScope { root_id: "a".into(), relative_path: String::new() }];
        assert_eq!(lyrics_scopes_cache_key(&scopes), "a:|z:Lyr ics");
        assert_eq!(lyrics_file_index_setting_key(&scopes), "lyricsFileIndex:a:|z:Lyr ics");
    }

    #[test]
    fn the_first_index_lookup_walks_and_persists_and_the_next_reads_it_back_without_walking() {
        let (l, _) = loader(&[("a.lrc", LRC)]);
        let first = block_on(l.get_lyrics_file_index(&[SCOPE()])).unwrap();
        let stored = block_on(l.store().get_setting("lyricsFileIndex:root:")).unwrap().expect("persisted");
        assert_eq!(stored, r#"[{"id":"a.lrc","name":"a.lrc","relativePath":"a.lrc","sizeBytes":31,"lastModifiedMs":0,"rootId":"root"}]"#);
        l.file_access().remove_file("a.lrc"); // a walk would now find nothing
        assert_eq!(block_on(l.get_lyrics_file_index(&[SCOPE()])).unwrap(), first, "trusted the persisted index");
    }

    #[test]
    fn a_corrupt_or_old_shape_index_is_replaced_by_a_fresh_walk() {
        let (l, _) = loader(&[("a.lrc", LRC)]);
        for bad in ["not json", "{}", r#"[{"id":"x"}]"#, r#"[{"id":"a.lrc","name":"a.lrc","relativePath":"a.lrc","sizeBytes":1,"lastModifiedMs":0}]"#] {
            block_on(l.store().put_setting("lyricsFileIndex:root:", bad)).unwrap();
            let files = block_on(l.get_lyrics_file_index(&[SCOPE()])).unwrap();
            assert_eq!(files.len(), 1, "{bad}");
            assert!(decode_lyrics_file_index(&block_on(l.store().get_setting("lyricsFileIndex:root:")).unwrap().unwrap()).is_some(), "overwritten with a good one");
        }
    }

    #[test]
    fn the_index_json_round_trips_including_fractional_mtimes_and_awkward_names() {
        let files = vec![LyricsFileRef { file: FileRef { id: "L:é \"q\".lrc".into(), name: "é \"q\".lrc".into(), relative_path: "d/é \"q\".lrc".into(), size_bytes: 123, last_modified_ms: 1700000000123.456 }, root_id: "L".into() }];
        assert_eq!(decode_lyrics_file_index(&encode_lyrics_file_index(&files)), Some(files));
        assert_eq!(decode_lyrics_file_index("[]"), Some(vec![]));
    }

    // ---- loadAssignedLyrics ----

    #[test]
    fn load_returns_none_when_the_track_has_no_assignment_or_the_assigned_file_is_gone() {
        let (l, _) = loader(&[]);
        assert_eq!(block_on(l.load_assigned_lyrics(&[SCOPE()], "track-1")).unwrap(), None);
        assign(&l, "track-1", "missing.lrc");
        assert_eq!(block_on(l.load_assigned_lyrics(&[SCOPE()], "track-1")).unwrap(), None);
    }

    #[test]
    fn load_resolves_the_assigned_file_id_and_parses_its_content() {
        let (l, _) = loader(&[("Track One.lrc", LRC)]);
        assign(&l, "track-1", "Track One.lrc");
        let lyrics = block_on(l.load_assigned_lyrics(&[SCOPE()], "track-1")).unwrap().unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.iter().map(|x| (x.time_seconds, x.text.as_str())).collect::<Vec<_>>(), [(Some(1.0), "Hello"), (Some(2.0), "World")]);
    }

    #[test]
    fn load_attaches_a_same_stemmed_en_lrc_sibling_as_each_lines_translation() {
        let (l, _) = loader(&[("Track One.lrc", LRC), ("Track One.EN.lrc", "[00:01.10]Bonjour\n[00:02.10]Monde")]);
        assign(&l, "track-1", "Track One.lrc");
        let lines = block_on(l.load_assigned_lyrics(&[SCOPE()], "track-1")).unwrap().unwrap().lines;
        assert_eq!(lines.iter().map(|x| x.translation.as_deref()).collect::<Vec<_>>(), [Some("Bonjour"), Some("Monde")]);
    }

    #[test]
    fn load_ignores_an_unrelated_en_lrc_file_that_does_not_share_the_stem() {
        let (l, _) = loader(&[("Track One.lrc", LRC), ("Track Two.en.lrc", "[00:01.00]Nope")]);
        assign(&l, "track-1", "Track One.lrc");
        let lines = block_on(l.load_assigned_lyrics(&[SCOPE()], "track-1")).unwrap().unwrap().lines;
        assert!(lines.iter().all(|x| x.translation.is_none()));
    }

    #[test]
    fn a_source_that_has_the_lyrics_ready_made_skips_the_file_hunt() {
        struct Server(MemoryFileAccess, LyricsAnswer);
        impl FileAccess for Server {
            async fn request_root(&self, k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                self.0.request_root(k).await
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                self.0.list_granted_roots().await
            }
            async fn revoke_root(&self, r: &str) -> Result<(), FileAccessError> {
                self.0.revoke_root(r).await
            }
            async fn list_directory(&self, r: &str, p: Option<&str>, o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                self.0.list_directory(r, p, o).await
            }
            async fn read_file_bytes(&self, f: &FileRef, o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
                self.0.read_file_bytes(f, o).await
            }
            async fn read_file_text(&self, f: &FileRef, o: CallOptions) -> Result<String, FileAccessError> {
                self.0.read_file_text(f, o).await
            }
            async fn write_file_text(&self, r: &str, p: &str, c: &str) -> Result<(), FileAccessError> {
                self.0.write_file_text(r, p, c).await
            }
            async fn load_lyrics(&self, _t: &str) -> Result<LyricsAnswer, FileAccessError> {
                Ok(self.1.clone())
            }
        }
        let ready = parse_lrc("[00:05.00]From the server");
        for (answer, expected) in [(LyricsAnswer::Lyrics(ready.clone()), Some(ready)), (LyricsAnswer::NoLyrics, None)] {
            let l = LyricsLoader::new(Arc::new(Server(MemoryFileAccess::new([("Track.lrc", LRC)]), answer)), Arc::new(MemoryLibraryStore::new()), || 0);
            block_on(l.store().put_lyrics_assignment("t", Some("Track.lrc".into()))).unwrap();
            assert_eq!(block_on(l.load_assigned_lyrics(&[SCOPE()], "t")).unwrap(), expected, "the assignment and the files were not consulted");
        }
    }

    #[test]
    fn translation_names_follow_the_stem_rule() {
        assert_eq!(translation_file_name("Track.lrc").as_deref(), Some("Track.en.lrc"));
        assert_eq!(translation_file_name("Dark Horse ft. Juicy J.LRC").as_deref(), Some("Dark Horse ft. Juicy J.en.lrc"));
        assert_eq!(translation_file_name("Track.txt"), None);
        assert_eq!(translation_file_name("lrc"), None);
        assert_eq!(translation_file_name(".lrc").as_deref(), Some(".en.lrc"));
    }

    // ---- resolveAssignedLyricsFile ----

    #[test]
    fn resolve_returns_none_without_an_assignment_and_the_root_id_with_the_file_otherwise() {
        let (l, _) = loader(&[("Track One.lrc", LRC)]);
        assert_eq!(block_on(l.resolve_assigned_lyrics_file(&[SCOPE()], "track-1")).unwrap(), None);
        assign(&l, "track-1", "Track One.lrc");
        let (root, file) = block_on(l.resolve_assigned_lyrics_file(&[SCOPE()], "track-1")).unwrap().unwrap();
        assert_eq!((root.as_str(), file.id.as_str(), file.name.as_str()), ("root", "Track One.lrc", "Track One.lrc"));
    }

    // ---- ensureLyricsAssignment ----

    #[test]
    fn ensure_assigns_a_same_stemmed_lrc_file_and_leaves_an_existing_assignment_alone() {
        let (l, _) = loader(&[("Track One.lrc", LRC), ("Other.lrc", LRC)]);
        block_on(l.ensure_lyrics_assignment(&[SCOPE()], "t1", "Track One.flac")).unwrap();
        assert_eq!(block_on(l.store().get_lyrics_assignment("t1")).unwrap().as_deref(), Some("Track One.lrc"));
        assign(&l, "t2", "Other.lrc");
        block_on(l.ensure_lyrics_assignment(&[SCOPE()], "t2", "Track One.flac")).unwrap();
        assert_eq!(block_on(l.store().get_lyrics_assignment("t2")).unwrap().as_deref(), Some("Other.lrc"), "a manual choice is never overwritten");
    }

    #[test]
    fn ensure_does_nothing_without_scopes_or_without_a_match_or_when_ambiguous() {
        let (l, _) = loader(&[("A.lrc", LRC), ("Dup/A.lrc", LRC)]);
        block_on(l.ensure_lyrics_assignment(&[], "t", "A.mp3")).unwrap();
        block_on(l.ensure_lyrics_assignment(&[SCOPE()], "t", "Nothing.mp3")).unwrap();
        block_on(l.ensure_lyrics_assignment(&[SCOPE()], "t", "A.mp3")).unwrap();
        assert_eq!(block_on(l.store().get_lyrics_assignment("t")).unwrap(), None, "two files share the stem, so no guess");
    }

    #[test]
    fn ensure_quietly_skips_a_lapsed_browser_grant_but_reports_other_failures() {
        struct Lapsed(FileAccessError);
        impl FileAccess for Lapsed {
            async fn request_root(&self, _k: RootKind) -> Result<Option<GrantedRoot>, FileAccessError> {
                Ok(None)
            }
            async fn list_granted_roots(&self) -> Result<Vec<GrantedRoot>, FileAccessError> {
                Ok(vec![])
            }
            async fn revoke_root(&self, _r: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
            async fn list_directory(&self, _r: &str, _p: Option<&str>, _o: CallOptions) -> Result<Vec<DirectoryEntry>, FileAccessError> {
                Err(self.0.clone())
            }
            async fn read_file_bytes(&self, _f: &FileRef, _o: CallOptions) -> Result<Vec<u8>, FileAccessError> {
                Ok(vec![])
            }
            async fn read_file_text(&self, _f: &FileRef, _o: CallOptions) -> Result<String, FileAccessError> {
                Ok(String::new())
            }
            async fn write_file_text(&self, _r: &str, _p: &str, _c: &str) -> Result<(), FileAccessError> {
                Ok(())
            }
        }
        let make = |e| LyricsLoader::new(Arc::new(Lapsed(e)), Arc::new(MemoryLibraryStore::new()), || 0);
        let pending = make(FileAccessError::PermissionPending { root_display_name: "Music".into() });
        assert_eq!(block_on(pending.ensure_lyrics_assignment(&[SCOPE()], "t", "A.mp3")), Ok(()));
        let broken = make(FileAccessError::UnknownRoot("Lyrics".into()));
        assert_eq!(block_on(broken.ensure_lyrics_assignment(&[SCOPE()], "t", "A.mp3")), Err(LibraryError::FileAccess(FileAccessError::UnknownRoot("Lyrics".into()))));
    }
}
