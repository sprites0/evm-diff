// Using rmp(rust-messagepack), read ~/abci_state.rmp.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, U256};
use clap::Parser;
use evm_diff::types::{AbciState, EvmBlock, EvmDb};
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
    AccountReader, DBProvider, DatabaseProviderFactory, ProviderFactory, ProviderResult,
    StateProvider, StateProviderFactory,
};
use std::collections::BTreeMap;
use std::fs::File;
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
    let file = File::open(args.file)?;
    let mut reader = std::io::BufReader::new(file);

    let abci_state: AbciState = rmp_serde::decode::from_read(&mut reader)?;
    let evm = abci_state.exchange.hyper_evm;
    let header = match &evm.latest_block2 {
        EvmBlock::Reth115(block) => block.header.clone(),
    };
    let block_number = header.number;

    let factory = get_reth_factory::<HlNode>(env).unwrap();
    let provider = BlockchainProvider::new(factory).unwrap();
    let db_provider = provider.database_provider_ro().unwrap();
    let state = provider
        .state_by_block_number_or_tag(BlockNumberOrTag::Number(header.number))
        .unwrap();
    {
        let EvmDb::InMemory {
            accounts,
            contracts,
        } = evm.state2.evm_db;
        let contracts = contracts
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        for (address, account) in tqdm::tqdm(accounts) {
            let account_in_db = state.basic_account(&address);
            match account_in_db {
                Ok(Some(account_in_db)) => {
                    assert_eq!(
                        account_in_db.balance, account.info.balance,
                        "{}:{}",
                        address, block_number,
                    );
                    assert_eq!(account_in_db.nonce, account.info.nonce, "{}", address);
                    assert_eq!(
                        account_in_db.get_bytecode_hash(),
                        account.info.code_hash,
                        "{}:{}",
                        address,
                        block_number
                    );

                    let contract_state = extract_contract_state(&db_provider, &state, address)
                        .unwrap()
                        .unwrap();
                    let expected = ContractState {
                        address,
                        account: account_in_db,
                        bytecode: state.account_code(&address).unwrap(),
                        storage: account
                            .storage
                            .into_iter()
                            .filter(|(_, v)| v != &U256::ZERO)
                            .map(|(k, v)| (k.into(), v.into()))
                            .collect(),
                    };
                    if contract_state.storage != expected.storage {
                        panic!(
                            "address: {:#?}\ncontract_state: {:#?}\nexpected: {:#?}",
                            address, contract_state, expected
                        );
                    }
                }
                Ok(Option::None) => {
                    assert_eq!(account.info.balance, U256::ZERO);
                    assert_eq!(account.info.nonce, 0);
                    assert_eq!(account.info.code_hash, KECCAK_EMPTY);
                    assert_eq!(account.storage.len(), 0);
                }
                Err(e) => {
                    println!("Error getting account: {:x}: {}", address, e);
                }
            }
        }
        for (code_hash, code) in tqdm::tqdm(contracts) {
            if code_hash == KECCAK_EMPTY {
                let code_in_db = state.bytecode_by_hash(&code_hash).unwrap();
                assert!(code_in_db.is_none() || code_in_db.unwrap().is_empty());
                continue;
            }
            let code_in_db = state.bytecode_by_hash(&code_hash).unwrap();
            match code_in_db {
                Some(code_in_db) => assert_eq!(code_in_db.original_bytes(), code.original_bytes()),
                None => {
                    if code_hash == B256::ZERO {
                        println!("WHAT {:?}", code.original_bytes());
                    } else {
                        panic!("Code not found: {:x}", code_hash);
                    }
                }
            }
        }
    }

    Ok(())
}
