// Using rmp(rust-messagepack), read ~/abci_state.rmp.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U256};
use clap::Parser;
use evm_diff::types::{AbciState, EvmBlock, EvmDb};
use reth_cli_commands::common::{AccessRights, CliNodeTypes, EnvironmentArgs};
use reth_db::DatabaseEnv;
use reth_hl::chainspec::parser::HlChainSpecParser;
use reth_hl::chainspec::HlChainSpec;
use reth_hl::node::HlNode;
use reth_hl::HlPrimitives;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::providers::BlockchainProvider;
use reth_provider::{AccountReader, ProviderFactory, StateProviderFactory};
use serde::{Deserialize, Serialize};
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

/// Type to deserialize state root from state dump file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StateRoot {
    root: B256,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let file = File::open(args.file)?;
    let mut reader = std::io::BufReader::new(file);

    let abci_state: AbciState = rmp_serde::decode::from_read(&mut reader)?;
    let evm = abci_state.exchange.hyper_evm;
    let header = match &evm.latest_block2 {
        EvmBlock::Reth115(block) => block.header.clone(),
    };

    let Subcommands::Diff(env) = args.diff;
    let factory = get_reth_factory::<HlNode>(env).unwrap();
    let provider = BlockchainProvider::new(factory).unwrap();
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
                    assert_eq!(account_in_db.balance, account.info.balance);
                    assert_eq!(account_in_db.nonce, account.info.nonce);
                    assert_eq!(account_in_db.get_bytecode_hash(), account.info.code_hash);
                }
                Ok(None) => {
                    assert_eq!(account.info.balance, U256::ZERO);
                    assert_eq!(account.info.nonce, 0);
                    assert_eq!(account.info.code_hash, KECCAK_EMPTY);
                    assert_eq!(account.storage.len(), 0);
                }
                Err(e) => {
                    println!("Error getting account: {:x}: {}", address, e);
                }
            }

            for (key, value) in account.storage {
                let storage_in_db = state.storage(address, key.into());
                match storage_in_db {
                    Ok(Some(storage)) => assert_eq!(storage, value.into()),
                    Ok(None) => assert_eq!(U256::ZERO, value),
                    Err(e) => panic!(
                        "Error getting storage: {}:{:x} at block {}",
                        e, address, header.number
                    ),
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
                    }
                    else {
                        panic!("Code not found: {:x}", code_hash);
                    }
                }
            }
        }
    }

    Ok(())
}
