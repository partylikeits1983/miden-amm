//! Offline MockChain tests for the Uniswap-v2-style AMM. These execute the real MASM
//! against the real transaction kernel, so they are the primary validation of
//! `amm.masm` / `liquidity.masm` before anything touches the testnet.

use anyhow::Result;
use miden_amm::common::{
    AmmBuild, MIN_LIQUIDITY, PayoutInfo, build_amm_account, create_add_liquidity_note,
    create_remove_liquidity_note, create_swap_note, lp_supply_slot, quote_initial_lp,
    quote_lp_mint, quote_remove_liquidity, quote_swap_output,
};
use miden_client::{
    Felt, Word,
    account::Account,
    asset::FungibleAsset,
    auth::AuthSchemeId,
    note::Note,
    transaction::RawOutputNote,
};
use miden_testing::{Auth, MockChain};

const FEE_BPS: u64 = 30;

fn auth() -> Auth {
    Auth::BasicAuth {
        auth_scheme: AuthSchemeId::Falcon512Poseidon2,
    }
}

fn serial(n: u64) -> Word {
    [
        Felt::new_unchecked(n),
        Felt::new_unchecked(n + 101),
        Felt::new_unchecked(n + 202),
        Felt::new_unchecked(n + 303),
    ]
    .into()
}

fn lp_supply_of(account: &Account) -> u64 {
    let word: Word = account
        .storage()
        .get_item(&lp_supply_slot())
        .expect("lp_supply slot exists")
        .into();
    word[0].as_canonical_u64()
}

/// Executes one AMM note on the mock chain, asserts the transaction actually produced the
/// expected payout note (NoteId commits to recipient, metadata AND assets, so id equality
/// is full content equality), applies the delta to the local AMM account state and seals
/// a block.
async fn consume_amm_note(
    mock_chain: &mut MockChain,
    amm_account: &mut Account,
    note: &Note,
    expected_payout: Note,
) -> Result<()> {
    let expected_id = expected_payout.id();
    let ctx = mock_chain
        .build_tx_context(amm_account.id(), &[note.id()], &[])?
        .extend_expected_output_notes(vec![RawOutputNote::Full(expected_payout)])
        .build()?;
    let executed = ctx.execute().await?;
    let produced: Vec<_> = executed.output_notes().iter().map(|n| n.id()).collect();
    anyhow::ensure!(
        produced.contains(&expected_id),
        "expected payout note {expected_id:?} not among produced output notes {produced:?}"
    );
    amm_account.apply_delta(&executed.account_delta())?;
    mock_chain.add_pending_executed_transaction(&executed)?;
    mock_chain.prove_next_block()?;
    Ok(())
}

/// MASM compile gate: building the AMM account assembles both components and all three
/// note scripts plus the deploy script. Run this first when debugging assembly errors.
#[test]
fn amm_masm_compiles() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_x = builder.add_existing_basic_faucet(auth(), "TKX", 1_000_000_000, Some(8))?;
    let faucet_y = builder.add_existing_basic_faucet(auth(), "TKY", 1_000_000_000, Some(8))?;

    let build = build_amm_account([7u8; 32], faucet_x.id(), faucet_y.id(), FEE_BPS, true)?;
    assert_eq!(lp_supply_of(&build.account), 0);
    Ok(())
}

/// Full Uniswap-v2 lifecycle, executed by the transaction kernel:
/// first add_liquidity (sqrt initial mint + locked minimum), second add (pro-rata),
/// a fee-charging swap, and a remove_liquidity whose payout proves fee accrual.
#[tokio::test]
async fn uniswap_v2_lifecycle() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_x = builder.add_existing_basic_faucet(auth(), "TKX", 1_000_000_000, Some(8))?;
    let faucet_y = builder.add_existing_basic_faucet(auth(), "TKY", 1_000_000_000, Some(8))?;
    let alice = builder.add_existing_wallet_with_assets(auth(), [])?;

    let build: AmmBuild = build_amm_account([7u8; 32], faucet_x.id(), faucet_y.id(), FEE_BPS, true)?;
    let amm_id = build.account.id();
    builder.add_account(build.account.clone())?;

    // ---- craft all notes up front (amounts are predetermined) ----------------------------

    // 1) first deposit: 100_000 X + 400_000 Y  ->  supply 200_000, minted 199_000
    let (lp1_minted, supply1) = quote_initial_lp(100_000, 400_000);
    assert_eq!((lp1_minted, supply1), (199_000, 200_000));
    let add1_payout = PayoutInfo::new(alice.id(), serial(1000));
    let add1_note = create_add_liquidity_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 100_000)?,
        FungibleAsset::new(faucet_y.id(), 400_000)?,
        lp1_minted,
        &add1_payout,
        build.add_liquidity_note_script.clone(),
        serial(1),
    )?;

    // 2) second deposit: 50_000 X + 200_000 Y  ->  pro-rata mint of 100_000 LP
    let lp2_minted = quote_lp_mint(50_000, 200_000, 100_000, 400_000, supply1);
    assert_eq!(lp2_minted, 100_000);
    let add2_payout = PayoutInfo::new(alice.id(), serial(2000));
    let add2_note = create_add_liquidity_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 50_000)?,
        FungibleAsset::new(faucet_y.id(), 200_000)?,
        lp2_minted,
        &add2_payout,
        build.add_liquidity_note_script.clone(),
        serial(2),
    )?;

    // 3) swap 30_000 X -> Y against reserves (150_000, 600_000)
    let dy = quote_swap_output(30_000, 150_000, 600_000, FEE_BPS);
    assert!(dy > 0 && dy < 600_000);
    let swap_payout = PayoutInfo::new(alice.id(), serial(3000));
    let swap_note = create_swap_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 30_000)?,
        faucet_y.id(),
        dy,
        &swap_payout,
        build.swap_note_script.clone(),
        serial(3),
    )?;

    // 4) burn 100_000 LP against reserves (180_000, 600_000 - dy), supply 300_000
    let supply2 = supply1 + lp2_minted;
    let (ax, ay) = quote_remove_liquidity(100_000, 180_000, 600_000 - dy, supply2);
    let remove_payout = PayoutInfo::new(alice.id(), serial(4000));
    let remove_note = create_remove_liquidity_note(
        alice.id(),
        amm_id,
        100_000,
        ax,
        ay,
        &remove_payout,
        build.remove_liquidity_note_script.clone(),
        serial(4),
    )?;

    for note in [&add1_note, &add2_note, &swap_note, &remove_note] {
        builder.add_output_note(RawOutputNote::Full((*note).clone()));
    }

    let mut mock_chain = builder.build()?;
    let mut amm_account = build.account.clone();

    // ---- execute -------------------------------------------------------------------------

    // first add_liquidity
    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &add1_note,
        add1_payout.expected_note(amm_id, vec![FungibleAsset::new(amm_id, lp1_minted)?])?,
    )
    .await?;
    assert_eq!(lp_supply_of(&amm_account), supply1);

    // second add_liquidity
    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &add2_note,
        add2_payout.expected_note(amm_id, vec![FungibleAsset::new(amm_id, lp2_minted)?])?,
    )
    .await?;
    assert_eq!(lp_supply_of(&amm_account), supply2);

    // swap
    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &swap_note,
        swap_payout.expected_note(amm_id, vec![FungibleAsset::new(faucet_y.id(), dy)?])?,
    )
    .await?;
    assert_eq!(lp_supply_of(&amm_account), supply2, "swap must not change LP supply");

    // remove_liquidity: the LP burned here was minted by the proportional second deposit
    // (50_000 X). Getting more X back than was deposited proves swap fees accrued to LPs.
    assert!(ax > 50_000, "LP payout must include accrued swap fees: {ax}");
    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &remove_note,
        remove_payout.expected_note(
            amm_id,
            vec![
                FungibleAsset::new(faucet_x.id(), ax)?,
                FungibleAsset::new(faucet_y.id(), ay)?,
            ],
        )?,
    )
    .await?;
    assert_eq!(lp_supply_of(&amm_account), supply2 - 100_000);

    // the locked MIN_LIQUIDITY can never be withdrawn
    assert!(lp_supply_of(&amm_account) >= MIN_LIQUIDITY);
    Ok(())
}

/// A swap whose min_amount_out is higher than the quote must be rejected by the kernel.
#[tokio::test]
async fn swap_slippage_reverts() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_x = builder.add_existing_basic_faucet(auth(), "TKX", 1_000_000_000, Some(8))?;
    let faucet_y = builder.add_existing_basic_faucet(auth(), "TKY", 1_000_000_000, Some(8))?;
    let alice = builder.add_existing_wallet_with_assets(auth(), [])?;

    let build = build_amm_account([7u8; 32], faucet_x.id(), faucet_y.id(), FEE_BPS, true)?;
    let amm_id = build.account.id();
    builder.add_account(build.account.clone())?;

    let (lp1, _) = quote_initial_lp(100_000, 400_000);
    let add_payout = PayoutInfo::new(alice.id(), serial(1000));
    let add_note = create_add_liquidity_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 100_000)?,
        FungibleAsset::new(faucet_y.id(), 400_000)?,
        lp1,
        &add_payout,
        build.add_liquidity_note_script.clone(),
        serial(1),
    )?;

    let dy = quote_swap_output(30_000, 100_000, 400_000, FEE_BPS);
    let swap_payout = PayoutInfo::new(alice.id(), serial(3000));
    let greedy_swap_note = create_swap_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 30_000)?,
        faucet_y.id(),
        dy + 1, // demands more than the pool will pay
        &swap_payout,
        build.swap_note_script.clone(),
        serial(3),
    )?;

    builder.add_output_note(RawOutputNote::Full(add_note.clone()));
    builder.add_output_note(RawOutputNote::Full(greedy_swap_note.clone()));

    let mut mock_chain = builder.build()?;
    let mut amm_account = build.account.clone();

    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &add_note,
        add_payout.expected_note(amm_id, vec![FungibleAsset::new(amm_id, lp1)?])?,
    )
    .await?;

    let ctx = mock_chain
        .build_tx_context(amm_account.id(), &[greedy_swap_note.id()], &[])?
        .build()?;
    let result = ctx.execute().await;
    assert!(result.is_err(), "slippage-violating swap must fail");
    Ok(())
}

/// A swap note carrying an asset that is not part of the pool pair must be rejected.
#[tokio::test]
async fn swap_wrong_asset_reverts() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_x = builder.add_existing_basic_faucet(auth(), "TKX", 1_000_000_000, Some(8))?;
    let faucet_y = builder.add_existing_basic_faucet(auth(), "TKY", 1_000_000_000, Some(8))?;
    let faucet_z = builder.add_existing_basic_faucet(auth(), "TKZ", 1_000_000_000, Some(8))?;
    let alice = builder.add_existing_wallet_with_assets(auth(), [])?;

    let build = build_amm_account([7u8; 32], faucet_x.id(), faucet_y.id(), FEE_BPS, true)?;
    let amm_id = build.account.id();
    builder.add_account(build.account.clone())?;

    let swap_payout = PayoutInfo::new(alice.id(), serial(3000));
    let alien_swap_note = create_swap_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_z.id(), 30_000)?, // not a pool asset
        faucet_y.id(),
        1,
        &swap_payout,
        build.swap_note_script.clone(),
        serial(3),
    )?;
    builder.add_output_note(RawOutputNote::Full(alien_swap_note.clone()));

    let mock_chain = builder.build()?;
    let ctx = mock_chain
        .build_tx_context(amm_id, &[alien_swap_note.id()], &[])?
        .build()?;
    let result = ctx.execute().await;
    assert!(result.is_err(), "swap with a non-pool asset must fail");
    Ok(())
}

/// A remove-liquidity note whose asset is not the pool's own LP token must be rejected.
#[tokio::test]
async fn remove_liquidity_requires_lp_token() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_x = builder.add_existing_basic_faucet(auth(), "TKX", 1_000_000_000, Some(8))?;
    let faucet_y = builder.add_existing_basic_faucet(auth(), "TKY", 1_000_000_000, Some(8))?;
    let alice = builder.add_existing_wallet_with_assets(auth(), [])?;

    let build = build_amm_account([7u8; 32], faucet_x.id(), faucet_y.id(), FEE_BPS, true)?;
    let amm_id = build.account.id();
    builder.add_account(build.account.clone())?;

    // craft a "remove" note that carries pool asset X instead of LP tokens
    let payout = PayoutInfo::new(alice.id(), serial(4000));
    let mut fake_remove = create_remove_liquidity_note(
        alice.id(),
        amm_id,
        1, // placeholder; assets replaced below
        0,
        0,
        &payout,
        build.remove_liquidity_note_script.clone(),
        serial(4),
    )?;
    // rebuild the note with a non-LP asset but identical script/storage
    fake_remove = Note::with_attachments(
        miden_client::note::NoteAssets::new(vec![
            FungibleAsset::new(faucet_x.id(), 1_000)?.into(),
        ])?,
        miden_client::note::PartialNoteMetadata::new(alice.id(), miden_client::note::NoteType::Public)
            .with_tag(fake_remove.metadata().tag()),
        fake_remove.recipient().clone(),
        fake_remove.attachments().clone(),
    );
    builder.add_output_note(RawOutputNote::Full(fake_remove.clone()));

    let mock_chain = builder.build()?;
    let ctx = mock_chain
        .build_tx_context(amm_id, &[fake_remove.id()], &[])?
        .build()?;
    let result = ctx.execute().await;
    assert!(result.is_err(), "remove_liquidity with a non-LP asset must fail");
    Ok(())
}

/// Multi-actor lifecycle: alice supplies both assets; bob swaps A->B and charlie swaps
/// B->A (a different amount); both receive exactly the amounts they asked for. Alice then
/// withdraws her entire LP position and provably collects the swap fees: her payout's
/// constant-product value strictly exceeds her pro-rata share of the ORIGINAL deposits.
#[tokio::test]
async fn multi_actor_swaps_accrue_fees_to_lp() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_a = builder.add_existing_basic_faucet(auth(), "TKA", 1_000_000_000, Some(8))?;
    let faucet_b = builder.add_existing_basic_faucet(auth(), "TKB", 1_000_000_000, Some(8))?;
    let alice = builder.add_existing_wallet_with_assets(auth(), [])?;
    let bob = builder.add_existing_wallet_with_assets(auth(), [])?;
    let charlie = builder.add_existing_wallet_with_assets(auth(), [])?;

    let build = build_amm_account([9u8; 32], faucet_a.id(), faucet_b.id(), FEE_BPS, true)?;
    let amm_id = build.account.id();
    builder.add_account(build.account.clone())?;

    // alice supplies 100_000 A + 400_000 B
    let (alice_lp, supply) = quote_initial_lp(100_000, 400_000);
    let add_payout = PayoutInfo::new(alice.id(), serial(1000));
    let add_note = create_add_liquidity_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_a.id(), 100_000)?,
        FungibleAsset::new(faucet_b.id(), 400_000)?,
        alice_lp,
        &add_payout,
        build.add_liquidity_note_script.clone(),
        serial(1),
    )?;

    // bob swaps 50_000 A -> B against (100_000, 400_000); he demands exactly the quote
    let bob_out = quote_swap_output(50_000, 100_000, 400_000, FEE_BPS);
    let bob_payout = PayoutInfo::new(bob.id(), serial(2000));
    let bob_swap = create_swap_note(
        bob.id(),
        amm_id,
        FungibleAsset::new(faucet_a.id(), 50_000)?,
        faucet_b.id(),
        bob_out,
        &bob_payout,
        build.swap_note_script.clone(),
        serial(2),
    )?;

    // charlie swaps 100_000 B -> A against (150_000, 400_000 - bob_out)
    let charlie_out = quote_swap_output(100_000, 400_000 - bob_out, 150_000, FEE_BPS);
    let charlie_payout = PayoutInfo::new(charlie.id(), serial(3000));
    let charlie_swap = create_swap_note(
        charlie.id(),
        amm_id,
        FungibleAsset::new(faucet_b.id(), 100_000)?,
        faucet_a.id(),
        charlie_out,
        &charlie_payout,
        build.swap_note_script.clone(),
        serial(3),
    )?;

    // alice exits her entire position against the post-swap reserves
    let (reserve_a, reserve_b) = (150_000 - charlie_out, 400_000 - bob_out + 100_000);
    let (alice_a, alice_b) = quote_remove_liquidity(alice_lp, reserve_a, reserve_b, supply);
    let remove_payout = PayoutInfo::new(alice.id(), serial(4000));
    let remove_note = create_remove_liquidity_note(
        alice.id(),
        amm_id,
        alice_lp,
        alice_a,
        alice_b,
        &remove_payout,
        build.remove_liquidity_note_script.clone(),
        serial(4),
    )?;

    for note in [&add_note, &bob_swap, &charlie_swap, &remove_note] {
        builder.add_output_note(RawOutputNote::Full((*note).clone()));
    }
    let mut mock_chain = builder.build()?;
    let mut amm_account = build.account.clone();

    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &add_note,
        add_payout.expected_note(amm_id, vec![FungibleAsset::new(amm_id, alice_lp)?])?,
    )
    .await?;

    // bob and charlie receive exactly what they asked for
    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &bob_swap,
        bob_payout.expected_note(amm_id, vec![FungibleAsset::new(faucet_b.id(), bob_out)?])?,
    )
    .await?;
    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &charlie_swap,
        charlie_payout
            .expected_note(amm_id, vec![FungibleAsset::new(faucet_a.id(), charlie_out)?])?,
    )
    .await?;

    // alice withdraws: fees from both swaps accrued to her LP position.
    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &remove_note,
        remove_payout.expected_note(
            amm_id,
            vec![
                FungibleAsset::new(faucet_a.id(), alice_a)?,
                FungibleAsset::new(faucet_b.id(), alice_b)?,
            ],
        )?,
    )
    .await?;

    // fee-accrual invariant: the constant-product value of alice's payout strictly exceeds
    // her pro-rata share of the ORIGINAL deposits (price moved, so compare in k terms).
    let payout_k = (alice_a as u128) * (alice_b as u128);
    let baseline_k = (alice_lp as u128).pow(2) * (100_000u128 * 400_000u128)
        / (supply as u128).pow(2);
    assert!(
        payout_k > baseline_k,
        "LP must earn fees: payout k {payout_k} <= baseline k {baseline_k}"
    );
    println!(
        "alice deposited (100000 A, 400000 B), withdrew ({alice_a} A, {alice_b} B); \
         fee gain in k terms: {payout_k} > {baseline_k}"
    );

    // after alice's full exit only the permanently locked minimum liquidity remains
    assert_eq!(lp_supply_of(&amm_account), MIN_LIQUIDITY);
    Ok(())
}

/// Liquidity payouts are bound to the note's sender: the withdrawer receives the funds,
/// and no note can redirect a withdrawal to a third party (the payout recipient is
/// computed in-VM from `get_sender`; note storage only carries a serial number).
#[tokio::test]
async fn remove_liquidity_payout_bound_to_sender() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_x = builder.add_existing_basic_faucet(auth(), "TKX", 1_000_000_000, Some(8))?;
    let faucet_y = builder.add_existing_basic_faucet(auth(), "TKY", 1_000_000_000, Some(8))?;
    let alice = builder.add_existing_wallet_with_assets(auth(), [])?;
    let bob = builder.add_existing_wallet_with_assets(auth(), [])?;

    let build = build_amm_account([7u8; 32], faucet_x.id(), faucet_y.id(), FEE_BPS, true)?;
    let amm_id = build.account.id();
    builder.add_account(build.account.clone())?;

    let (lp1, supply) = quote_initial_lp(100_000, 400_000);
    let add_payout = PayoutInfo::new(alice.id(), serial(1000));
    let add_note = create_add_liquidity_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 100_000)?,
        FungibleAsset::new(faucet_y.id(), 400_000)?,
        lp1,
        &add_payout,
        build.add_liquidity_note_script.clone(),
        serial(1),
    )?;

    // bob holds LP (bearer asset, e.g. transferred to him) and submits a remove note
    let lp_burn = 50_000u64;
    let (ax, ay) = quote_remove_liquidity(lp_burn, 100_000, 400_000, supply);
    let bob_payout = PayoutInfo::new(bob.id(), serial(5000));
    let bob_remove = create_remove_liquidity_note(
        bob.id(),
        amm_id,
        lp_burn,
        ax,
        ay,
        &bob_payout,
        build.remove_liquidity_note_script.clone(),
        serial(5),
    )?;

    builder.add_output_note(RawOutputNote::Full(add_note.clone()));
    builder.add_output_note(RawOutputNote::Full(bob_remove.clone()));
    let mut mock_chain = builder.build()?;
    let mut amm_account = build.account.clone();

    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &add_note,
        add_payout.expected_note(amm_id, vec![FungibleAsset::new(amm_id, lp1)?])?,
    )
    .await?;

    // the payout of bob's note goes to BOB (its sender) — an identically-parameterised
    // payout directed at alice must NOT be among the produced notes. There is no field a
    // note creator could set to redirect it: the recipient is derived in-VM from the sender.
    let payout_assets = vec![
        FungibleAsset::new(faucet_x.id(), ax)?,
        FungibleAsset::new(faucet_y.id(), ay)?,
    ];
    let bob_directed = bob_payout.expected_note(amm_id, payout_assets.clone())?;
    let alice_directed =
        PayoutInfo::new(alice.id(), serial(5000)).expected_note(amm_id, payout_assets)?;

    let ctx = mock_chain
        .build_tx_context(amm_id, &[bob_remove.id()], &[])?
        .extend_expected_output_notes(vec![RawOutputNote::Full(bob_directed.clone())])
        .build()?;
    let executed = ctx.execute().await?;
    let produced: Vec<_> = executed.output_notes().iter().map(|n| n.id()).collect();
    assert!(
        produced.contains(&bob_directed.id()),
        "the withdrawer (note sender) must receive the payout"
    );
    assert!(
        !produced.contains(&alice_directed.id()),
        "a bob-sent remove note must not pay out to alice"
    );
    amm_account.apply_delta(&executed.account_delta())?;
    assert_eq!(lp_supply_of(&amm_account), supply - lp_burn);
    Ok(())
}

/// A non-depositor holds no LP tokens, and every path to the pool's assets requires them:
/// an asset-less remove note and a burn larger than the total supply must both fail.
/// (The wrong-asset path is covered by `remove_liquidity_requires_lp_token`.)
#[tokio::test]
async fn non_depositor_cannot_withdraw() -> Result<()> {
    let mut builder = MockChain::builder();
    let faucet_x = builder.add_existing_basic_faucet(auth(), "TKX", 1_000_000_000, Some(8))?;
    let faucet_y = builder.add_existing_basic_faucet(auth(), "TKY", 1_000_000_000, Some(8))?;
    let alice = builder.add_existing_wallet_with_assets(auth(), [])?;
    let bob = builder.add_existing_wallet_with_assets(auth(), [])?;

    let build = build_amm_account([7u8; 32], faucet_x.id(), faucet_y.id(), FEE_BPS, true)?;
    let amm_id = build.account.id();
    builder.add_account(build.account.clone())?;

    let (lp1, supply) = quote_initial_lp(100_000, 400_000);
    let add_payout = PayoutInfo::new(alice.id(), serial(1000));
    let add_note = create_add_liquidity_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 100_000)?,
        FungibleAsset::new(faucet_y.id(), 400_000)?,
        lp1,
        &add_payout,
        build.add_liquidity_note_script.clone(),
        serial(1),
    )?;

    // (a) bob has no LP: a remove note without any assets must be rejected
    let template = create_remove_liquidity_note(
        bob.id(),
        amm_id,
        1,
        0,
        0,
        &PayoutInfo::new(bob.id(), serial(6000)),
        build.remove_liquidity_note_script.clone(),
        serial(6),
    )?;
    let empty_remove = Note::with_attachments(
        miden_client::note::NoteAssets::default(),
        miden_client::note::PartialNoteMetadata::new(bob.id(), miden_client::note::NoteType::Public)
            .with_tag(template.metadata().tag()),
        template.recipient().clone(),
        template.attachments().clone(),
    );

    // (b) a burn exceeding the total LP supply must be rejected
    let oversized_remove = create_remove_liquidity_note(
        bob.id(),
        amm_id,
        supply + 1,
        0,
        0,
        &PayoutInfo::new(bob.id(), serial(7000)),
        build.remove_liquidity_note_script.clone(),
        serial(7),
    )?;

    builder.add_output_note(RawOutputNote::Full(add_note.clone()));
    builder.add_output_note(RawOutputNote::Full(empty_remove.clone()));
    builder.add_output_note(RawOutputNote::Full(oversized_remove.clone()));
    let mut mock_chain = builder.build()?;
    let mut amm_account = build.account.clone();

    consume_amm_note(
        &mut mock_chain,
        &mut amm_account,
        &add_note,
        add_payout.expected_note(amm_id, vec![FungibleAsset::new(amm_id, lp1)?])?,
    )
    .await?;

    let ctx = mock_chain
        .build_tx_context(amm_id, &[empty_remove.id()], &[])?
        .build()?;
    assert!(
        ctx.execute().await.is_err(),
        "an asset-less remove note must fail"
    );

    let ctx = mock_chain
        .build_tx_context(amm_id, &[oversized_remove.id()], &[])?
        .build()?;
    assert!(
        ctx.execute().await.is_err(),
        "burning more LP than the total supply must fail"
    );

    // pool state is untouched by the failed attempts
    assert_eq!(lp_supply_of(&amm_account), supply);
    Ok(())
}
