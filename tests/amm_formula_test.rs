//! Pure-Rust tests of the reference math in `common.rs`. These mirror the MASM formulas
//! exactly, so they double as a specification for `amm.masm` / `liquidity.masm`.

use miden_amm::common::{
    FEE_DENOM, MIN_LIQUIDITY, quote_initial_lp, quote_lp_mint, quote_remove_liquidity,
    quote_swap_output,
};

#[test]
fn zero_fee_reduces_to_constant_product() {
    // dy = dx*y/(x+dx) when fee = 0
    let (dx, x, y) = (1_000u64, 100_000u64, 400_000u64);
    let expected = ((dx as u128) * (y as u128) / ((x + dx) as u128)) as u64;
    assert_eq!(quote_swap_output(dx, x, y, 0), expected);
}

#[test]
fn fee_reduces_output() {
    let (dx, x, y) = (30_000u64, 150_000u64, 600_000u64);
    let no_fee = quote_swap_output(dx, x, y, 0);
    let with_fee = quote_swap_output(dx, x, y, 30);
    assert!(with_fee < no_fee);
    // 0.3% fee should cost roughly 0.3% of output
    let diff = no_fee - with_fee;
    assert!(diff <= no_fee * 4 / 1000, "fee took too much: {diff} of {no_fee}");
}

#[test]
fn swap_preserves_constant_product_invariant() {
    // With a fee, k = x*y must strictly grow after a swap.
    let (mut x, mut y) = (150_000u64, 600_000u64);
    let k_before = (x as u128) * (y as u128);
    let dx = 30_000u64;
    let dy = quote_swap_output(dx, x, y, 30);
    x += dx;
    y -= dy;
    let k_after = (x as u128) * (y as u128);
    assert!(k_after > k_before, "constant product must not decrease");
}

#[test]
fn max_fee_gives_zero_output() {
    assert_eq!(quote_swap_output(1_000, 100_000, 400_000, FEE_DENOM), 0);
}

#[test]
fn initial_lp_is_sqrt_minus_minimum() {
    // sqrt(100_000 * 400_000) = sqrt(4e10) = 200_000
    let (minted, supply) = quote_initial_lp(100_000, 400_000);
    assert_eq!(supply, 200_000);
    assert_eq!(minted, 200_000 - MIN_LIQUIDITY);
}

#[test]
#[should_panic(expected = "initial deposit too small")]
fn tiny_initial_deposit_panics() {
    // sqrt(100*100) = 100 <= MIN_LIQUIDITY
    quote_initial_lp(100, 100);
}

#[test]
fn proportional_deposit_mints_proportional_lp() {
    // Depositing 50% of reserves mints 50% of supply.
    let (x, y, s) = (100_000u64, 400_000u64, 200_000u64);
    let lp = quote_lp_mint(50_000, 200_000, x, y, s);
    assert_eq!(lp, s / 2);
}

#[test]
fn unbalanced_deposit_mints_minimum_side() {
    // The overpaid side is donated to the pool (Uniswap v2 semantics).
    let (x, y, s) = (100_000u64, 400_000u64, 200_000u64);
    let balanced = quote_lp_mint(50_000, 200_000, x, y, s);
    let unbalanced = quote_lp_mint(50_000, 300_000, x, y, s);
    assert_eq!(balanced, unbalanced);
}

#[test]
fn remove_liquidity_round_trip() {
    let (x, y, s) = (150_000u64, 600_000u64, 300_000u64);
    let (ax, ay) = quote_remove_liquidity(100_000, x, y, s);
    assert_eq!(ax, 50_000);
    assert_eq!(ay, 200_000);
    // burning the whole supply drains the pool
    let (ax, ay) = quote_remove_liquidity(s, x, y, s);
    assert_eq!((ax, ay), (x, y));
}

#[test]
fn lps_earn_swap_fees() {
    // A passive LP's redeemable value (in both assets) grows after fee-charging swaps.
    let (mut x, mut y) = (100_000u64, 400_000u64);
    let supply = 200_000u64;
    let lp_position = 50_000u64;

    let (ax0, ay0) = quote_remove_liquidity(lp_position, x, y, supply);

    // run a round-trip swap (X->Y then Y->X) with a 0.3% fee
    let dy = quote_swap_output(10_000, x, y, 30);
    x += 10_000;
    y -= dy;
    let dx_back = quote_swap_output(dy, y, x, 30);
    y += dy;
    x -= dx_back;

    let (ax1, ay1) = quote_remove_liquidity(lp_position, x, y, supply);
    // y reserve is unchanged after the round trip, x reserve grew by the fees
    assert!(ax1 > ax0);
    assert!(ay1 >= ay0);
}
