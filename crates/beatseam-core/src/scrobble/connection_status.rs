/// A Last.fm connect attempt that's in flight. `Idle` = no attempt running, so the resting status applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastFmAttemptStatus {
    Idle,
    RequestingToken,
    AwaitingAuthorization,
    Confirming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastFmConnectionStatus {
    Disconnected,
    RequestingToken,
    AwaitingAuthorization,
    Confirming,
    Connected,
}

/// What the Settings UI should show. An attempt in flight wins; otherwise it's derived from whether a session key is
/// stored (an empty string, which a disconnect writes, counts as none).
///
/// Derived rather than held in state because settings hydrate from storage AFTER the connection view model first
/// exists: a status initialised once from the session key at construction always read "disconnected" after a restart,
/// showing the Connect form for an account that was in fact still connected (and pressing Connect overwrote the
/// working session).
pub fn derive_last_fm_status(attempt: LastFmAttemptStatus, session_key: Option<&str>) -> LastFmConnectionStatus {
    match attempt {
        LastFmAttemptStatus::RequestingToken => LastFmConnectionStatus::RequestingToken,
        LastFmAttemptStatus::AwaitingAuthorization => LastFmConnectionStatus::AwaitingAuthorization,
        LastFmAttemptStatus::Confirming => LastFmConnectionStatus::Confirming,
        LastFmAttemptStatus::Idle if session_key.is_some_and(|k| !k.is_empty()) => LastFmConnectionStatus::Connected,
        LastFmAttemptStatus::Idle => LastFmConnectionStatus::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LastFmAttemptStatus as A;
    use LastFmConnectionStatus as S;

    #[test]
    fn is_connected_when_a_session_key_is_stored_and_nothing_is_in_flight() {
        assert_eq!(derive_last_fm_status(A::Idle, Some("sk")), S::Connected);
    }

    #[test]
    fn is_disconnected_with_no_stored_session() {
        assert_eq!(derive_last_fm_status(A::Idle, None), S::Disconnected);
        assert_eq!(derive_last_fm_status(A::Idle, Some("")), S::Disconnected);
    }

    #[test]
    fn follows_the_stored_key_as_settings_hydrate_after_construction() {
        assert_eq!(derive_last_fm_status(A::Idle, None), S::Disconnected);
        assert_eq!(derive_last_fm_status(A::Idle, Some("sk")), S::Connected);
    }

    #[test]
    fn shows_an_attempt_in_flight_regardless_of_the_stored_key() {
        for (attempt, status) in [(A::RequestingToken, S::RequestingToken), (A::AwaitingAuthorization, S::AwaitingAuthorization), (A::Confirming, S::Confirming)] {
            assert_eq!(derive_last_fm_status(attempt, None), status);
            assert_eq!(derive_last_fm_status(attempt, Some("sk")), status);
        }
    }
}
