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
async fn test_deposit_withdraw_local() -> Result<()> {
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
    // STEP 1: Create accounts and deploy faucet
    // -------------------------------------------------------------------------
    println!("\n[STEP 1] Creating new accounts");
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

    println!("\nDeploying a new fungible faucet.");
    let faucet = create_basic_faucet(&mut client, keystore.clone()).await?;
    println!(
        "Faucet account ID: {:?}",
        Address::from(AccountIdAddress::new(
            faucet.id(),
            AddressInterface::Unspecified
        ))
        .to_bech32(NetworkId::Testnet)
    );
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 2: Create deposit_withdraw contract
    // -------------------------------------------------------------------------
    println!("\n[STEP 2] Creating deposit_withdraw contract.");

    // Prepare assembler (debug mode = true)
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Load the MASM file for the deposit_withdraw contract
    let contract_path = Path::new("masm/accounts/deposit_withdraw.masm");
    let contract_code = fs::read_to_string(contract_path)?;

    let storage_map = StorageMap::new();
    let storage_slot_map = StorageSlot::Map(storage_map.clone());

    // Compile the account code into `AccountComponent` with one storage slot for balance
    let contract_component =
        AccountComponent::compile(contract_code.clone(), assembler, vec![storage_slot_map])?
            .with_supports_all_types();

    // Init seed for the deposit_withdraw contract
    let mut seed = [0_u8; 32];
    client.rng().fill_bytes(&mut seed);

    let key_pair = SecretKey::with_rng(client.rng());

    // Build the new `Account` with the component
    let (deposit_contract, contract_seed) = AccountBuilder::new(seed)
        .account_type(AccountType::RegularAccountImmutableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_component(BasicWallet)
        .with_component(contract_component.clone())
        .with_auth_component(NoAuth)
        .build()?;

    println!(
        "deposit_contract commitment: {:?}",
        deposit_contract.commitment()
    );
    println!(
        "deposit_contract id: {:?}",
        Address::from(AccountIdAddress::new(
            faucet.id(),
            AddressInterface::Unspecified
        ))
        .to_bech32(NetworkId::Testnet)
    );
    println!("deposit_contract storage: {:?}", deposit_contract.storage());

    client
        .add_account(&deposit_contract.clone(), Some(contract_seed), false)
        .await?;

    keystore.add_key(&AuthSecretKey::RpoFalcon512(key_pair))?;

    // -------------------------------------------------------------------------
    // STEP 3: Mint tokens for Alice
    // -------------------------------------------------------------------------
    println!("\n[STEP 3] Mint tokens for Alice");
    let faucet_id = faucet.id();
    let amount: u64 = 100;
    let mint_amount = FungibleAsset::new(faucet_id, amount)?;
    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        mint_amount,
        alice_account_id,
        NoteType::Public,
        client.rng(),
    )?;

    let tx_exec = client.new_transaction(faucet.id(), tx_request).await?;
    client.submit_transaction(tx_exec.clone()).await?;

    let p2id_note = if let OutputNote::Full(note) = tx_exec.created_notes().get_note(0) {
        note.clone()
    } else {
        return Err(anyhow::anyhow!("Expected OutputNote::Full"));
    };

    // Wait for the P2ID note to be available

    wait_for_note(&mut client, &alice_account, &p2id_note).await?;

    let consume_request = TransactionRequestBuilder::new()
        .authenticated_input_notes([(p2id_note.id(), None)])
        .build()?;
    let tx_exec = client
        .new_transaction(alice_account_id, consume_request)
        .await?;
    client.submit_transaction(tx_exec).await?;
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 4: Create deposit note with assets
    // -------------------------------------------------------------------------
    println!("\n[STEP 4] Create deposit note with assets");

    let assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Create library from the deposit contract code so the note can call its procedures
    let contract_lib = create_library_with_assembler(
        assembler.clone(),
        "external_contract::deposit_withdraw_contract",
        &contract_code,
    )
    .map_err(|e| anyhow::anyhow!("Failed to create library: {}", e))?;

    let note_code = fs::read_to_string(Path::new("masm/notes/deposit_withdraw_note.masm"))?;
    let serial_num = client.rng().draw_word();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&contract_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let note_inputs = NoteInputs::new(vec![])?; // No special inputs needed
    let recipient = NoteRecipient::new(serial_num, note_script, note_inputs);
    let tag = NoteTag::for_public_use_case(0, 0, NoteExecutionMode::Local)?;
    let metadata = NoteMetadata::new(
        alice_account_id,
        NoteType::Public,
        tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;
    let vault = NoteAssets::new(vec![mint_amount.into()])?;
    let deposit_note = Note::new(vault, metadata, recipient);
    println!("deposit note hash: {:?}", deposit_note.id().to_hex());

    let note_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(deposit_note.clone())])
        .build()?;
    let tx_result = client
        .new_transaction(alice_account_id, note_request)
        .await?;
    println!(
        "View transaction on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result.executed_transaction().id()
    );
    let _ = client.submit_transaction(tx_result.clone()).await;
    client.sync_state().await?;

    wait_for_note(&mut client, &deposit_contract, &deposit_note).await?;

    // -------------------------------------------------------------------------
    // STEP 5: Consume the deposit note (deposit assets into contract)
    // -------------------------------------------------------------------------
    println!("\n[STEP 5] Deposit assets into the contract");

    // let note_args: Word = [Felt::new(0), Felt::new(0), Felt::new(0), Felt::new(1)].into();

    let consume_deposit_request = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(deposit_note, None)])
        .build()?;
    let tx_result = client
        .new_transaction(deposit_contract.id(), consume_deposit_request)
        .await?;
    println!(
        "Deposit Tx on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result.executed_transaction().id()
    );
    println!("account delta: {:?}", tx_result.account_delta().vault());
    let _ = client.submit_transaction(tx_result).await;

    client.sync_state().await?;

    // Retrieve updated contract data to see the balance
    let account = client.get_account(deposit_contract.id()).await?;
    println!(
        "deposit contract balance: {:?}",
        account.unwrap().account().storage().get_item(0)
    );

    // -------------------------------------------------------------------------
    // STEP 6: Create P2ID withdraw note for Alice
    // -------------------------------------------------------------------------
    println!("\n[STEP 6] Create P2ID withdraw note for Alice");

    // Create a P2ID note with the same asset amount, targeted to Alice
    let withdraw_p2id_note = create_p2id_note(
        deposit_contract.id(),    // sender (the contract)
        alice_account_id,         // target (Alice)
        vec![mint_amount.into()], // same asset that was deposited
        NoteType::Private,
        Felt::new(0),
        client.rng(),
    )?;

    println!(
        "Withdraw P2ID note hash: {:?}",
        withdraw_p2id_note.id().to_hex()
    );
    println!("Withdraw note assets: {:?}", withdraw_p2id_note.assets());

    // -------------------------------------------------------------------------
    // STEP 7: Create withdrawal note
    // -------------------------------------------------------------------------
    println!("\n[STEP 7] Create withdrawal note with assets");

    let assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Create library from the deposit contract code so the note can call its procedures
    let contract_lib = create_library_with_assembler(
        assembler.clone(),
        "external_contract::deposit_withdraw_contract",
        &contract_code,
    )
    .map_err(|e| anyhow::anyhow!("Failed to create library: {}", e))?;

    let note_code = fs::read_to_string(Path::new("masm/notes/deposit_withdraw_note.masm"))?;
    let serial_num = client.rng().draw_word();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&contract_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let p2id_withdraw_recipient: Word = withdraw_p2id_note.recipient().digest().into();

    let note_inputs = NoteInputs::new(vec![
        p2id_withdraw_recipient[0],
        p2id_withdraw_recipient[1],
        p2id_withdraw_recipient[2],
        p2id_withdraw_recipient[3],
        withdraw_p2id_note.metadata().execution_hint().into(),
        withdraw_p2id_note.metadata().note_type().into(),
        Felt::new(0),
        withdraw_p2id_note.metadata().tag().into(),
    ])?;

    let withdrawal_note_recipient = NoteRecipient::new(serial_num, note_script, note_inputs);
    let tag = NoteTag::for_public_use_case(0, 0, NoteExecutionMode::Local)?;
    let metadata = NoteMetadata::new(
        alice_account_id,
        NoteType::Public,
        tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;
    let vault = NoteAssets::new(vec![])?;
    let withdrawal_note = Note::new(vault, metadata, withdrawal_note_recipient);
    println!("deposit note hash: {:?}", withdrawal_note.id().to_hex());

    let note_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(withdrawal_note.clone())])
        .build()?;
    let tx_result = client
        .new_transaction(alice_account_id, note_request)
        .await?;
    println!(
        "View transaction on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result.executed_transaction().id()
    );
    let _ = client.submit_transaction(tx_result.clone()).await;
    client.sync_state().await?;

    // Wait for the withdrawal note to be available

    wait_for_note(&mut client, &alice_account, &withdrawal_note).await?;

    // -------------------------------------------------------------------------
    // STEP 8: Consume the withdrawal note
    // -------------------------------------------------------------------------
    println!("\n[STEP 8] Consume the withdrawal note");

    // let note_args: Word = [Felt::new(0), Felt::new(0), Felt::new(0), Felt::new(0)].into();

    let consume_deposit_request = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(withdrawal_note.clone(), None)])
        .build()?;
    let tx_result = client
        .new_transaction(deposit_contract.id(), consume_deposit_request)
        .await?;
    println!(
        "Deposit Tx on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result.executed_transaction().id()
    );
    println!("account delta: {:?}", tx_result.account_delta().vault());
    let _ = client.submit_transaction(tx_result.clone()).await;

    client.sync_state().await?;

    // Retrieve updated contract data to see the balance
    let account = client.get_account(deposit_contract.id()).await?;
    println!(
        "deposit contract balance: {:?}",
        account.unwrap().account().storage().get_item(0)
    );

    wait_for_note(&mut client, &alice_account, &withdrawal_note).await?;

    // -------------------------------------------------------------------------
    // STEP 9: Consume the private p2id note
    // -------------------------------------------------------------------------
    println!("\n[STEP 9] Consume the output p2id note");

    let consume_deposit_request = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(withdraw_p2id_note.clone(), None)])
        .build()?;
    let tx_result = client
        .new_transaction(alice_account_id, consume_deposit_request)
        .await?;
    println!(
        "Deposit Tx on MidenScan: https://testnet.midenscan.com/tx/{:?}",
        tx_result.executed_transaction().id()
    );
    println!("account delta: {:?}", tx_result.account_delta().vault());
    let _ = client.submit_transaction(tx_result.clone()).await;

    client.sync_state().await?;

    // Retrieve updated contract data to see the balance
    let account = client.get_account(deposit_contract.id()).await?;
    println!(
        "deposit contract balance: {:?}",
        account.unwrap().account().storage().get_item(0)
    );

    Ok(())
}
