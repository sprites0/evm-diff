// Using rmp(rust-messagepack), read ~/abci_state.rmp.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
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
use std::sync::Arc;
use std::{fs::File, io::Write};

pub fn get_reth_factory<N: CliNodeTypes<ChainSpec = HlChainSpec, Primitives = HlPrimitives>>(
    env: EnvironmentArgs<HlChainSpecParser>,
) -> eyre::Result<ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>> {
    let env = env.init::<N>(AccessRights::RO)?;
    Ok(env.provider_factory)
}

#[derive(Parser)]
struct Args {
    /// Path to the abci state
    file: String,
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
        EvmBlock::Reth115(block) => block.header(),
    };

    let jsonl_output = format!("{}.jsonl", header.number);
    let env = EnvironmentArgs::<HlChainSpecParser>::parse();
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
        let file = File::create(&jsonl_output)?;
        let mut file = std::io::BufWriter::new(file);
        writeln!(
            file,
            "{}",
            serde_json::to_string(&StateRoot { root: B256::ZERO })?
        )?;
        for (address, account) in accounts {
            let account_in_db = state.basic_account(&address).unwrap().unwrap();
            // check eq
            assert_eq!(account_in_db.balance, account.info.balance);
            assert_eq!(account_in_db.nonce, account.info.nonce);
            assert_eq!(account_in_db.bytecode_hash, Some(account.info.code_hash));

            for (key, value) in account.storage {
                let storage_in_db = state.storage(address, key.into()).unwrap().unwrap();
                assert_eq!(storage_in_db, value.into());
            }
        }
        for (code_hash, code) in contracts {
            let code_in_db = state.bytecode_by_hash(&code_hash).unwrap().unwrap();
            assert_eq!(code_in_db.original_bytes(), code.original_bytes());
        }
    }

    Ok(())
}
