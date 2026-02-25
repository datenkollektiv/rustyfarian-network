//! Pure MQTT primitives — no I/O, no ESP-IDF.

/// Returns the number of 100 ms poll iterations needed to cover `timeout_ms`.
///
/// Uses ceiling division so that a timeout that is not an exact multiple of
/// 100 ms is always fully respected — e.g. 5050 ms yields 51 iterations
/// (5100 ms of polling) rather than 50 (5000 ms).
pub fn connection_wait_iterations(timeout_ms: u64) -> u64 {
    timeout_ms.div_ceil(100)
}

#[cfg(test)]
mod tests {
    use super::connection_wait_iterations;

    #[test]
    fn zero_timeout_yields_zero_iterations() {
        assert_eq!(connection_wait_iterations(0), 0);
    }

    #[test]
    fn exact_multiple_is_not_rounded_up() {
        assert_eq!(connection_wait_iterations(100), 1);
        assert_eq!(connection_wait_iterations(5000), 50);
    }

    #[test]
    fn non_multiple_is_rounded_up() {
        // The edge case from the review: 5050 ms must not be truncated to 50
        assert_eq!(connection_wait_iterations(5050), 51);
        assert_eq!(connection_wait_iterations(5001), 51);
        assert_eq!(connection_wait_iterations(4999), 50);
    }

    #[test]
    fn sub_100ms_timeout_yields_one_iteration() {
        assert_eq!(connection_wait_iterations(1), 1);
        assert_eq!(connection_wait_iterations(99), 1);
    }
}
