/// Fisher-Yates shuffle. Returns a new vector (never mutates the input) so callers can freely hold onto the original order
/// alongside the shuffled one. `random` yields numbers in `[0, 1)` and is injectable so tests can assert an exact
/// permutation instead of just "still a permutation".
pub fn fisher_yates_shuffle<T: Clone>(items: &[T], random: &mut dyn FnMut() -> f64) -> Vec<T> {
    let mut result = items.to_vec();
    for i in (1..result.len()).rev() {
        let j = (random() * (i + 1) as f64).floor() as usize;
        result.swap(i, j.min(i));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_mutate_the_input() {
        let input = [1, 2, 3, 4, 5];
        let copy = input;
        fisher_yates_shuffle(&input, &mut || 0.0);
        assert_eq!(input, copy);
    }

    #[test]
    fn returns_a_permutation_containing_every_element_exactly_once() {
        let input: Vec<i32> = (0..20).collect();
        let mut state = 12345u64;
        let mut random = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut shuffled = fisher_yates_shuffle(&input, &mut random);
        assert_ne!(shuffled, input, "twenty elements are overwhelmingly unlikely to stay in order");
        shuffled.sort();
        assert_eq!(shuffled, input);
    }

    #[test]
    fn produces_a_deterministic_result_for_a_fixed_random_source() {
        // random() always returning 0 means every swap targets index 0.
        assert_eq!(fisher_yates_shuffle(&[1, 2, 3, 4], &mut || 0.0), [2, 3, 4, 1]);
    }

    #[test]
    fn handles_empty_and_single_element_arrays() {
        assert_eq!(fisher_yates_shuffle::<i32>(&[], &mut || 0.5), Vec::<i32>::new());
        assert_eq!(fisher_yates_shuffle(&[1], &mut || 0.5), [1]);
    }

    #[test]
    fn a_random_source_at_the_top_of_its_range_stays_in_bounds() {
        assert_eq!(fisher_yates_shuffle(&[1, 2, 3], &mut || 0.999_999_999_999).len(), 3);
    }
}
