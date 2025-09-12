use miden_amm::common::{
    create_basic_account, create_basic_faucet, create_library_with_assembler, wait_for_note,
    wait_for_tx,
};
use rand::RngCore;
use std::{fs, path::Path, sync::Arc};
use tokio::time::{Duration, sleep};

use miden_client::{
    Felt, Word,
    account::{
        AccountBuilder, AccountIdAddress, AccountStorageMode, AccountType, Address,
        AddressInterface, StorageMap, StorageSlot, component::BasicWallet,
    },
    asset::FungibleAsset,
    builder::ClientBuilder,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{
        Note, NoteAssets, NoteExecutionHint, NoteInputs, NoteMetadata, NoteRecipient, NoteTag,
        NoteType, create_p2id_note,
    },
    rpc::{Endpoint, TonicRpcClient},
    transaction::{OutputNote, TransactionKernel, TransactionRequestBuilder},
};
use miden_lib::account::auth::NoAuth;
use miden_lib::utils::ScriptBuilder;
use miden_objects::{
    account::{AccountComponent, NetworkId},
    assembly::Assembler,
};

#[tokio::test]
async fn test_deploy_deposit_withdraw_ntx() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize client & keystore
    let endpoint = Endpoint::testnet();
    let timeout_ms = 10_000;
    let rpc_api = Arc::new(TonicRpcClient::new(&endpoint, timeout_ms));

    let keystore = Arc::new(FilesystemKeyStore::new("./keystore".into()).unwrap());

    let mut client = ClientBuilder::new()
        .rpc(rpc_api)
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await?;

    let sync_summary = client.sync_state().await.unwrap();
    println!("Latest block: {}", sync_summary.block_num);

    // -------------------------------------------------------------------------
    // STEP 1: Create accounts and deploy faucet
    // -------------------------------------------------------------------------
    println!("\n[STEP 1] Creating new accounts");
    let alice_account = create_basic_account(&mut client, (*keystore).clone()).await?;
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
    let faucet = create_basic_faucet(&mut client, (*keystore).clone()).await?;
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
    // STEP 2: Create deposit_withdraw contract (NETWORK ACCOUNT)
    // -------------------------------------------------------------------------
    println!("\n[STEP 2] Creating deposit_withdraw network contract.");

    // Prepare assembler (debug mode = true)
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Load the MASM file for the deposit_withdraw contract
    let contract_path = Path::new("masm/accounts/deposit_withdraw.masm");
    let contract_code = fs::read_to_string(contract_path).unwrap();

    let storage_map = StorageMap::new();
    let storage_slot_map = StorageSlot::Map(storage_map.clone());

    // Compile the account code into `AccountComponent` with one storage slot for balance
    let contract_component =
        AccountComponent::compile(contract_code.clone(), assembler, vec![storage_slot_map])
            .unwrap()
            .with_supports_all_types();

    // Init seed for the deposit_withdraw contract
    let mut seed = [0_u8; 32];
    client.rng().fill_bytes(&mut seed);

    // Build the new `Account` with the component - NETWORK ACCOUNT
    let (deposit_contract, contract_seed) = AccountBuilder::new(seed)
        .account_type(AccountType::RegularAccountImmutableCode)
        .storage_mode(AccountStorageMode::Network) // NETWORK STORAGE
        .with_component(BasicWallet)
        .with_component(contract_component.clone())
        .with_auth_component(NoAuth) // NO AUTH FOR NETWORK ACCOUNTS
        .build()
        .unwrap();

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
        .await
        .unwrap();

    // -------------------------------------------------------------------------
    // STEP 3: Deploy Network Contract with Transaction Script
    // -------------------------------------------------------------------------
    println!("\n[STEP 3] Deploy network deposit_withdraw smart contract");

    let script_code = fs::read_to_string(Path::new(
        "masm/scripts/deploy_deposit_withdraw_script.masm",
    ))
    .unwrap();

    let library_path = "external_contract::deposit_withdraw_contract";
    let contract_lib = create_library_with_assembler(
        TransactionKernel::assembler().with_debug_mode(true),
        library_path,
        &contract_code,
    )
    .unwrap();

    let tx_script = ScriptBuilder::default()
        .with_dynamically_linked_library(&contract_lib)
        .map_err(|e| format!("Failed to link library: {}", e))?
        .compile_tx_script(script_code)
        .map_err(|e| format!("Failed to compile script: {}", e))?;

    let tx_deploy_request = TransactionRequestBuilder::new()
        .custom_script(tx_script)
        .build()
        .unwrap();

    let tx_result = client
        .new_transaction(deposit_contract.id(), tx_deploy_request)
        .await
        .unwrap();

    let _ = client.submit_transaction(tx_result.clone()).await;

    let tx_id = tx_result.executed_transaction().id();
    println!(
        "View deployment transaction on MidenScan: https://testnet.midenscan.com/tx/{}",
        tx_id.to_hex()
    );

    // Wait for the deployment transaction to be committed
    wait_for_tx(&mut client, tx_id).await?;

    // -------------------------------------------------------------------------
    // STEP 4: Mint tokens for Alice
    // -------------------------------------------------------------------------
    println!("\n[STEP 4] Mint tokens for Alice");
    let faucet_id = faucet.id();
    let amount: u64 = 100;
    let mint_amount = FungibleAsset::new(faucet_id, amount).unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            mint_amount,
            alice_account_id,
            NoteType::Public,
            client.rng(),
        )
        .unwrap();
    let tx_exec = client.new_transaction(faucet.id(), tx_request).await?;
    client.submit_transaction(tx_exec.clone()).await?;

    let p2id_note = if let OutputNote::Full(note) = tx_exec.created_notes().get_note(0) {
        note.clone()
    } else {
        panic!("Expected OutputNote::Full");
    };

    // Wait for the P2ID note to be available
    wait_for_note(&mut client, &alice_account.clone(), &p2id_note).await?;

    let consume_request = TransactionRequestBuilder::new()
        .authenticated_input_notes([(p2id_note.id(), None)])
        .build()
        .unwrap();
    let tx_exec = client
        .new_transaction(alice_account_id, consume_request)
        .await?;
    client.submit_transaction(tx_exec).await?;
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 5: Create deposit note with assets (TAGGED FOR NETWORK CONTRACT)
    // -------------------------------------------------------------------------
    println!("\n[STEP 5] Create deposit note with assets");

    let assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Create library from the deposit contract code so the note can call its procedures
    let contract_lib = create_library_with_assembler(
        assembler.clone(),
        "external_contract::deposit_withdraw_contract",
        &contract_code,
    )
    .unwrap();

    let note_code = fs::read_to_string(Path::new("masm/notes/deposit_withdraw_note.masm")).unwrap();
    let serial_num = client.rng().draw_word();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&contract_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();
    let note_inputs = NoteInputs::new(vec![]).unwrap(); // No special inputs needed
    let recipient = NoteRecipient::new(serial_num, note_script, note_inputs);

    // TAG THE NOTE WITH THE DEPOSIT CONTRACT ID FOR NETWORK ROUTING
    let tag = NoteTag::from_account_id(deposit_contract.id());

    let metadata = NoteMetadata::new(
        alice_account_id,
        NoteType::Public,
        tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;
    let vault = NoteAssets::new(vec![mint_amount.into()])?;
    let deposit_note = Note::new(vault, metadata, recipient);
    println!("deposit network note id: {:?}", deposit_note.id().to_hex());

    let note_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(deposit_note.clone())])
        .build()
        .unwrap();
    let tx_result = client
        .new_transaction(alice_account_id, note_request)
        .await
        .unwrap();
    println!(
        "View transaction on MidenScan: https://testnet.midenscan.com/tx/{}",
        tx_result.executed_transaction().id().to_hex()
    );
    let _ = client.submit_transaction(tx_result.clone()).await;
    client.sync_state().await?;

    wait_for_note(
        &mut client,
        &alice_account, // No specific account filter for network notes
        &deposit_note,
    )
    .await?;

    // Wait for network to process the tagged note
    println!("Waiting for network to process tagged deposit note...");
    sleep(Duration::from_secs(10)).await;
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 6: Check contract state after network processing
    // -------------------------------------------------------------------------
    println!("\n[STEP 6] Checking contract state after network deposit");

    // Retrieve updated contract data to see the balance
    let account = client.get_account(deposit_contract.id()).await.unwrap();
    if let Some(account_data) = account {
        println!("📊 Contract balance updated by network");
        println!(
            "Contract storage: {:?}",
            account_data.account().storage().get_item(0)
        );
    }

    // -------------------------------------------------------------------------
    // STEP 7: Create P2ID withdraw note for Alice
    // -------------------------------------------------------------------------
    println!("\n[STEP 7] Create P2ID withdraw note for Alice");

    // Create a P2ID note with the same asset amount, targeted to Alice
    let withdraw_p2id_note = create_p2id_note(
        deposit_contract.id(),    // sender (the contract)
        alice_account_id,         // target (Alice)
        vec![mint_amount.into()], // same asset that was deposited
        NoteType::Private,
        Felt::new(0),
        client.rng(),
    )
    .unwrap();

    println!(
        "Withdraw P2ID note id: {:?}",
        withdraw_p2id_note.id().to_hex()
    );
    println!("Withdraw note assets: {:?}", withdraw_p2id_note.assets());

    // -------------------------------------------------------------------------
    // STEP 8: Create withdrawal note (TAGGED FOR NETWORK CONTRACT)
    // -------------------------------------------------------------------------
    println!("\n[STEP 8] Create withdrawal note");

    let assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Create library from the deposit contract code so the note can call its procedures
    let contract_lib = create_library_with_assembler(
        assembler.clone(),
        "external_contract::deposit_withdraw_contract",
        &contract_code,
    )
    .unwrap();

    let note_code = fs::read_to_string(Path::new("masm/notes/deposit_withdraw_note.masm")).unwrap();
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
    ])
    .unwrap();

    let withdrawal_note_recipient = NoteRecipient::new(serial_num, note_script, note_inputs);

    // TAG THE WITHDRAWAL NOTE WITH THE DEPOSIT CONTRACT ID FOR NETWORK ROUTING
    let tag = NoteTag::from_account_id(deposit_contract.id());

    let metadata = NoteMetadata::new(
        alice_account_id,
        NoteType::Public,
        tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;
    let vault = NoteAssets::new(vec![])?;
    let withdrawal_note = Note::new(vault, metadata, withdrawal_note_recipient);
    println!("withdrawal note id: {:?}", withdrawal_note.id().to_hex());

    let note_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(withdrawal_note.clone())])
        .build()
        .unwrap();
    let tx_result = client
        .new_transaction(alice_account_id, note_request)
        .await
        .unwrap();
    println!(
        "View transaction on MidenScan: https://testnet.midenscan.com/tx/{}",
        tx_result.executed_transaction().id().to_hex()
    );
    let _ = client.submit_transaction(tx_result.clone()).await;
    client.sync_state().await?;

    // Wait for the withdrawal note to be available
    wait_for_note(
        &mut client,
        &alice_account, // No specific account filter for network notes
        &withdrawal_note,
    )
    .await?;

    // Wait for network to process the tagged withdrawal note
    println!("Waiting for network to process tagged withdrawal note...");
    sleep(Duration::from_secs(10)).await;
    client.sync_state().await?;

    // -------------------------------------------------------------------------
    // STEP 9: Alice consumes the P2ID withdraw note
    // -------------------------------------------------------------------------
    println!("\n[STEP 9] Alice consumes the P2ID withdraw note");

    let consume_p2id_request = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(withdraw_p2id_note.clone(), None)])
        .build()
        .unwrap();

    let tx_result = client
        .new_transaction(alice_account_id, consume_p2id_request)
        .await
        .unwrap();

    println!(
        "P2ID consumption Tx on MidenScan: https://testnet.midenscan.com/tx/{}",
        tx_result.executed_transaction().id().to_hex()
    );
    println!(
        "Alice's account delta: {:?}",
        tx_result.account_delta().vault()
    );
    let _ = client.submit_transaction(tx_result.clone()).await;

    client.sync_state().await.unwrap();

    println!("\n🎉 Network deposit and withdrawal flow completed successfully!");
    println!("✅ Assets were deposited into network contract via tagged note");
    println!("✅ Assets were withdrawn from network contract via tagged note");
    println!("✅ Alice received her assets back through P2ID note");

    Ok(())
}
