//! Live-testnet end-to-end test of the AMM as a Miden network account.
//!
//! Flow: deploy the AMM (network account with allowlisted note scripts), add liquidity,
//! swap, and remove liquidity — each AMM interaction is a network note executed
//! asynchronously by the network transaction builder (NTB).
//!
//! Runs against the public testnet, so it is `#[ignore]`d by default:
//!   cargo test --test amm_swap_ntx -- --ignored --nocapture

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use miden_amm::common::{
    FEE_DENOM, PayoutInfo, build_amm_account, create_add_liquidity_note,
    create_remove_liquidity_note, create_swap_note, create_basic_account, create_basic_faucet,
    lp_supply_slot, mint_and_consume, quote_initial_lp, quote_remove_liquidity,
    quote_swap_output, wait_for_tx,
};
use miden_client::{
    Client, Word,
    account::AccountId,
    address::NetworkId,
    asset::{AssetCallbackFlag, AssetVaultKey, FungibleAsset},
    builder::ClientBuilder,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{Note, NoteId},
    rpc::{Endpoint, GrpcClient, NodeRpcClient},
    transaction::TransactionRequestBuilder,
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use rand::RngCore;
use tokio::time::{Duration, sleep};

const FEE_BPS: u64 = 30;

/// Polls the AMM account state until its lp_supply slot reaches `expected`.
/// The NTB executes network notes asynchronously, so this is the signal that our
/// note was picked up and processed. Every few polls the node is asked for the
/// network note's processing status, which surfaces NTB-side execution errors.
async fn wait_for_lp_supply(
    client: &mut Client<FilesystemKeyStore>,
    rpc_client: &Arc<GrpcClient>,
    note_id: NoteId,
    amm_id: AccountId,
    expected: u64,
) -> Result<()> {
    for attempt in 0..40 {
        client.sync_state().await?;
        if let Some(account) = client.get_account(amm_id).await? {
            let word: Word = account
                .storage()
                .get_item(&lp_supply_slot())
                .context("reading lp_supply slot")?
                .into();
            let value = word[0].as_canonical_u64();
            println!("[poll {attempt}] lp_supply = {value} (waiting for {expected})");
            if value == expected {
                return Ok(());
            }
        }
        if attempt % 5 == 4 {
            match rpc_client.get_network_note_status(note_id).await {
                Ok(info) => {
                    println!(
                        "network note status: {} (attempts: {}, last_error: {:?})",
                        info.status, info.attempt_count, info.last_error
                    );
                }
                Err(e) => println!("network note status query failed: {e}"),
            }
        }
        sleep(Duration::from_secs(6)).await;
    }
    bail!(
        "timed out waiting for lp_supply == {expected}; the network transaction builder \
         has not consumed the note (check the allowlist / NetworkAccountTarget attachment)"
    )
}

/// Submits a network note from `sender` and waits for the creating tx to commit.
async fn submit_amm_note(
    client: &mut Client<FilesystemKeyStore>,
    sender: AccountId,
    note: Note,
) -> Result<()> {
    let req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note])
        .build()
        .context("building note submission request")?;
    let tx_id = client.submit_new_transaction(sender, req).await?;
    println!("submitted note tx: https://testnet.midenscan.com/tx/{tx_id:?}");
    wait_for_tx(client, tx_id).await?;
    Ok(())
}

/// Consumes a (private) payout note into `target`'s vault.
///
/// Payout notes are private, so they never show up in `get_consumable_notes`; the
/// recipient passes the full note (which they can reconstruct) as an unauthenticated
/// input note. Retries until the note actually exists on-chain.
async fn claim_payout(
    client: &mut Client<FilesystemKeyStore>,
    target: AccountId,
    payout_note: &Note,
) -> Result<()> {
    for attempt in 0..40 {
        client.sync_state().await?;
        let req = TransactionRequestBuilder::new()
            .input_notes([(payout_note.clone(), None)])
            .build()
            .context("building payout consume request")?;
        match client.submit_new_transaction(target, req).await {
            Ok(tx_id) => {
                println!("claimed payout: https://testnet.midenscan.com/tx/{tx_id:?}");
                wait_for_tx(client, tx_id).await?;
                return Ok(());
            }
            Err(e) => {
                println!("[claim attempt {attempt}] payout not claimable yet: {e}");
                sleep(Duration::from_secs(6)).await;
            }
        }
    }
    bail!("failed to claim payout note {:?}", payout_note.id())
}

/// Polls until the AMM's vault balance for `faucet_id` equals `expected` — the signal
/// that the network transaction builder executed a swap.
async fn wait_for_amm_balance(
    client: &mut Client<FilesystemKeyStore>,
    amm_id: AccountId,
    faucet_id: AccountId,
    expected: u64,
) -> Result<()> {
    for attempt in 0..40 {
        let balance = balance_of(client, amm_id, faucet_id).await?;
        println!("[poll {attempt}] amm balance = {balance} (waiting for {expected})");
        if balance == expected {
            return Ok(());
        }
        sleep(Duration::from_secs(6)).await;
    }
    bail!("timed out waiting for AMM balance == {expected}")
}

async fn balance_of(
    client: &mut Client<FilesystemKeyStore>,
    account_id: AccountId,
    faucet_id: AccountId,
) -> Result<u64> {
    client.sync_state().await?;
    let account = client
        .get_account(account_id)
        .await?
        .context("account not found")?;
    let amount = account
        .vault()
        .get_balance(AssetVaultKey::new_fungible(faucet_id, AssetCallbackFlag::Disabled))
        .context("reading vault balance")?;
    Ok(amount.as_u64())
}

#[tokio::test]
#[ignore = "runs against the live Miden testnet: cargo test --test amm_swap_ntx -- --ignored --nocapture"]
async fn amm_network_account_e2e() -> Result<()> {
    // ---------------------------------------------------------------------------------
    // client setup (fresh store per run; keys are per-account so the keystore persists)
    // ---------------------------------------------------------------------------------
    let _ = std::fs::remove_file("./store.sqlite3");
    let endpoint = Endpoint::testnet();
    let rpc_client = Arc::new(GrpcClient::new(&endpoint, 10_000));
    let keystore = Arc::new(FilesystemKeyStore::new(PathBuf::from("./keystore"))?);
    let mut client = ClientBuilder::new()
        .rpc(rpc_client.clone())
        .sqlite_store(PathBuf::from("./store.sqlite3"))
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await?;
    let sync = client.sync_state().await?;
    println!("connected to testnet, latest block: {}", sync.block_num);

    // ---------------------------------------------------------------------------------
    // actors: alice + the two pool-asset faucets; fund alice
    // ---------------------------------------------------------------------------------
    let alice = create_basic_account(&mut client, &keystore).await?;
    println!("alice: {:?}", alice.id().to_bech32(NetworkId::Testnet));
    let faucet_x = create_basic_faucet(&mut client, &keystore, "TKX").await?;
    let faucet_y = create_basic_faucet(&mut client, &keystore, "TKY").await?;
    println!(
        "faucet X: {:?}\nfaucet Y: {:?}",
        faucet_x.id().to_bech32(NetworkId::Testnet),
        faucet_y.id().to_bech32(NetworkId::Testnet)
    );
    client.sync_state().await?;

    mint_and_consume(&mut client, faucet_x.id(), alice.id(), 1_000_000).await?;
    mint_and_consume(&mut client, faucet_y.id(), alice.id(), 4_000_000).await?;
    println!("alice funded with 1_000_000 TKX and 4_000_000 TKY");

    // ---------------------------------------------------------------------------------
    // build + deploy the AMM network account
    // ---------------------------------------------------------------------------------
    let mut amm_seed = [0u8; 32];
    client.rng().fill_bytes(&mut amm_seed);
    let build = build_amm_account(amm_seed, faucet_x.id(), faucet_y.id(), FEE_BPS, false)?;
    let amm_id = build.account.id();
    client.add_account(&build.account, false).await?;
    println!("AMM (network account): {:?}", amm_id.to_bech32(NetworkId::Testnet));

    let deploy_req = TransactionRequestBuilder::new()
        .custom_script(build.deploy_tx_script.clone())
        .build()
        .context("building deploy request")?;
    let deploy_tx = client.submit_new_transaction(amm_id, deploy_req).await?;
    println!("deploy tx: https://testnet.midenscan.com/tx/{deploy_tx:?}");
    wait_for_tx(&mut client, deploy_tx).await?;
    println!("AMM deployed ✅");

    // ---------------------------------------------------------------------------------
    // add liquidity: 100_000 TKX + 400_000 TKY -> 199_000 LP (supply 200_000)
    // ---------------------------------------------------------------------------------
    let (lp_minted, supply) = quote_initial_lp(100_000, 400_000);
    let add_payout = PayoutInfo::new(alice.id(), client.rng().draw_word());
    let add_note = create_add_liquidity_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 100_000)?,
        FungibleAsset::new(faucet_y.id(), 400_000)?,
        lp_minted,
        &add_payout,
        build.add_liquidity_note_script.clone(),
        client.rng().draw_word(),
    )?;
    let add_note_id = add_note.id();
    submit_amm_note(&mut client, alice.id(), add_note).await?;
    println!("add-liquidity note submitted; waiting for the network transaction builder...");
    wait_for_lp_supply(&mut client, &rpc_client, add_note_id, amm_id, supply).await?;
    println!("liquidity added ✅ (lp_supply = {supply})");

    let lp_payout_note =
        add_payout.expected_note(amm_id, vec![FungibleAsset::new(amm_id, lp_minted)?])?;
    claim_payout(&mut client, alice.id(), &lp_payout_note).await?;
    let lp_balance = balance_of(&mut client, alice.id(), amm_id).await?;
    assert_eq!(lp_balance, lp_minted, "alice must hold the minted LP tokens");
    println!("alice holds {lp_balance} LP ✅");

    // ---------------------------------------------------------------------------------
    // swap: 30_000 TKX -> TKY against reserves (100_000, 400_000)
    // ---------------------------------------------------------------------------------
    let dy = quote_swap_output(30_000, 100_000, 400_000, FEE_BPS);
    println!("swapping 30_000 TKX for an expected {dy} TKY (fee {FEE_BPS}/{FEE_DENOM})");
    let y_before = balance_of(&mut client, alice.id(), faucet_y.id()).await?;

    let swap_payout = PayoutInfo::new(alice.id(), client.rng().draw_word());
    let swap_note = create_swap_note(
        alice.id(),
        amm_id,
        FungibleAsset::new(faucet_x.id(), 30_000)?,
        faucet_y.id(),
        dy,
        &swap_payout,
        build.swap_note_script.clone(),
        client.rng().draw_word(),
    )?;
    submit_amm_note(&mut client, alice.id(), swap_note).await?;
    println!("swap note submitted; waiting for the network transaction builder...");
    // the AMM's TKX reserve reaching 130_000 signals the swap executed
    wait_for_amm_balance(&mut client, amm_id, faucet_x.id(), 130_000).await?;
    println!("swap executed by the network transaction builder ✅");

    let swap_payout_note =
        swap_payout.expected_note(amm_id, vec![FungibleAsset::new(faucet_y.id(), dy)?])?;
    claim_payout(&mut client, alice.id(), &swap_payout_note).await?;

    let y_after = balance_of(&mut client, alice.id(), faucet_y.id()).await?;
    assert_eq!(y_after, y_before + dy, "alice must receive exactly the quoted output");
    println!("swap executed on testnet ✅ alice received {dy} TKY");

    // ---------------------------------------------------------------------------------
    // remove liquidity: burn half of alice's LP
    // ---------------------------------------------------------------------------------
    let lp_burn = lp_minted / 2;
    let (ax, ay) = quote_remove_liquidity(lp_burn, 130_000, 400_000 - dy, supply);
    let remove_payout = PayoutInfo::new(alice.id(), client.rng().draw_word());
    let remove_note = create_remove_liquidity_note(
        alice.id(),
        amm_id,
        lp_burn,
        ax,
        ay,
        &remove_payout,
        build.remove_liquidity_note_script.clone(),
        client.rng().draw_word(),
    )?;
    let remove_note_id = remove_note.id();
    submit_amm_note(&mut client, alice.id(), remove_note).await?;
    println!("remove-liquidity note submitted; waiting for the network transaction builder...");
    wait_for_lp_supply(&mut client, &rpc_client, remove_note_id, amm_id, supply - lp_burn).await?;

    let remove_payout_note = remove_payout.expected_note(
        amm_id,
        vec![
            FungibleAsset::new(faucet_x.id(), ax)?,
            FungibleAsset::new(faucet_y.id(), ay)?,
        ],
    )?;
    claim_payout(&mut client, alice.id(), &remove_payout_note).await?;
    println!("liquidity removed ✅ alice received {ax} TKX and {ay} TKY");

    println!("\n🎉 full Uniswap-v2 AMM lifecycle executed on Miden testnet");
    println!("AMM account: {:?}", amm_id.to_bech32(NetworkId::Testnet));
    Ok(())
}
