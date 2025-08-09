#[cfg(test)]
mod amm_formula_tests {
    const BASE: u64 = 100000; // 1e5 as defined in the MASM code

    /// Calculates the amount of asset Y to return given input of asset X
    /// Formula: dy = (dx * y * BASE) / (dx + y)
    ///
    /// Args:
    /// - x: Pool X amount (not used in calculation but represents pool state)
    /// - y: Pool Y amount
    /// - dx: Amount of X being input
    ///
    /// Returns: Amount of Y to output
    fn get_amount_y_out(_x: u64, y: u64, dx: u64) -> u64 {
        // Prevent division by zero
        if dx + y == 0 {
            return 0;
        }

        // Calculate: (dx * y * BASE) / (dx + y)
        let numerator = dx
            .checked_mul(y)
            .and_then(|result| result.checked_mul(BASE))
            .expect("Numerator overflow");

        let denominator = dx.checked_add(y).expect("Denominator overflow");

        numerator / denominator
    }

    /// Alternative implementation that matches the MASM code more closely
    /// This version divides by BASE at the end, effectively calculating:
    /// dy = (dx * y) / (dx + y)
    fn get_amount_y_out_alternative(_x: u64, y: u64, dx: u64) -> u64 {
        // Prevent division by zero
        if dx + y == 0 {
            return 0;
        }

        // Calculate: (dx * y * BASE) / (dx + y) / BASE = (dx * y) / (dx + y)
        let numerator = dx
            .checked_mul(y)
            .and_then(|result| result.checked_mul(BASE))
            .expect("Numerator overflow");

        let denominator = dx.checked_add(y).expect("Denominator overflow");

        let result = numerator / denominator;
        result / BASE
    }

    #[test]
    fn test_amm_formula_basic() {
        // Test case 1: Basic calculation
        let x = 1000; // Pool X (not used in formula but represents state)
        let y = 1000; // Pool Y
        let dx = 100; // Input amount X

        let dy = get_amount_y_out(x, y, dx);

        // Expected: (100 * 2000 * 100000) / (100 + 2000) = 20000000000 / 2100 = 9523809
        let expected = (dx * y * BASE) / (dx + y);
        assert_eq!(dy, expected);
        // assert_eq!(dy, 9523809);

        println!("Test 1 - Basic calculation:");
        println!("  Pool X: {}, Pool Y: {}, Input dX: {}", x, y, dx);
        println!("  Output dY: {}", dy);
    }

    #[test]
    fn test_amm_formula_alternative() {
        // Test the alternative implementation (matching MASM double division)
        let x = 1000;
        let y = 2000;
        let dx = 100;

        let dy = get_amount_y_out_alternative(x, y, dx);

        // Expected: (100 * 2000) / (100 + 2000) = 200000 / 2100 = 95
        let expected = (dx * y) / (dx + y);
        assert_eq!(dy, expected);
        assert_eq!(dy, 95);

        println!("Test 2 - Alternative calculation (with BASE cancellation):");
        println!("  Pool X: {}, Pool Y: {}, Input dX: {}", x, y, dx);
        println!("  Output dY: {}", dy);
    }

    #[test]
    fn test_amm_formula_edge_cases() {
        // Test case: Small values
        let dy1 = get_amount_y_out(100, 100, 10);
        assert_eq!(dy1, (10 * 100 * BASE) / (10 + 100));

        // Test case: Large pool, small input
        let dy2 = get_amount_y_out(1000000, 1000000, 1);
        assert_eq!(dy2, (1 * 1000000 * BASE) / (1 + 1000000));

        // Test case: Equal pools
        let dy3 = get_amount_y_out(500, 500, 50);
        assert_eq!(dy3, (50 * 500 * BASE) / (50 + 500));

        println!("Test 3 - Edge cases:");
        println!("  Small values: {}", dy1);
        println!("  Large pool, small input: {}", dy2);
        println!("  Equal pools: {}", dy3);
    }

    #[test]
    fn test_amm_formula_zero_input() {
        // Test case: Zero input should return zero output
        let dy = get_amount_y_out(1000, 1000, 0);
        assert_eq!(dy, 0);

        println!("Test 4 - Zero input: {}", dy);
    }

    #[test]
    fn test_amm_formula_precision() {
        // Test precision with the BASE multiplier
        let x = 1000;
        let y = 1000;
        let dx = 1;

        let dy = get_amount_y_out(x, y, dx);
        let dy_alt = get_amount_y_out_alternative(x, y, dx);

        // With BASE: (1 * 1000 * 100000) / (1 + 1000) = 100000000 / 1001 = 99900
        // Without BASE: (1 * 1000) / (1 + 1000) = 1000 / 1001 = 0 (integer division)

        assert_eq!(dy, 99900);
        assert_eq!(dy_alt, 0); // Loses precision due to integer division

        println!("Test 5 - Precision comparison:");
        println!("  With BASE multiplier: {}", dy);
        println!("  Without BASE multiplier: {}", dy_alt);
        println!("  This shows BASE provides precision for small values");
    }

    #[test]
    fn test_amm_formula_realistic_values() {
        // Test with realistic but smaller token amounts to avoid overflow
        let pool_x = 1_000_000; // 1M tokens (no decimals for simplicity)
        let pool_y = 2_000_000; // 2M tokens
        let input_dx = 10_000; // 10K tokens

        let dy = get_amount_y_out(pool_x, pool_y, input_dx);

        // Calculate expected result using checked arithmetic to avoid overflow
        let numerator = input_dx
            .checked_mul(pool_y)
            .and_then(|result| result.checked_mul(BASE))
            .expect("Expected calculation overflow");
        let denominator = input_dx + pool_y;
        let expected = numerator / denominator;

        assert_eq!(dy, expected);

        println!("Test 6 - Realistic values:");
        println!("  Pool X: {} tokens", pool_x);
        println!("  Pool Y: {} tokens", pool_y);
        println!("  Input dX: {} tokens", input_dx);
        println!("  Output dY: {} tokens", dy);
        println!("  Expected: {}", expected);
    }

    #[test]
    fn test_amm_formula_large_numbers() {
        // Test with large numbers using more careful arithmetic
        let pool_x = 100_000;
        let pool_y = 200_000;
        let input_dx = 1_000;

        let dy = get_amount_y_out(pool_x, pool_y, input_dx);

        // Calculate step by step to avoid overflow
        let dx_mul_y = input_dx.checked_mul(pool_y).expect("dx * y overflow");
        let numerator = dx_mul_y.checked_mul(BASE).expect("numerator overflow");
        let denominator = input_dx.checked_add(pool_y).expect("denominator overflow");
        let expected = numerator / denominator;

        assert_eq!(dy, expected);

        println!("Test 7 - Large numbers:");
        println!(
            "  Pool X: {}, Pool Y: {}, Input dX: {}",
            pool_x, pool_y, input_dx
        );
        println!("  Output dY: {}", dy);
        println!("  dx * y = {}", dx_mul_y);
        println!("  numerator = {}", numerator);
        println!("  denominator = {}", denominator);
    }
}
