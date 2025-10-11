use miden_amm::common::{create_amm_input_note, create_testing_amm_account};
use miden_client::{
    asset::{Asset, FungibleAsset},
    testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
};
use miden_testing::{Auth, MockChain, TransactionContextBuilder};

use miden_objects::{
    testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2, transaction::OutputNote,
};

#[tokio::test]
async fn amm_local_test() -> anyhow::Result<()> {
    let mut builder = MockChain::builder();

    // Initialize assets & accounts
    let asset_a: Asset = FungibleAsset::new(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1.try_into().unwrap(),
        10_000,
    )
    .unwrap()
    .into();
    let asset_b: Asset = FungibleAsset::new(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().unwrap(),
        10_001,
    )
    .unwrap()
    .into();

    println!(
        "ASSET A: {:?} {:?}",
        asset_a.unwrap_fungible().faucet_id().prefix(),
        asset_a.unwrap_fungible().faucet_id().suffix()
    );
    println!(
        "ASSET B: {:?} {:?}",
        asset_b.unwrap_fungible().faucet_id().prefix(),
        asset_b.unwrap_fungible().faucet_id().suffix()
    );

    // Create alice account for the note creation
    let alice_account = builder.add_existing_wallet(Auth::BasicAuth)?;

    let amm_account = create_testing_amm_account(asset_a, asset_b).await?;

    // Initialize assets & accounts
    let asset_in: FungibleAsset =
        FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1.try_into().unwrap(), 100)
            .unwrap()
            .into();
    let asset_out: FungibleAsset =
        FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().unwrap(), 95)
            .unwrap()
            .into();

    let amm_input_note = create_amm_input_note(
        alice_account.clone().id(),
        amm_account.id(),
        asset_in,
        asset_out,
    )
    .await
    .unwrap();

    // Add the note to the builder
    builder.add_note(OutputNote::Full(amm_input_note.clone()));

    // Build the mock chain
    let mock_chain = builder.build()?;

    let tx_inputs = mock_chain.get_transaction_inputs(
        amm_account.clone(),
        None,
        &[amm_input_note.id()],
        &[],
    )?;

    let tx_context = TransactionContextBuilder::new(amm_account.clone())
        .account_seed(None)
        .tx_inputs(tx_inputs)
        .build()?;
    let _executed_transaction = tx_context.execute().await?;

    Ok(())
}
