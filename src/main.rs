// Using rmp(rust-messagepack), read ~/abci_state.rmp.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, U256};
use clap::Parser;
use evm_diff::types::{AbciState, DbAccountInfo, EvmBlock, EvmDb};
use reth_cli_commands::common::{AccessRights, CliNodeTypes, EnvironmentArgs};
use reth_db::cursor::{DbCursorRO, DbDupCursorRO};
use reth_db::transaction::DbTx;
use reth_db::{tables, DatabaseEnv};
use reth_hl::chainspec::parser::HlChainSpecParser;
use reth_hl::chainspec::HlChainSpec;
use reth_hl::node::HlNode;
use reth_hl::HlPrimitives;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_primitives::{Account, Bytecode};
use reth_provider::providers::BlockchainProvider;
use reth_provider::{
    DBProvider, DatabaseProviderFactory, ProviderFactory, ProviderResult,
    StateProvider, StateProviderFactory,
};
use rocksdb::{Options, DB};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

pub fn get_reth_factory<N: CliNodeTypes<ChainSpec = HlChainSpec, Primitives = HlPrimitives>>(
    env: EnvironmentArgs<HlChainSpecParser>,
) -> eyre::Result<ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>> {
    let env = env.init::<N>(AccessRights::RO)?;
    Ok(env.provider_factory)
}

#[derive(Parser)]
enum Subcommands {
    #[command(name = "diff")]
    Diff(EnvironmentArgs<HlChainSpecParser>),
}

#[derive(Parser)]
struct Args {
    /// Path to the abci state
    file: String,

    #[command(subcommand)]
    pub diff: Subcommands,
}

/// Represents the complete state of a contract including account info, bytecode, and storage
/// From https://github.com/paradigmxyz/reth/pull/17601
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractState {
    /// The address of the contract
    pub address: Address,
    /// Basic account information (balance, nonce, code hash)
    pub account: Account,
    /// Contract bytecode (None if not a contract or doesn't exist)
    pub bytecode: Option<Bytecode>,
    /// All storage slots for the contract
    pub storage: BTreeMap<B256, U256>,
}

/// Extract the full state of a specific contract
pub fn extract_contract_state<P: DBProvider>(
    provider: &P,
    state_provider: &dyn StateProvider,
    contract_address: Address,
) -> ProviderResult<Option<ContractState>> {
    let account = state_provider.basic_account(&contract_address)?;
    let Some(account) = account else {
        return Ok(None);
    };

    let bytecode = state_provider.account_code(&contract_address)?;

    let mut storage_cursor = provider
        .tx_ref()
        .cursor_dup_read::<tables::PlainStorageState>()?;
    let mut storage = BTreeMap::new();

    if let Some((_, first_entry)) = storage_cursor.seek_exact(contract_address)? {
        storage.insert(first_entry.key, first_entry.value);

        while let Some((_, entry)) = storage_cursor.next_dup()? {
            storage.insert(entry.key, entry.value);
        }
    }

    Ok(Some(ContractState {
        address: contract_address,
        account,
        bytecode,
        storage,
    }))
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let Subcommands::Diff(env) = args.diff;
    let abci_state_path: PathBuf = args.file.into();
    let file = File::open(&abci_state_path)?;
    let mut reader = std::io::BufReader::new(file);

    let abci_state: AbciState = rmp_serde::decode::from_read(&mut reader)?;
    let evm = abci_state.exchange.hyper_evm;
    let header = match &evm.latest_block2 {
        EvmBlock::Reth115(block) => block.header.clone(),
    };
    let block_number = header.number;
    eprintln!("EVM block number to compare: {block_number}");

    let factory = get_reth_factory::<HlNode>(env).unwrap();
    let provider = BlockchainProvider::new(factory).unwrap();
    let db_provider = provider.database_provider_ro().unwrap();
    let state = provider
        .state_by_block_number_or_tag(BlockNumberOrTag::Number(header.number))
        .unwrap();
    match evm.state2.evm_db {
        EvmDb::InMemory {
            accounts,
            contracts,
        } => {
            let contracts: HashMap<B256, evm_diff::types::Bytecode> = contracts.into_iter().collect();
            let reth_contracts: HashMap<B256, Bytecode> = contracts
                .iter()
                .map(|(h, c)| (*h, Bytecode::new_raw(c.original_bytes())))
                .collect();
            for (address, account) in tqdm::tqdm(accounts) {
                diff_account(
                    &db_provider,
                    &state,
                    block_number,
                    address,
                    &account.info,
                    account.storage.into_iter(),
                );
            }
            for (code_hash, code) in tqdm::tqdm(reth_contracts.iter()) {
                diff_contract(&state, *code_hash, code);
            }
        }
        EvmDb::NoEvmDb {} => {
            let home = std::env::var("HOME").unwrap();
            let db_path = PathBuf::from(home)
                .join("hl/hyperliquid_data/evm_db_hub_slow")
                .join("checkpoint")
                .join(abci_state.exchange.locus.context.height.to_string())
                .join("EvmState");
            println!("Opening RocksDB at {:?}", db_path);

            let prefix_extractor = rocksdb::SliceTransform::create_fixed_prefix(2);
            let mut opts = Options::default();
            opts.set_prefix_extractor(prefix_extractor);
            let db = DB::open(&opts, &db_path).unwrap();

            let mut contracts: HashMap<B256, Bytecode> = HashMap::new();
            for entry in db.prefix_iterator(b"\x45\x63") {
                let entry = entry.unwrap();
                let (key, value) = entry;
                let code_hash = B256::from_slice(&key[2..34]);
                let bytecode: evm_diff::types::Bytecode =
                    rmp_serde::from_slice(&value).unwrap();
                contracts.insert(code_hash, Bytecode::new_raw(bytecode.original_bytes()));
            }

            let mut storage_iterator = db.prefix_iterator(b"\x45\x73").peekable();
            let account_iter = db.prefix_iterator(b"\x45\x61").map(|entry| {
                let entry = entry.unwrap();
                let (key, value) = entry;
                let address = Address::from_slice(&key[2..22]);
                let info: DbAccountInfo = rmp_serde::from_slice(&value).unwrap();
                (address, info)
            });

            for (address, info) in tqdm::tqdm(account_iter.collect::<Vec<_>>()) {
                let mut current_storage: Vec<(U256, U256)> = Vec::new();
                loop {
                    let Some(entry) = storage_iterator.peek() else { break };
                    let entry = entry.as_ref().unwrap();
                    let (key, _value) = entry;
                    let storage_address = Address::from_slice(&key[2..22]);
                    if storage_address != address {
                        break;
                    }
                    let entry = storage_iterator.next().unwrap().unwrap();
                    let (key, value) = entry;
                    let storage_key = B256::from_slice(&key[22..54]);
                    let storage_value: B256 = rmp_serde::from_slice(&value).unwrap();
                    current_storage.push((storage_key.into(), storage_value.into()));
                }

                diff_account(
                    &db_provider,
                    &state,
                    block_number,
                    address,
                    &info,
                    current_storage.into_iter(),
                );
            }

            for (code_hash, code) in tqdm::tqdm(contracts.iter()) {
                diff_contract(&state, *code_hash, code);
            }
        }
    }

    Ok(())
}

fn diff_account<P: DBProvider>(
    db_provider: &P,
    state: &dyn StateProvider,
    block_number: u64,
    address: Address,
    info: &DbAccountInfo,
    storage: impl Iterator<Item = (U256, U256)>,
) {
    let account_in_db = state.basic_account(&address);
    match account_in_db {
        Ok(Some(account_in_db)) => {
            if account_in_db.balance != info.balance {
                eprintln!("\x1b[1mBalance mismatch for {}\x1b[0m (block {})", address, block_number);
                eprintln!("  \x1b[31m- reth: {}\x1b[0m", account_in_db.balance);
                eprintln!("  \x1b[32m+ abci: {}\x1b[0m", info.balance);
            }
            if account_in_db.nonce != info.nonce {
                eprintln!("\x1b[1mNonce mismatch for {}\x1b[0m", address);
                eprintln!("  \x1b[31m- reth: {}\x1b[0m", account_in_db.nonce);
                eprintln!("  \x1b[32m+ abci: {}\x1b[0m", info.nonce);
            }
            if account_in_db.get_bytecode_hash() != info.code_hash {
                eprintln!("\x1b[1mCode hash mismatch for {}\x1b[0m (block {})", address, block_number);
                eprintln!("  \x1b[31m- reth: {:#x}\x1b[0m", account_in_db.get_bytecode_hash());
                eprintln!("  \x1b[32m+ abci: {:#x}\x1b[0m", info.code_hash);
            }

            let contract_state = extract_contract_state(db_provider, state, address)
                .unwrap()
                .unwrap();
            let expected_storage: BTreeMap<B256, U256> = storage
                .filter(|(_, v)| v != &U256::ZERO)
                .map(|(k, v)| (k.into(), v.into()))
                .collect();
            if contract_state.storage != expected_storage {
                eprintln!("\x1b[1mStorage mismatch for {}\x1b[0m", address);
                for (key, reth_val) in &contract_state.storage {
                    match expected_storage.get(key) {
                        Some(abci_val) if abci_val != reth_val => {
                            eprintln!("  \x1b[33m~ {:#x}\x1b[0m", key);
                            eprintln!("    \x1b[31m- reth: {:#x}\x1b[0m", reth_val);
                            eprintln!("    \x1b[32m+ abci: {:#x}\x1b[0m", abci_val);
                        }
                        None => {
                            eprintln!("  \x1b[31m- {:#x} = {:#x}\x1b[0m (only in reth)", key, reth_val);
                        }
                        _ => {}
                    }
                }
                for (key, abci_val) in &expected_storage {
                    if !contract_state.storage.contains_key(key) {
                        eprintln!("  \x1b[32m+ {:#x} = {:#x}\x1b[0m (only in abci)", key, abci_val);
                    }
                }
            }
        }
        Ok(None) => {
            if info.balance != U256::ZERO || info.nonce != 0 || info.code_hash != KECCAK_EMPTY {
                eprintln!("\x1b[1mAccount {} not found in reth but exists in abci\x1b[0m", address);
                eprintln!("  balance: {}, nonce: {}, code_hash: {:#x}", info.balance, info.nonce, info.code_hash);
            }
        }
        Err(e) => {
            println!("Error getting account: {:x}: {}", address, e);
        }
    }
}

fn diff_contract(
    state: &dyn StateProvider,
    code_hash: B256,
    code: &Bytecode,
) {
    if code_hash == KECCAK_EMPTY {
        return;
    }
    let code_in_db = state.bytecode_by_hash(&code_hash).unwrap();
    match code_in_db {
        Some(code_in_db) => {
            if code_in_db.original_bytes() != code.original_bytes() {
                eprintln!("\x1b[1mBytecode mismatch for hash {:#x}\x1b[0m", code_hash);
            }
        }
        None => {
            eprintln!("\x1b[31mCode not found in reth: {:#x}\x1b[0m", code_hash);
        }
    }
}
