//! Port of `store/restore/restoreSlice`: where the startup restore is up to.

/// The fixed, ordered phases the restoring screen shows - defined once so the wording/order can't drift between apps. Deliberately
/// coarse (one row per phase, not per root/lyrics scope scanned within it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RestoringStep {
    ListingFolders,
    ScanningLibrary,
    ScanningLyrics,
    RestoringPlayback,
    LoadingPlaylist,
}

pub const RESTORING_STEPS: [(RestoringStep, &str); 5] = [
    (RestoringStep::ListingFolders, "Listing folders"),
    (RestoringStep::ScanningLibrary, "Scanning library"),
    (RestoringStep::ScanningLyrics, "Scanning lyrics"),
    (RestoringStep::RestoringPlayback, "Restoring playback"),
    (RestoringStep::LoadingPlaylist, "Loading playlist"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct RestoreState {
    /// True for the whole startup restore window, so the UI can hold off rendering its normal screens until the right one is known
    /// (otherwise the library flashes before a restored playlist).
    pub is_restoring: bool,
    pub current_step: Option<RestoringStep>,
    pub completed_steps: Vec<RestoringStep>,
    /// Whether at least one lyrics scope is configured - the restoring screen only shows "Scanning lyrics" when true, so a person
    /// with no lyrics folders doesn't see a row that can never complete.
    pub has_lyrics_scopes: bool,
    /// The person did something that starts/changes playback. The startup restore is a slow chain that can still be in flight
    /// seconds after the screen is interactive; once this is set it must not apply itself, or a person who taps a track early
    /// watches their choice get overwritten with last session's position/track (confirmed on-device on an 858-track library).
    /// Read synchronously by the restore at each of its decision points.
    pub user_took_over: bool,
}

impl Default for RestoreState {
    fn default() -> Self {
        Self { is_restoring: true, current_step: None, completed_steps: Vec::new(), has_lyrics_scopes: false, user_took_over: false }
    }
}

impl RestoreState {
    /// Marks the previously-current step (if any) complete and makes `step` the current one.
    pub fn step_advanced(&mut self, step: RestoringStep) {
        if let Some(current) = self.current_step {
            if !self.completed_steps.contains(&current) {
                self.completed_steps.push(current);
            }
        }
        self.current_step = Some(step);
    }

    pub fn lyrics_scopes_known(&mut self, known: bool) {
        self.has_lyrics_scopes = known;
    }

    pub fn finished(&mut self) {
        self.is_restoring = false;
    }

    pub fn took_over(&mut self) {
        self.user_took_over = true;
    }

    pub fn is_step_completed(&self, step: RestoringStep) -> bool {
        self.completed_steps.contains(&step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_advance_completes_the_previous_step_once() {
        let mut state = RestoreState::default();
        assert!(state.is_restoring);
        state.step_advanced(RestoringStep::ListingFolders);
        assert!(state.completed_steps.is_empty());
        state.step_advanced(RestoringStep::ScanningLibrary);
        state.step_advanced(RestoringStep::ScanningLibrary);
        // Advancing to the step that is already current completes it, exactly as the TypeScript reducer did.
        assert_eq!(state.completed_steps, [RestoringStep::ListingFolders, RestoringStep::ScanningLibrary]);
        assert_eq!(state.current_step, Some(RestoringStep::ScanningLibrary));
        assert!(state.is_step_completed(RestoringStep::ListingFolders));
    }

    #[test]
    fn finishing_and_taking_over_are_recorded() {
        let mut state = RestoreState::default();
        state.took_over();
        state.finished();
        state.lyrics_scopes_known(true);
        assert_eq!((state.user_took_over, state.is_restoring, state.has_lyrics_scopes), (true, false, true));
    }

    #[test]
    fn the_steps_keep_the_documented_order_and_wording() {
        let labels: Vec<&str> = RESTORING_STEPS.iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, ["Listing folders", "Scanning library", "Scanning lyrics", "Restoring playback", "Loading playlist"]);
    }
}
