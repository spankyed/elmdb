//! The sum of a set's numbers: the number nearest to it, whatever order the numbers come in.
//!
//! Added one by one, each addition rounds, the roundings add up, and which they are depends on the order: ten 0.1s
//! come to 0.9999999999999999, and a 1 between 1e16 and its opposite is lost whole. Here the sum is kept exactly, as
//! a few numbers that share no digits (Shewchuk's expansion: what an addition rounds off is a number too, and is kept
//! beside the result), and is rounded once, when it is asked for, to the nearest number, a tie to the even one. So
//! the answer is the same for every order of the set.
//!
//! The one thing an order can still change is a refusal. A number is added to the parts from the smallest, and a sum
//! is refused where one of those additions passes what a number holds: where the sum so far is past it, though the
//! numbers after it would have brought it back, and in one corner short of it, the largest number there is added to
//! a sum that is its opposite to within its last digit, whose small part it meets first.

/// A sum kept exactly: numbers from the smallest to the largest in size, no two sharing a digit, whose sum is the sum
/// of everything added
#[derive(Default)]
pub(super) struct Sum {
    parts: Vec<f64>,
}

impl Sum {
    /// Adds a number. False when an addition it makes passes what a number holds, and nothing more is to be added
    pub(super) fn add(&mut self, mut x: f64) -> bool {
        // `x` is added to each part in turn, from the smallest: the result goes on to the next, and what the
        // addition rounded off, if anything, stays as a part. The parts never number more than the additions made
        let mut kept = 0;
        for at in 0..self.parts.len() {
            let mut y = self.parts[at];
            // mutant-skip: replace < with <= in Sum::add
            // Two of one size are either way round: the larger of the two is taken out of their sum to leave the
            // smaller, and they are each other's
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let sum = x + y;
            // What the addition lost of the smaller: exact, since the larger subtracted from the sum is
            let lost = y - (sum - x);
            if lost != 0.0 {
                self.parts[kept] = lost;
                kept += 1;
            }
            x = sum;
        }
        self.parts.truncate(kept);
        self.parts.push(x);
        x.is_finite()
    }

    /// The sum, rounded to the nearest number, a tie to the even one
    pub(super) fn total(&self) -> f64 {
        let mut parts = self.parts.iter().rev();
        // From the largest part down, until a part doesn't fit whole: the rest are smaller than what was lost of it
        let mut total = parts.next().copied().unwrap_or(0.0);
        let mut lost = 0.0;
        for &part in parts.by_ref() {
            let larger = total;
            total = larger + part;
            lost = part - (total - larger);
            if lost != 0.0 {
                break;
            }
        }
        // What was lost is half a step exactly when the addition met a tie, which it broke to the even number without
        // the parts below. One of them on the far side of the tie makes the sum nearer the odd number
        if let Some(&below) = parts.next()
            // mutant-skip: replace < with <= in Sum::total
            // mutant-skip: replace > with >= in Sum::total
            // A part is never zero but the largest, and what was lost is zero only when no part is left below
            && ((lost < 0.0 && below < 0.0) || (lost > 0.0 && below > 0.0))
        {
            let twice = lost * 2.0;
            let other = total + twice;
            // Twice what was lost fits whole only where it is a whole step: the tie
            if twice == other - total {
                total = other;
            }
        }
        // No digits and a sign is zero: of nothing, or of zeros that were all below zero
        total + 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::Sum;

    fn sum(numbers: &[f64]) -> Option<f64> {
        let mut sum = Sum::default();
        numbers.iter().all(|&x| sum.add(x)).then(|| sum.total())
    }

    #[test]
    fn a_sum_is_the_nearest_number_to_what_adding_one_by_one_rounds_away() {
        assert_eq!(sum(&[0.1; 10]), Some(1.0));
        assert_eq!(sum(&[1e16, 1.0, -1e16]), Some(1.0));
        assert_eq!(sum(&[1.0, 1e16, -1e16]), Some(1.0));
        // What compensating for each addition's error alone loses: an error lost among the errors
        assert_eq!(sum(&[1e17, 1.0, 1e-17, -1.0, -1e17]), Some(1e-17));
        assert_eq!(sum(&[1e17, 1.0, -1.0, -1e17, 1e-17]), Some(1e-17));
        // Whole numbers past where every one has a number of its own
        let two_to_53 = 9_007_199_254_740_992.0;
        assert_eq!(sum(&[two_to_53, 1.0, 1.0]), Some(two_to_53 + 2.0));
        assert_eq!(sum(&[]), Some(0.0));
    }

    #[test]
    fn a_tie_goes_to_the_even_number_unless_a_smaller_part_says_which_side() {
        let two_to_53 = 9_007_199_254_740_992.0;
        let tiny = 2f64.powi(-53);
        // Halfway between two numbers: the even one
        assert_eq!(sum(&[two_to_53, 1.0]), Some(two_to_53));
        assert_eq!(sum(&[two_to_53, 3.0]), Some(two_to_53 + 4.0));
        // Past halfway by the least there is, and short of it: the far number, and the near one
        assert_eq!(sum(&[two_to_53, 1.0, tiny]), Some(two_to_53 + 2.0));
        assert_eq!(sum(&[two_to_53, 1.0, -tiny]), Some(two_to_53));
        assert_eq!(sum(&[-two_to_53, -1.0, -tiny]), Some(-two_to_53 - 2.0));
        assert_eq!(sum(&[-two_to_53, -1.0, tiny]), Some(-two_to_53));
        // And where the tie was broken upwards, a part below it brings the sum back down
        assert_eq!(sum(&[two_to_53, 3.0, -tiny]), Some(two_to_53 + 2.0));
        assert_eq!(sum(&[two_to_53, 3.0, tiny]), Some(two_to_53 + 4.0));
    }

    #[test]
    fn a_sum_of_zeros_is_zero_and_a_sum_past_what_a_number_holds_is_none() {
        assert_eq!(sum(&[-0.0]).map(f64::to_bits), Some(0));
        assert_eq!(sum(&[-0.0, -0.0]).map(f64::to_bits), Some(0));
        assert_eq!(sum(&[1.5, -1.5]).map(f64::to_bits), Some(0));
        assert_eq!(sum(&[f64::MAX, f64::MAX]), None);
        // Past it on the way, though the last would bring it back
        assert_eq!(sum(&[1e308, 1e308, -1e308]), None);
        assert_eq!(sum(&[-1e308, -1e308, 1e308]), None);
        // The largest number and half a step more is a tie the even number wins, which is past the largest
        assert_eq!(sum(&[f64::MAX, 2f64.powi(969), 2f64.powi(969)]), None);
        assert_eq!(sum(&[f64::MAX, 2f64.powi(969)]), Some(f64::MAX));
        // A quarter of a step and a little more is short of the tie, though twice what was lost added on passes
        assert_eq!(sum(&[f64::MAX, 2f64.powi(969), 1.0]), Some(f64::MAX));
        // The corner short of it: no sum on the way is past the largest number, and the largest, added last, meets
        // the half step before it meets its opposite
        let short_of_the_largest = -f64::MAX + 2f64.powi(971);
        assert_eq!(sum(&[short_of_the_largest, 2f64.powi(970), f64::MAX]), None);
        assert_eq!(sum(&[f64::MAX, short_of_the_largest, 2f64.powi(970)]), Some(3.0 * 2f64.powi(970)));
    }

    #[test]
    fn a_sum_is_what_whole_number_arithmetic_makes_it_in_every_order() {
        // Numbers that are whole multiples of 2^-55, of very different sizes, so that their sum as a whole number of
        // 2^-55s fits a 128-bit integer, whose conversion to a float rounds to the nearest, a tie to the even
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        let unit = 2f64.powi(-55);
        for round in 0..4000 {
            let count = 1 + next() as usize % 40;
            let mut numbers = Vec::with_capacity(count);
            let mut exact = 0i128;
            for _ in 0..count {
                // Up to eleven digits, at any of 111 places, of either sign; every fourth round's are one digit or
                // two, whose sums meet a tie more often
                let digits = if round % 4 == 0 { 1 + (next() % 2) as i128 } else { 1 + (next() % 2047) as i128 };
                let place = (next() % 111) as u32;
                let whole = if next() % 2 == 0 { digits << place } else { -(digits << place) };
                exact += whole;
                numbers.push(whole as f64 * unit);
            }
            let expected = exact as f64 * unit;
            assert_eq!(sum(&numbers), Some(expected), "{numbers:?}");
            numbers.reverse();
            assert_eq!(sum(&numbers), Some(expected), "{numbers:?}, from the last");
            numbers.sort_by(|a, b| a.abs().total_cmp(&b.abs()));
            assert_eq!(sum(&numbers), Some(expected), "{numbers:?}, from the smallest");
        }
    }
}
