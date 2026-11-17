#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Before,
    After,
}

/// `ids` with `moving` taken out and re-inserted, in the order given, before or after `anchor`. `None` if the
/// anchor isn't in the list once the moving tracks are removed (nothing sensible to do).
pub fn move_within<S: AsRef<str>>(ids: &[S], moving: &[S], anchor: &str, position: Position) -> Option<Vec<String>> {
    let remaining: Vec<&str> = ids
        .iter()
        .map(AsRef::as_ref)
        .filter(|id| !moving.iter().any(|m| m.as_ref() == *id))
        .collect();
    let anchor_index = remaining.iter().position(|id| *id == anchor)?;
    let insert_at = match position {
        Position::Before => anchor_index,
        Position::After => anchor_index + 1,
    };
    Some(
        remaining[..insert_at]
            .iter()
            .copied()
            .chain(moving.iter().map(AsRef::as_ref))
            .chain(remaining[insert_at..].iter().copied())
            .map(str::to_owned)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [&str; 5] = ["a", "b", "c", "d", "e"];

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn moves_a_track_before_or_after_an_anchor() {
        assert_eq!(move_within(&IDS, &["a"], "d", Position::After), Some(v(&["b", "c", "d", "a", "e"])));
        assert_eq!(move_within(&IDS, &["e"], "b", Position::Before), Some(v(&["a", "e", "b", "c", "d"])));
    }

    #[test]
    fn keeps_several_moved_tracks_together_in_the_order_given() {
        assert_eq!(move_within(&IDS, &["d", "b"], "e", Position::After), Some(v(&["a", "c", "e", "d", "b"])));
    }

    #[test]
    fn gives_nothing_when_the_anchor_is_one_of_the_moving_tracks_or_unknown() {
        assert_eq!(move_within(&IDS, &["a", "b"], "b", Position::After), None);
        assert_eq!(move_within(&IDS, &["a"], "zzz", Position::Before), None);
    }

    #[test]
    fn accepts_owned_strings_too() {
        let ids = v(&IDS);
        assert_eq!(move_within(&ids, &v(&["a"]), "c", Position::After), Some(v(&["b", "c", "a", "d", "e"])));
    }
}
