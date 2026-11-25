
/// Calculates updated proposal tallies when a voter changes
/// their vote from one direction to the other.
///
/// Returns None if the weight is invalid, the vote direction
/// has not changed, or an arithmetic operation overflows.
pub fn calculate_revised_tally(
    for_votes: i128,
    against_votes: i128,
    weight: i128,
    previous_support: bool,
    new_support: bool,
) -> Option<(i128, i128)> {
    if weight <= 0 || previous_support == new_support {
        return None;
    }

    let (updated_for, updated_against) = if previous_support {
        (
            for_votes.checked_sub(weight)?,
            against_votes.checked_add(weight)?,
        )
    } else {
        (
            for_votes.checked_add(weight)?,
            against_votes.checked_sub(weight)?,
        )
    };

    if updated_for < 0 || updated_against < 0 {
        return None;
    }

    Some((updated_for, updated_against))
}

/// Safely increments a voter's vote revision count.
pub fn increment_revision_count(current: u32) -> Option<u32> {
    current.checked_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_weight_from_for_to_against() {
        assert_eq!(
            calculate_revised_tally(100, 50, 25, true, false),
            Some((75, 75))
        );
    }

    #[test]
    fn moves_weight_from_against_to_for() {
        assert_eq!(
            calculate_revised_tally(100, 50, 20, false, true),
            Some((120, 30))
        );
    }

    #[test]
    fn rejects_same_vote_direction() {
        assert_eq!(
            calculate_revised_tally(100, 50, 20, true, true),
            None
        );
    }

    #[test]
    fn rejects_invalid_weight() {
        assert_eq!(
            calculate_revised_tally(100, 50, 0, true, false),
            None
        );
    }

    #[test]
    fn increments_revision_count() {
        assert_eq!(increment_revision_count(2), Some(3));
    }
}
