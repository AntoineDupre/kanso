/// Fractional ordering shared by task columns and checklists (PLAN §4).
///
/// Values are `REAL`; new entries append at `max + 1` or insert at a midpoint.
/// When two neighbours get closer than [`MIN_GAP`], callers renumber the whole
/// column with [`renumber`] inside the same transaction.
pub const MIN_GAP: f64 = 1e-6;

/// Rank for a new trailing entry.
pub fn append(max: Option<f64>) -> f64 {
    max.map_or(1.0, |value| value + 1.0)
}

/// Rank for a new leading entry.
pub fn prepend(min: Option<f64>) -> f64 {
    min.map_or(1.0, |value| value - 1.0)
}

/// Midpoint between two ranks, or `None` when the gap is too small (the caller
/// must renumber) or the bounds are not strictly increasing.
///
/// A gap of at least `2 * MIN_GAP` is required so the inserted value keeps at
/// least [`MIN_GAP`] to both neighbours, matching PLAN §4's renumber trigger.
pub fn between(lower: f64, upper: f64) -> Option<f64> {
    if !lower.is_finite() || !upper.is_finite() {
        return None;
    }
    let gap = upper - lower;
    if gap < 2.0 * MIN_GAP {
        return None;
    }
    let midpoint = lower + gap / 2.0;
    (midpoint > lower && midpoint < upper).then_some(midpoint)
}

/// Evenly spaced ranks `1.0 ..= count as f64` used by renormalisation.
pub fn renumber(count: usize) -> impl Iterator<Item = f64> {
    (1..=count).map(|index| index as f64)
}

/// True when two neighbours cannot accept another midpoint.
pub fn needs_renumber(lower: f64, upper: f64) -> bool {
    between(lower, upper).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_starts_at_one_and_grows_by_one() {
        assert_eq!(append(None), 1.0);
        assert_eq!(append(Some(1.0)), 2.0);
        assert_eq!(append(Some(41.5)), 42.5);
    }

    #[test]
    fn prepend_starts_at_one_and_shrinks_by_one() {
        assert_eq!(prepend(None), 1.0);
        assert_eq!(prepend(Some(1.0)), 0.0);
        assert_eq!(prepend(Some(-2.0)), -3.0);
    }

    #[test]
    fn between_returns_the_midpoint() {
        assert_eq!(between(1.0, 2.0), Some(1.5));
        assert_eq!(between(1.0, 3.0), Some(2.0));
        assert_eq!(between(-1.0, 1.0), Some(0.0));
    }

    #[test]
    fn between_refuses_tiny_gaps_and_bad_bounds() {
        assert_eq!(between(1.0, 1.0 + MIN_GAP / 2.0), None);
        assert_eq!(between(1.0, 1.0 + MIN_GAP), None);
        assert!(needs_renumber(1.0, 1.0 + MIN_GAP / 2.0));

        assert_eq!(between(2.0, 1.0), None);
        assert_eq!(between(1.0, 1.0), None);
        assert_eq!(between(f64::NAN, 1.0), None);
        assert_eq!(between(1.0, f64::INFINITY), None);
    }

    #[test]
    fn renumber_is_one_based_and_evenly_spaced() {
        let values: Vec<f64> = renumber(4).collect();
        assert_eq!(values, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(renumber(0).count(), 0);

        // A full renumber always restores room for midpoints.
        for pair in values.windows(2) {
            assert!(between(pair[0], pair[1]).is_some(), "{pair:?}");
        }
    }

    #[test]
    fn repeated_midpoints_eventually_require_renumbering() {
        let lower = 1.0;
        let mut upper = 2.0;
        let mut steps = 0;
        while let Some(midpoint) = between(lower, upper) {
            upper = midpoint;
            steps += 1;
            assert!(steps < 100, "midpoint range should collapse");
        }
        assert!(needs_renumber(lower, upper));
    }
}
