use miden_assembly::{
    Assembler, DefaultSourceManager, Library, LibraryPath,
    ast::{Module, ModuleKind},
};
use miden_lib::account::auth;
use rand::{RngCore, rngs::StdRng};
use std::{fs, path::Path, sync::Arc};
use tokio::time::{Duration, sleep};

use miden_client::{
    Client, ClientError, Felt, ScriptBuilder, Word,
    account::{
        Account, AccountBuilder, AccountId, AccountStorageMode, AccountType, StorageSlot,
        component::{AuthRpoFalcon512, BasicFungibleFaucet, BasicWallet},
    },
    asset::{Asset, FungibleAsset, TokenSymbol},
    auth::AuthSecretKey,
    crypto::SecretKey,
    keystore::FilesystemKeyStore,
    note::{
        Note, NoteAssets, NoteExecutionHint, NoteInputs, NoteMetadata, NoteRecipient,
        NoteRelevance, NoteTag, NoteType, build_p2id_recipient,
    },
    store::{InputNoteRecord, TransactionFilter},
    transaction::{
        OutputNote, TransactionId, TransactionKernel, TransactionRequestBuilder, TransactionStatus,
    },
};
use miden_objects::account::AccountComponent;
use serde::de::value::Error;

// Helper to create a basic account
pub async fn create_basic_account(
    client: &mut Client<FilesystemKeyStore<rand::prelude::StdRng>>,
    keystore: FilesystemKeyStore<StdRng>,
) -> Result<Account, ClientError> {
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = SecretKey::with_rng(client.rng());
    let builder = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key()))
        .with_component(BasicWallet);
    let (account, seed) = builder.build().unwrap();
    client.add_account(&account, Some(seed), false).await?;
    keystore
        .add_key(&AuthSecretKey::RpoFalcon512(key_pair))
        .unwrap();
    Ok(account)
}

pub async fn create_basic_faucet(
    client: &mut Client<FilesystemKeyStore<rand::prelude::StdRng>>,
    keystore: FilesystemKeyStore<StdRng>,
) -> Result<miden_client::account::Account, ClientError> {
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = SecretKey::with_rng(client.rng());
    let symbol = TokenSymbol::new("MID").unwrap();
    let decimals = 8;
    let max_supply = Felt::new(1_000_000);
    let builder = AccountBuilder::new(init_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key()))
        .with_component(BasicFungibleFaucet::new(symbol, decimals, max_supply).unwrap());
    let (account, seed) = builder.build().unwrap();
    client.add_account(&account, Some(seed), false).await?;
    keystore
        .add_key(&AuthSecretKey::RpoFalcon512(key_pair))
        .unwrap();
    Ok(account)
}

/// Creates [num_accounts] accounts, [num_faucets] faucets, and mints the given [balances].
///
/// - `balances[a][f]`: how many tokens faucet `f` should mint for account `a`.
/// - Returns: a tuple of `(Vec<Account>, Vec<Account>)` i.e. (accounts, faucets).
pub async fn setup_accounts_and_faucets(
    client: &mut Client<FilesystemKeyStore<rand::prelude::StdRng>>,
    keystore: FilesystemKeyStore<StdRng>,
    num_accounts: usize,
    num_faucets: usize,
    balances: Vec<Vec<u64>>,
) -> Result<(Vec<Account>, Vec<Account>), ClientError> {
    // ---------------------------------------------------------------------
    // 1)  Create basic accounts
    // ---------------------------------------------------------------------
    let mut accounts = Vec::with_capacity(num_accounts);
    for i in 0..num_accounts {
        let account = create_basic_account(client, keystore.clone()).await?;
        println!("Created Account #{i} ⇒ ID: {:?}", account.id().to_hex());
        accounts.push(account);
    }

    // ---------------------------------------------------------------------
    // 2)  Create basic faucets
    // ---------------------------------------------------------------------
    let mut faucets = Vec::with_capacity(num_faucets);
    for j in 0..num_faucets {
        let faucet = create_basic_faucet(client, keystore.clone()).await?;
        println!("Created Faucet #{j} ⇒ ID: {:?}", faucet.id().to_hex());
        faucets.push(faucet);
    }

    // Tell the client about the new accounts/faucets
    client.sync_state().await?;

    // ---------------------------------------------------------------------
    // 3)  Mint tokens
    // ---------------------------------------------------------------------
    // `minted_notes[i]` collects the notes minted **for** `accounts[i]`
    let mut minted_notes: Vec<Vec<Note>> = vec![Vec::new(); num_accounts];

    for (acct_idx, account) in accounts.iter().enumerate() {
        for (faucet_idx, faucet) in faucets.iter().enumerate() {
            let amount = balances[acct_idx][faucet_idx];
            if amount == 0 {
                continue;
            }

            println!("Minting {amount} tokens from Faucet #{faucet_idx} to Account #{acct_idx}");

            // Build & submit the mint transaction
            let asset = FungibleAsset::new(faucet.id(), amount).unwrap();
            let tx_request = TransactionRequestBuilder::new()
                .build_mint_fungible_asset(asset, account.id(), NoteType::Public, client.rng())
                .unwrap();

            let tx_exec = client.new_transaction(faucet.id(), tx_request).await?;
            client.submit_transaction(tx_exec.clone()).await?;

            // Remember the freshly-created note so we can consume it later
            let minted_note = match tx_exec.created_notes().get_note(0) {
                OutputNote::Full(n) => n.clone(),
                _ => panic!("Expected OutputNote::Full, got something else"),
            };
            minted_notes[acct_idx].push(minted_note);
        }
    }

    // ---------------------------------------------------------------------
    // 4)  ONE wait-phase – ensure every account can now see all its notes
    // ---------------------------------------------------------------------
    for (acct_idx, account) in accounts.iter().enumerate() {
        let expected = minted_notes[acct_idx].len();
        if expected > 0 {
            wait_for_notes(client, account, expected).await?;
        }
    }
    client.sync_state().await?;

    // ---------------------------------------------------------------------
    // 5)  Consume notes so the tokens live in the public vaults
    // ---------------------------------------------------------------------
    for (acct_idx, account) in accounts.iter().enumerate() {
        for note in &minted_notes[acct_idx] {
            let consume_req = TransactionRequestBuilder::new()
                .authenticated_input_notes([(note.id(), None)])
                .build()
                .unwrap();

            let tx_exec = client.new_transaction(account.id(), consume_req).await?;
            client.submit_transaction(tx_exec).await?;
        }
    }
    client.sync_state().await?;

    Ok((accounts, faucets))
}

pub async fn wait_for_notes(
    client: &mut Client<FilesystemKeyStore<rand::prelude::StdRng>>,
    account_id: &miden_client::account::Account,
    expected: usize,
) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;
        let notes = client.get_consumable_notes(Some(account_id.id())).await?;
        if notes.len() >= expected {
            break;
        }
        println!(
            "{} consumable notes found for account {}. Waiting...",
            notes.len(),
            account_id.id().to_hex()
        );
        sleep(Duration::from_secs(3)).await;
    }
    Ok(())
}

pub async fn create_testing_amm_account(pool_x: Asset, pool_y: Asset) -> Result<Account, Error> {
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Load the MASM file for the counter contract
    let amm_path = Path::new("masm/accounts/amm.masm");
    let amm_code = fs::read_to_string(amm_path).unwrap();

    // Load the MASM file for the counter contract
    let deposit_withdraw_path = Path::new("masm/accounts/liquidity.masm");
    let deposit_withdraw_code = fs::read_to_string(deposit_withdraw_path).unwrap();

    let swap_component = AccountComponent::compile(
        amm_code.to_string(),
        assembler.clone(),
        vec![StorageSlot::Value(
            [Felt::new(0), Felt::new(0), Felt::new(0), Felt::new(0)].into(),
        )],
    )
    .unwrap()
    .with_supports_all_types();

    let lp_component = AccountComponent::compile(
        deposit_withdraw_code.to_string(),
        assembler.clone(),
        vec![StorageSlot::Value(
            [Felt::new(0), Felt::new(0), Felt::new(0), Felt::new(0)].into(),
        )],
    )
    .unwrap()
    .with_supports_all_types();

    let symbol: TokenSymbol = TokenSymbol::new("LP").unwrap();
    let decimals = 8u8;
    let max_supply = Felt::new(1_000_000u64);

    let amm_contract = AccountBuilder::new([3u8; 32])
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(auth::NoAuth)
        .with_component(swap_component.clone())
        .with_component(lp_component)
        .with_component(BasicWallet)
        .with_component(BasicFungibleFaucet::new(symbol, decimals, max_supply).unwrap())
        .with_assets(vec![pool_x, pool_y])
        .build_existing()
        .unwrap();

    Ok(amm_contract)
}

pub async fn create_testnet_amm_account() -> Result<(Account, Word), Error> {
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);

    // Load the MASM file for the counter contract
    let amm_path = Path::new("masm/accounts/amm.masm");
    let amm_code = fs::read_to_string(amm_path).unwrap();

    // Load the MASM file for the counter contract
    let deposit_withdraw_path = Path::new("masm/accounts/liquidity.masm");
    let deposit_withdraw_code = fs::read_to_string(deposit_withdraw_path).unwrap();

    let swap_component = AccountComponent::compile(
        amm_code.to_string(),
        assembler.clone(),
        vec![StorageSlot::Value(
            [Felt::new(0), Felt::new(0), Felt::new(0), Felt::new(0)].into(),
        )],
    )
    .unwrap()
    .with_supports_all_types();

    let lp_component = AccountComponent::compile(
        deposit_withdraw_code.to_string(),
        assembler.clone(),
        vec![StorageSlot::Value(
            [Felt::new(0), Felt::new(0), Felt::new(0), Felt::new(0)].into(),
        )],
    )
    .unwrap()
    .with_supports_all_types();

    let symbol: TokenSymbol = TokenSymbol::new("LP").unwrap();
    let decimals = 8u8;
    let max_supply = Felt::new(1_000_000u64);

    let (amm_contract, amm_seed) = AccountBuilder::new([3u8; 32])
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Network)
        .with_auth_component(auth::NoAuth)
        .with_component(swap_component.clone())
        .with_component(lp_component)
        .with_component(BasicWallet)
        .with_component(BasicFungibleFaucet::new(symbol, decimals, max_supply).unwrap())
        .build()
        .unwrap();

    Ok((amm_contract, amm_seed))
}

pub fn create_exact_p2id_note(
    sender: AccountId,
    target: AccountId,
    assets: Vec<Asset>,
    serial_num: Word,
) -> Result<Note, Error> {
    let recipient = build_p2id_recipient(target, serial_num).unwrap();

    let tag = NoteTag::from_account_id(target);

    let metadata = NoteMetadata::new(
        sender,
        NoteType::Private,
        tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )
    .unwrap();
    let vault = NoteAssets::new(assets).unwrap();

    Ok(Note::new(vault, metadata, recipient))
}

pub async fn create_amm_input_note(
    creator_account: AccountId,
    amm_account: AccountId,
    asset_in: FungibleAsset,
    asset_out: FungibleAsset,
) -> Result<Note, Error> {
    let note_code = fs::read_to_string(Path::new("masm/notes/amm_input_note.masm")).unwrap();
    let account_code = fs::read_to_string(Path::new("masm/accounts/amm.masm")).unwrap();

    let library_path = "external_contract::amm_contract";
    let library = create_library(account_code, library_path).unwrap();

    let swap_note_in_serial_num = Word::default();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&library)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let p2id_output = create_exact_p2id_note(
        amm_account,
        creator_account,
        vec![],
        swap_note_in_serial_num,
    )
    .unwrap();

    let note_inputs = NoteInputs::new(
        [
            Felt::new(p2id_output.metadata().execution_hint().into()),
            Felt::try_from(p2id_output.metadata().note_type()).unwrap(),
            Felt::try_from(p2id_output.metadata().aux()).unwrap(),
            Felt::new(p2id_output.metadata().tag().as_u32().into()),
            p2id_output.recipient().digest()[3],
            p2id_output.recipient().digest()[2],
            p2id_output.recipient().digest()[1],
            p2id_output.recipient().digest()[0],
            Felt::new(asset_out.amount()),
            Felt::new(0),
            asset_out.faucet_id().suffix().into(),
            asset_out.faucet_id().prefix().into(),
        ]
        .to_vec(),
    )
    .unwrap();
    let recipient = NoteRecipient::new(swap_note_in_serial_num, note_script, note_inputs.clone());

    let tag = NoteTag::from_account_id(amm_account);
    let metadata = NoteMetadata::new(
        creator_account,
        NoteType::Public,
        tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )
    .unwrap();

    let note_asset = NoteAssets::new(vec![asset_in.into()]).unwrap();
    let note = Note::new(note_asset, metadata, recipient);

    Ok(note)
}

pub fn create_library(
    account_code: String,
    library_path: &str,
) -> Result<Library, Box<dyn std::error::Error>> {
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);
    let source_manager = Arc::new(DefaultSourceManager::default());
    let module = Module::parser(ModuleKind::Library).parse_str(
        LibraryPath::new(library_path)?,
        account_code,
        &source_manager,
    )?;
    let library = assembler.clone().assemble_library([module])?;
    Ok(library)
}

// Alternative create_library function with assembler parameter (used in some tests)
pub fn create_library_with_assembler(
    assembler: Assembler,
    library_path: &str,
    source_code: &str,
) -> Result<Library, Box<dyn std::error::Error>> {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let module = Module::parser(ModuleKind::Library).parse_str(
        LibraryPath::new(library_path)?,
        source_code,
        &source_manager,
    )?;
    let library = assembler.clone().assemble_library([module])?;
    Ok(library)
}

/// Waits for a specific note to become available in the client's state.
pub async fn wait_for_note(
    client: &mut Client<FilesystemKeyStore<rand::prelude::StdRng>>,
    account_id: &Account,
    expected: &Note,
) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;

        let notes: Vec<(InputNoteRecord, Vec<(AccountId, NoteRelevance)>)> =
            client.get_consumable_notes(Some(account_id.id())).await?;

        let found = notes.iter().any(|(rec, _)| rec.id() == expected.id());

        if found {
            println!("✅ note found {}", expected.id().to_hex());
            break;
        }

        println!("Note {} not found. Waiting...", expected.id().to_hex());
        sleep(Duration::from_secs(3)).await;
    }
    Ok(())
}

/// Waits for a specific transaction to be committed.
pub async fn wait_for_tx(
    client: &mut Client<FilesystemKeyStore<rand::prelude::StdRng>>,
    tx_id: TransactionId,
) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;

        // Check transaction status
        let txs = client
            .get_transactions(TransactionFilter::Ids(vec![tx_id]))
            .await?;
        let tx_committed = if !txs.is_empty() {
            matches!(txs[0].status, TransactionStatus::Committed { .. })
        } else {
            false
        };

        if tx_committed {
            println!("✅ transaction {} committed", tx_id.to_hex());
            break;
        }

        println!(
            "Transaction {} not yet committed. Waiting...",
            tx_id.to_hex()
        );
        sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}
