use super::used_percent;
use pretty_assertions::assert_eq;

#[test]
fn compaction_percentage_rounds_up_and_clamps_without_overflow() {
    for (used, limit, expected) in [
        (0, 500_000, 0),
        (1, 500_000, 1),
        (250_000, 500_000, 50),
        (250_001, 500_000, 51),
        (499_999, 500_000, 100),
        (500_000, 500_000, 100),
        (600_000, 500_000, 100),
        (-1, 500_000, 0),
        (0, 0, 100),
        (0, -1, 100),
        (i64::MAX, i64::MAX, 100),
        (i64::MAX / 2, i64::MAX, 50),
        (i64::MAX / 2 + 1, i64::MAX, 51),
    ] {
        assert_eq!(used_percent(used, limit), expected, "{used}/{limit}");
    }
}
