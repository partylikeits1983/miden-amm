use std::{fs, path::Path};

use miden_amm::common::{create_amm_account, create_amm_input_note, create_library};
use miden_client::{
    asset::{Asset, FungibleAsset},
    note::NoteType,
    testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
};
// use miden_clob::{create_partial_swap_note, try_match_swapp_notes};
use miden_testing::{Auth, MockChain, TransactionContextBuilder};

use miden_objects::{
    testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2, transaction::OutputNote,
};

#[test]
fn p2id_script_multiple_assets() -> anyhow::Result<()> {
    let mut mock_chain = MockChain::new();

    // Create assets
    let fungible_asset_1: Asset = FungibleAsset::mock(123);
    let fungible_asset_2: Asset =
        FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().unwrap(), 456)
            .unwrap()
            .into();

    // Create sender and target account
    let sender_account = mock_chain.add_pending_new_wallet(Auth::BasicAuth);
    let target_account = mock_chain.add_pending_existing_wallet(Auth::BasicAuth, vec![]);

    // Create the note
    let _note = mock_chain
        .add_pending_p2id_note(
            sender_account.id(),
            target_account.id(),
            &[fungible_asset_1, fungible_asset_2],
            NoteType::Public,
        )
        .unwrap();

    mock_chain.prove_next_block()?;

    Ok(())
}

#[tokio::test]
async fn amm_test() -> anyhow::Result<()> {
    let mut mock_chain = MockChain::new();
    mock_chain.prove_until_block(1u32)?;

    // Initialize assets & accounts
    let _asset_a: Asset =
        FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1.try_into().unwrap(), 100)
            .unwrap()
            .into();
    let _asset_b: Asset =
        FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().unwrap(), 100)
            .unwrap()
            .into();

    // Create sender and target and malicious account
    let alice_account = mock_chain.add_pending_existing_wallet(Auth::BasicAuth, vec![]);
    let _bob_account = mock_chain.add_pending_existing_wallet(Auth::BasicAuth, vec![]);

    mock_chain.add_pending_account(alice_account.clone());

    // Load the MASM file for the counter contract
    let counter_path = Path::new("masm/accounts/amm_account.masm");
    let counter_code = fs::read_to_string(counter_path).unwrap();

    let (amm_account, account_seed) = create_amm_account(&counter_code).await?;

    mock_chain.add_pending_account(amm_account.clone());
    mock_chain.prove_next_block()?;

    let note_code = fs::read_to_string(Path::new("masm/notes/amm_input_note.masm")).unwrap();
    let account_code = fs::read_to_string(Path::new("masm/accounts/amm_account.masm")).unwrap();

    let library_path = "external_contract::amm_contract";
    let library = create_library(account_code, library_path).unwrap();

    let amm_input_note = create_amm_input_note(note_code, library, alice_account, amm_account.id())
        .await
        .unwrap();

    mock_chain.add_pending_note(OutputNote::Full(amm_input_note.clone()));
    mock_chain.prove_next_block()?;

    let tx_inputs = mock_chain.get_transaction_inputs(
        amm_account.clone(),
        Some(account_seed),
        &[amm_input_note.id()],
        &[],
    )?;
    let tx_context = TransactionContextBuilder::new(amm_account.clone())
        .tx_inputs(tx_inputs)
        .build()?;
    let executed_transaction = tx_context.execute().await?;

    let _target_account = mock_chain.add_pending_executed_transaction(&executed_transaction)?;

    Ok(())
}

#[test]
fn test_convert_two_32bit_to_64bit() {
    // Your two 32-bit integers
    let int1: u32 = 0;
    let int2: u32 = 100_000;

    // Convert to 64-bit integer (int2 as high bits, int1 as low bits)
    let result: u64 = ((int2 as u64) << 32) | (int1 as u64);

    println!("Converting two 32-bit integers to 64-bit:");
    let result_alt: u64 = ((int1 as u64) << 32) | (int2 as u64);
    println!("value: {}", result_alt);

    assert!(result > 0);
}
