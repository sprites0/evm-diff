use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{Address, B256, Bytes, Log, U256};
use reth_primitives::{SealedBlock, Transaction, Bytecode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockAndReceipts {
    pub block: EvmBlock,
    pub receipts: Vec<LegacyReceipt>,
    #[serde(default)]
    pub system_txs: Vec<SystemTx>,
    #[serde(default)]
    pub read_precompile_calls: Vec<(Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EvmBlock {
    Reth115(SealedBlock),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyReceipt {
    tx_type: LegacyTxType,
    success: bool,
    cumulative_gas_used: u64,
    logs: Vec<Log>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum LegacyTxType {
    Legacy = 0,
    Eip2930 = 1,
    Eip1559 = 2,
    Eip4844 = 3,
    Eip7702 = 4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemTx {
    pub tx: Transaction,
    pub receipt: Option<LegacyReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct ReadPrecompileInput {
    pub input: Bytes,
    pub gas_limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadPrecompileResult {
    Ok { gas_used: u64, bytes: Bytes },
    OutOfGas,
    Error,
    UnexpectedError,
}

#[derive(Deserialize)]
pub struct AbciState {
    pub exchange: Exchange,
}

#[derive(Deserialize)]
pub struct Exchange {
    pub hyper_evm: HyperEvm,
}

#[derive(Deserialize)]
pub struct HyperEvm {
    pub state2: EvmState,
    pub latest_block2: EvmBlock,
}

#[derive(Deserialize)]
pub struct EvmState {
    pub evm_db: EvmDb,
    pub block_hashes: Vec<(U256, B256)>,
}

#[derive(Deserialize)]
pub enum EvmDb {
    InMemory {
        accounts: Vec<(Address, DbAccount)>,
        contracts: Vec<(B256, Bytecode)>,
    },
}

#[derive(Deserialize, Clone)]
pub struct DbAccount {
    #[serde(rename = "i", alias = "info", default)]
    pub info: DbAccountInfo,
    #[serde(rename = "s", alias = "storage", default)]
    pub storage: Vec<(U256, U256)>,
}

#[derive(Deserialize, Clone)]
pub struct DbAccountInfo {
    #[serde(rename = "b", alias = "balance", default)]
    pub balance: U256,
    #[serde(rename = "n", alias = "nonce", default)]
    pub nonce: u64,
    #[serde(rename = "c", alias = "code_hash", default = "keccak_empty")]
    pub code_hash: B256,
}

impl Default for DbAccountInfo {
    fn default() -> Self {
        Self {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: KECCAK_EMPTY,
        }
    }
}

const fn keccak_empty() -> B256 {
    KECCAK_EMPTY
}
