use anyhow::Result;
use miden_amm::common::{
    create_basic_account, create_basic_faucet, create_library_with_assembler, wait_for_note,
};
use rand::RngCore;
use std::{fs, path::Path, sync::Arc};

use miden_client::{
    Felt, ScriptBuilder, Word,
    account::{
        AccountBuilder, AccountIdAddress, AccountStorageMode, AccountType, Address,
        AddressInterface, StorageMap, StorageSlot, component::BasicWallet,
    },
    asset::FungibleAsset,
    auth::AuthSecretKey,
    builder::ClientBuilder,
    crypto::{FeltRng, SecretKey},
    keystore::FilesystemKeyStore,
    note::{
        Note, NoteAssets, NoteExecutionHint, NoteExecutionMode, NoteInputs, NoteMetadata,
        NoteRecipient, NoteTag, NoteType, create_p2id_note,
    },
    rpc::{Endpoint, TonicRpcClient},
    transaction::{OutputNote, TransactionKernel, TransactionRequestBuilder},
};
use miden_lib::account::auth::NoAuth;
use miden_objects::{
    account::{AccountComponent, NetworkId},
    assembly::Assembler,
};

#[tokio::test]
async fn amm_local_test() -> Result<()> {
    // Initialize client & keystore
    let endpoint = Endpoint::testnet();
    let timeout_ms = 10_000;
    let rpc_api = Arc::new(TonicRpcClient::new(&endpoint, timeout_ms));

    let mut client = ClientBuilder::new()
        .rpc(rpc_api)
        .filesystem_keystore("./keystore")
        .in_debug_mode(true.into())
        .build()
        .await?;

    let sync_summary = client.sync_state().await?;
    println!("Latest block: {}", sync_summary.block_num);

    let keystore = FilesystemKeyStore::new("./keystore".into())?;

    // -------------------------------------------------------------------------
    // STEP 1: Deploy AMM contract (renamed from deposit_withdraw)
    // -------------------------------------------------------------------------
    println!("\n[STEP 1] Creating AMM contract");

    // Create Alice's account
    let alice_account = create_basic_account(&mut client, keystore.clone()).await?;
    let alice_account_id = alice_account.id();
    println!(
        "Alice's account ID: {:?}",
        Address::from(AccountIdAddress::new(
            alice_account_id,
            AddressInterface::Unspecified
        ))
        .to_bech32(NetworkId::Testnet)
    );

    // Create two faucets
    println!("\nDeploying faucet A (asset A).");
    let faucet_a = create_basic_faucet(&mut client, keystore.clone()).await?;
    println!(
        "Faucet A account ID: {:?}",
        Address::from(AccountIdAddress::new(
            faucet_a.id(),
            AddressInterface::Unspecified
        ))
        .to_bech32(NetworkId::Testnet)
    );

    println!("\nDeploying faucet B (asset B).");
    let faucet_b = create_basic_faucet(&mut client, keystore.clone()).await?;
    println!(
        "Faucet B account ID: {:?}",
        Address::from(AccountIdAddress::new(
            faucet_b.id(),
            AddressInterface::Unspecified
        ))
        .to_bech32(NetworkId::Testnet)
    );

    println!(
        "faucet A ID: {:?} {:?}",
        faucet_a.id().prefix(),
        faucet_a.id().suffix()
    );
    println!(
        "faucet B ID: {:?} {:?}",
        faucet_b.id().prefix(),
        faucet_b.id().suffix()
    );

    client.sync_state().await?;

    // Prepare assembler (debug mode = true)
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Load the MASM file for the AMM contract (renamed from deposit_withdraw)
    let contract_path = Path::new("masm/accounts/amm.masm");
    let contract_code = fs::read_to_string(contract_path)?;

    let storage_map = StorageMap::new();
    let storage_slot_map = StorageSlot::Map(storage_map.clone());

    // Compile the account code into `AccountComponent` with one storage slot for balance
    let contract_component =
        AccountComponent::compile(contract_code.clone(), assembler, vec![storage_slot_map])?
            .with_supports_all_types();

    // Init seed for the AMM contract
    let mut seed = [0_u8; 32];
    client.rng().fill_bytes(&mut seed);

    let key_pair = SecretKey::with_rng(client.rng());

    // Build the new `Account` with the component
    let (amm_contract, contract_seed) = AccountBuilder::new(seed)
        .account_type(AccountType::RegularAccountImmutableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_component(BasicWallet)
        .with_component(contract_component.clone())
        .with_auth_component(NoAuth)
        .build()?;

    println!("AMM contract commitment: {:?}", amm_contract.commitment());
    println!(
        "AMM contract id: {:?}",
        Address::from(AccountIdAddress::new(
            amm_contract.id(),
            AddressInterface::Unspecified
        ))
        .to_bech32(NetworkId::Testnet)
    );
    println!("AMM contract storage: {:?}", amm_contract.storage());

    client
        .add_account(&amm_contract.clone(), Some(contract_seed), false)
        .await?;

    keystore.add_key(&AuthSecretKey::RpoFalcon512(key_pair))?;

    // -------------------------------------------------------------------------
    // STEP 2: Mint tokens for Alice
    // -------------------------------------------------------------------------
    println!("\n[STEP 2] Mint tokens for Alice");

    // Mint from Faucet A
    let faucet_a_id = faucet_a.id();
    let amount_a: u64 = 500;
    let mint_amount_a = FungibleAsset::new(faucet_a_id, amount_a)?;
    let tx_request_a = TransactionRequestBuilder::new().build_mint_fungible_asset(
        mint_amount_a,
        alice_account_id,
        NoteType::Public,
        client.rng(),
    )?;

    let tx_exec_a = client.new_transaction(faucet_a.id(), tx_request_a).await?;
    client.submit_transaction(tx_exec_a.clone()).await?;

    let p2id_note_a = if let OutputNote::Full(note) = tx_exec_a.created_notes().get_note(0) {
        note.clone()
    } else {
        return Err(anyhow::anyhow!("Expected OutputNote::Full for faucet A"));
    };

    // Mint from Faucet B
    let faucet_b_id = faucet_b.id();
    let amount_b: u64 = 500;
    let mint_amount_b = FungibleAsset::new(faucet_b_id, amount_b)?;
    let tx_request_b = TransactionRequestBuilder::new().build_mint_fungible_asset(
        mint_amount_b,
        alice_account_id,
        NoteType::Public,
        client.rng(),
    )?;

    let tx_exec_b = client.new_transaction(faucet_b.id(), tx_request_b).await?;
    client.submit_transaction(tx_exec_b.clone()).await?;

    let p2id_note_b = if let OutputNote::Full(note) = tx_exec_b.created_notes().get_note(0) {
        note.clone()
    } else {
        return Err(anyhow::anyhow!("Expected OutputNote::Full for faucet B"));
    };

    // -------------------------------------------------------------------------
    // STEP 3: Consume minted notes
    // -------------------------------------------------------------------------
    println!("\n[STEP 3] Consume minted notes");

    // Wait for the P2ID note A to be available
    wait_for_note(&mut client, &alice_account, &p2id_note_a).await?;

    let consume_request_a = TransactionRequestBuilder::new()
        .authenticated_input_notes([(p2id_note_a.id(), None)])
        .build()?;
    let tx_exec_a = client
        .new_transaction(alice_account_id, consume_request_a)
        .await?;
    client.submit_transaction(tx_exec_a).await?;
    client.sync_state().await?;

    // Wait for the P2ID note B to be available
    wait_for_note(&mut client, &alice_account, &p2id_note_b).await?;

    let consume_request_b = TransactionRequestBuilder::new()
        .authenticated_input_notes([(p2id_note_b.id(), None)])
        .build()?;
    let tx_exec_b = client
        .new_transaction(alice_account_id, consume_request_b)
        .await?;
    client.submit_transaction(tx_exec_b).await?;
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 4: Create deposit notes
    // -------------------------------------------------------------------------
    println!("\n[STEP 4] Create deposit notes with assets from both faucets");

    let assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Create library from the AMM contract code so the note can call its procedures
    let contract_lib = create_library_with_assembler(
        assembler.clone(),
        "external_contract::deposit_withdraw_contract",
        &contract_code,
    )
    .map_err(|e| anyhow::anyhow!("Failed to create library: {}", e))?;

    let note_code = fs::read_to_string(Path::new("masm/notes/deposit_withdraw_note.masm"))?;

    // Create deposit note for Asset A
    let serial_num_a = client.rng().draw_word();
    let note_script_a = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&contract_lib)
        .unwrap()
        .compile_note_script(note_code.clone())
        .unwrap();

    let note_inputs_a = NoteInputs::new(vec![])?; // No special inputs needed
    let recipient_a = NoteRecipient::new(serial_num_a, note_script_a, note_inputs_a);
    let tag_a = NoteTag::for_public_use_case(0, 0, NoteExecutionMode::Local)?;
    let metadata_a = NoteMetadata::new(
        alice_account_id,
        NoteType::Public,
        tag_a,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;
    let deposit_amount_a = FungibleAsset::new(faucet_a_id, 100).unwrap();
    let vault_a = NoteAssets::new(vec![deposit_amount_a.into()])?;
    let deposit_note_a = Note::new(vault_a, metadata_a, recipient_a);
    println!("deposit note A hash: {:?}", deposit_note_a.id().to_hex());

    // Create deposit note for Asset B
    let serial_num_b = client.rng().draw_word();
    let note_script_b = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&contract_lib)
        .unwrap()
        .compile_note_script(note_code.clone())
        .unwrap();

    let note_inputs_b = NoteInputs::new(vec![])?; // No special inputs needed
    let recipient_b = NoteRecipient::new(serial_num_b, note_script_b, note_inputs_b);
    let tag_b = NoteTag::for_public_use_case(0, 0, NoteExecutionMode::Local)?;
    let metadata_b = NoteMetadata::new(
        alice_account_id,
        NoteType::Public,
        tag_b,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;
    let deposit_amount_b = FungibleAsset::new(faucet_b_id, 100).unwrap();
    let vault_b = NoteAssets::new(vec![deposit_amount_b.into()])?;
    let deposit_note_b = Note::new(vault_b, metadata_b, recipient_b);
    println!("deposit note B hash: {:?}", deposit_note_b.id().to_hex());

    // Submit deposit note A
    let note_request_a = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(deposit_note_a.clone())])
        .build()?;
    let tx_result_a = client
        .new_transaction(alice_account_id, note_request_a)
        .await?;
    println!(
        "View deposit A transaction on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result_a.executed_transaction().id()
    );
    let _ = client.submit_transaction(tx_result_a.clone()).await;
    client.sync_state().await?;

    // Submit deposit note B
    let note_request_b = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(deposit_note_b.clone())])
        .build()?;
    let tx_result_b = client
        .new_transaction(alice_account_id, note_request_b)
        .await?;
    println!(
        "View deposit B transaction on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result_b.executed_transaction().id()
    );
    let _ = client.submit_transaction(tx_result_b.clone()).await;
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 5: AMM account consumes the deposit notes for both assets
    // -------------------------------------------------------------------------
    println!("\n[STEP 5] AMM account consumes deposit notes for both assets");

    // Consume deposit note A
    let consume_deposit_request_a = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(deposit_note_a, None)])
        .build()?;
    let tx_result_a = client
        .new_transaction(amm_contract.id(), consume_deposit_request_a)
        .await?;
    println!(
        "AMM consume deposit A Tx on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result_a.executed_transaction().id()
    );
    println!(
        "AMM account delta A: {:?}",
        tx_result_a.account_delta().vault()
    );
    let _ = client.submit_transaction(tx_result_a).await;
    client.sync_state().await?;

    // Consume deposit note B
    let consume_deposit_request_b = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(deposit_note_b, None)])
        .build()?;
    let tx_result_b = client
        .new_transaction(amm_contract.id(), consume_deposit_request_b)
        .await?;
    println!(
        "AMM consume deposit B Tx on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result_b.executed_transaction().id()
    );
    println!(
        "AMM account delta B: {:?}",
        tx_result_b.account_delta().vault()
    );
    let _ = client.submit_transaction(tx_result_b).await;
    client.sync_state().await?;

    // Check AMM contract balance after consuming both deposits
    let account = client.get_account(amm_contract.id()).await?;
    println!(
        "AMM contract balance after deposits: {:?}",
        account.unwrap().account().storage().get_item(0)
    );

    // -------------------------------------------------------------------------
    // [STEP 6] Create P2ID output note for Alice
    // -------------------------------------------------------------------------

    println!("\n[STEP 6] Create P2ID withdraw note for Alice");

    let output_asset = FungibleAsset::new(faucet_a_id, 9)?;

    // Create a P2ID note with the same asset amount, targeted to Alice
    let amm_output_p2id_note = create_p2id_note(
        amm_contract.id(),         // sender (the contract)
        alice_account_id,          // target (Alice)
        vec![output_asset.into()], // same asset that was deposited
        NoteType::Private,
        Felt::new(0),
        client.rng(),
    )
    .unwrap();

    println!(
        "Withdraw P2ID note id: {:?}",
        amm_output_p2id_note.id().to_hex()
    );
    println!("Withdraw note assets: {:?}", amm_output_p2id_note.assets());

    // -------------------------------------------------------------------------
    // STEP 6: Create AMM input note
    // -------------------------------------------------------------------------
    println!("\n[STEP 6] Create AMM input note");

    let assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Load the AMM account code to create library
    let amm_contract_path = Path::new("masm/accounts/amm.masm");
    let amm_contract_code = fs::read_to_string(amm_contract_path)?;

    // Create library from the AMM contract code
    let amm_contract_lib = create_library_with_assembler(
        assembler.clone(),
        "external_contract::amm_contract",
        &amm_contract_code,
    )
    .map_err(|e| anyhow::anyhow!("Failed to create AMM library: {}", e))?;

    let amm_note_code = fs::read_to_string(Path::new("masm/notes/amm_input_note.masm"))?;
    let serial_num_amm = client.rng().draw_word();

    let amm_note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&amm_contract_lib)
        .unwrap()
        .compile_note_script(amm_note_code)
        .unwrap();

    let output_note_details_word: Word = [
        amm_output_p2id_note.metadata().execution_hint().into(),
        amm_output_p2id_note.metadata().note_type().into(),
        amm_output_p2id_note.metadata().aux(),
        amm_output_p2id_note.metadata().tag().into(),
    ]
    .into();

    let output_note_recipient: Word = amm_output_p2id_note.recipient().digest();

    let amm_note_inputs = NoteInputs::new(vec![
        faucet_b_id.suffix(),
        faucet_b_id.prefix().into(),
        Felt::new(0),
        Felt::new(0),
        output_note_recipient[0],
        output_note_recipient[1],
        output_note_recipient[2],
        output_note_recipient[3],
        output_note_details_word[0],
        output_note_details_word[1],
        output_note_details_word[2],
        output_note_details_word[3],
    ])?;

    println!("amm note inputs: {:?}", amm_note_inputs);

    let amm_recipient = NoteRecipient::new(serial_num_amm, amm_note_script, amm_note_inputs);
    let amm_tag = NoteTag::for_public_use_case(0, 0, NoteExecutionMode::Local)?;
    let amm_metadata = NoteMetadata::new(
        alice_account_id,
        NoteType::Public,
        amm_tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;

    // Create AMM input note
    let input_asset = FungibleAsset::new(faucet_a_id, 10)?;
    let amm_vault = NoteAssets::new(vec![input_asset.into()])?;
    let amm_input_note = Note::new(amm_vault, amm_metadata, amm_recipient);
    println!("AMM input note hash: {:?}", amm_input_note.id().to_hex());

    let amm_note_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(amm_input_note.clone())])
        .build()?;
    let amm_tx_result = client
        .new_transaction(alice_account_id, amm_note_request)
        .await?;
    println!(
        "View AMM input transaction on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        amm_tx_result.executed_transaction().id()
    );
    let _ = client.submit_transaction(amm_tx_result.clone()).await;
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 7: Consume the AMM input note
    // -------------------------------------------------------------------------
    println!("\n[STEP 7] Consume the AMM input note");

    let consume_amm_request = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(amm_input_note, None)])
        .build()?;
    let amm_consume_result = client
        .new_transaction(amm_contract.id(), consume_amm_request)
        .await?;
    println!(
        "AMM consume input note Tx on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        amm_consume_result.executed_transaction().id()
    );
    println!(
        "AMM account delta: {:?}",
        amm_consume_result.account_delta().vault()
    );
    let _ = client.submit_transaction(amm_consume_result).await;

    client.sync_state().await?;

    // Retrieve final AMM contract data
    let amm_account = client.get_account(amm_contract.id()).await?;
    println!(
        "AMM contract final storage: {:?}",
        amm_account.unwrap().account().storage().get_item(0)
    );

    println!("\n[TEST COMPLETE] AMM test completed successfully");
    println!(
        "- Created two faucets: A ({:?}) and B ({:?})",
        faucet_a_id, faucet_b_id
    );
    println!(
        "- Minted {} tokens from faucet A and {} tokens from faucet B",
        amount_a, amount_b
    );
    println!("- Created and consumed deposit notes for both assets");
    println!("- Created and consumed AMM input note");
    println!("- AMM contract ID: {:?}", amm_contract.id());

    Ok(())
}
