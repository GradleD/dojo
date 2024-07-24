mod error;
mod state;
pub mod utils;

use std::num::NonZeroU128;

use blockifier::blockifier::block::{BlockInfo, GasPrices};
use blockifier::blockifier::config::TransactionExecutorConfig;
use blockifier::blockifier::transaction_executor::{TransactionExecutor, TransactionExecutorError};
use blockifier::bouncer::BouncerConfig;
use blockifier::context::BlockContext;
use blockifier::fee::fee_utils::get_fee_by_gas_vector;
use blockifier::state::cached_state::{self, MutRefState};
use blockifier::state::state_api::StateReader;
use blockifier::transaction::objects::FeeType;
use katana_cairo::starknet_api::block::{BlockNumber, BlockTimestamp};
use katana_cairo::starknet_api::transaction::Fee;
use katana_primitives::block::{ExecutableBlock, GasPrices as KatanaGasPrices, PartialHeader};
use katana_primitives::env::{BlockEnv, CfgEnv};
use katana_primitives::fee::TxFeeInfo;
use katana_primitives::transaction::{ExecutableTx, ExecutableTxWithHash, Tx, TxWithHash};
use katana_primitives::FieldElement;
use katana_provider::traits::state::StateProvider;
use starknet::core::types::PriceUnit;
use tracing::info;
use utils::to_executor_tx;

use self::state::CachedState;
use self::utils::get_fee_type_from_tx;
use crate::utils::build_receipt;
use crate::{
    EntryPointCall, ExecutionError, ExecutionOutput, ExecutionResult, ExecutionStats, Executor,
    ExecutorError, ExecutorExt, ExecutorFactory, ExecutorResult, ResultAndStates, SimulationFlag,
    StateProviderDb,
};

pub(crate) const LOG_TARGET: &str = "katana::executor::blockifier";

#[derive(Debug)]
pub struct BlockifierFactory {
    cfg: CfgEnv,
    flags: SimulationFlag,
}

impl BlockifierFactory {
    /// Create a new factory with the given configuration and simulation flags.
    pub fn new(cfg: CfgEnv, flags: SimulationFlag) -> Self {
        Self { cfg, flags }
    }
}

impl ExecutorFactory for BlockifierFactory {
    fn with_state<'a, P>(&self, state: P) -> Box<dyn Executor<'a> + 'a>
    where
        P: StateProvider + 'a,
    {
        self.with_state_and_block_env(state, BlockEnv::default())
    }

    fn with_state_and_block_env<'a, P>(
        &self,
        state: P,
        block_env: BlockEnv,
    ) -> Box<dyn Executor<'a> + 'a>
    where
        P: StateProvider + 'a,
    {
        let cfg_env = self.cfg.clone();
        let flags = self.flags.clone();
        Box::new(StarknetVMProcessor::new(Box::new(state), block_env, cfg_env, flags))
    }

    fn cfg(&self) -> &CfgEnv {
        &self.cfg
    }
}

pub struct StarknetVMProcessor<'a> {
    // block_context: BlockContext,
    transactions: Vec<(TxWithHash, ExecutionResult)>,
    simulation_flags: SimulationFlag,
    stats: ExecutionStats,

    state: CachedState<StateProviderDb<'a>>,
    executor: TransactionExecutor<CachedState<StateProviderDb<'a>>>,
}

impl<'a> StarknetVMProcessor<'a> {
    pub fn new(
        state: Box<dyn StateProvider + 'a>,
        block_env: BlockEnv,
        cfg_env: CfgEnv,
        simulation_flags: SimulationFlag,
    ) -> Self {
        let transactions = Vec::new();
        let block_context = utils::block_context_from_envs(&block_env, &cfg_env);
        let state = state::CachedState::new(StateProviderDb(state));

        let config = TransactionExecutorConfig::create_for_testing();
        let executor = TransactionExecutor::new(state.clone(), block_context, config);

        Self {
            // block_context,
            transactions,
            simulation_flags,
            stats: Default::default(),

            state,
            executor,
        }
    }

    fn fill_block_env_from_header(&mut self, header: &PartialHeader) {
        let number = BlockNumber(header.number);
        let timestamp = BlockTimestamp(header.timestamp);

        // TODO: should we enforce the gas price to not be 0,
        // as there's a flag to disable gas uasge instead?
        let eth_l1_gas_price =
            NonZeroU128::new(header.gas_prices.eth).unwrap_or(NonZeroU128::new(1).unwrap());
        let strk_l1_gas_price =
            NonZeroU128::new(header.gas_prices.strk).unwrap_or(NonZeroU128::new(1).unwrap());

        // TODO: which values is correct for those one?
        let eth_l1_data_gas_price = eth_l1_gas_price;
        let strk_l1_data_gas_price = strk_l1_gas_price;

        // TODO: @kariy, not sure here if we should add some functions to alter it
        // instead of cloning. Or did I miss a function?
        // https://github.com/starkware-libs/blockifier/blob/a6200402ab635d8a8e175f7f135be5914c960007/crates/blockifier/src/context.rs#L23
        let versioned_constants = self.executor.block_context.versioned_constants().clone();
        let chain_info = self.executor.block_context.chain_info().clone();
        let block_info = BlockInfo {
            block_number: number,
            block_timestamp: timestamp,
            sequencer_address: utils::to_blk_address(header.sequencer_address),
            gas_prices: GasPrices {
                eth_l1_gas_price,
                strk_l1_gas_price,
                eth_l1_data_gas_price,
                strk_l1_data_gas_price,
            },
            use_kzg_da: false,
        };

        // TODO: check what should be the value of the bouncer config
        self.executor.block_context =
            BlockContext::new(block_info, chain_info, versioned_constants, BouncerConfig::max());
    }

    fn simulate_with<F, T>(
        &self,
        transactions: Vec<ExecutableTxWithHash>,
        flags: &SimulationFlag,
        mut op: F,
    ) -> Vec<T>
    where
        F: FnMut(&mut dyn StateReader, (TxWithHash, ExecutionResult)) -> T,
    {
        let block_context = &self.executor.block_context;
        let state = &mut self.state.0.lock().inner;
        let mut state = cached_state::CachedState::new(MutRefState::new(state));

        let mut results = Vec::with_capacity(transactions.len());
        for exec_tx in transactions {
            let tx = TxWithHash::from(&exec_tx);
            let res = utils::transact(&mut state, block_context, flags, exec_tx);
            results.push(op(&mut state, (tx, res)));
        }

        results
    }
}

impl<'a> Executor<'a> for StarknetVMProcessor<'a> {
    fn execute_block(&mut self, block: ExecutableBlock) -> ExecutorResult<()> {
        self.fill_block_env_from_header(&block.header);
        self.execute_transactions(block.body)?;
        Ok(())
    }

    // bcs the executor is not writing to the CacheState directly, we need to call `.finalize()`
    // of the `TransactionExecutor` and apply the state diff to the cache state.
    fn execute_transactions(
        &mut self,
        transactions: Vec<ExecutableTxWithHash>,
    ) -> ExecutorResult<()> {
        let txs = transactions.clone().into_iter().map(utils::to_executor_tx).collect::<Vec<_>>();
        let results = self.executor.execute_txs(&txs);

        let mut is_full = false;
        // let txs = transactions.into_iter().map(TxWithHash::from).collect::<Vec<_>>();
        let mut execution_results = Vec::with_capacity(results.len());

        for (res, tx) in results.into_iter().zip(transactions.iter()) {
            println!("processing transaction");

            match res {
                Ok(info) => {
                    // Collect class artifacts if its a declare tx
                    let class_decl_artifacts = if let ExecutableTx::Declare(ref tx) = tx.as_ref() {
                        let class_hash = tx.class_hash();
                        Some((class_hash, tx.compiled_class.clone(), tx.sierra_class.clone()))
                    } else {
                        None
                    };

                    let fee_type = FeeType::Eth;

                    let fee = if info.transaction_receipt.fee == Fee(0) {
                        get_fee_by_gas_vector(
                            self.executor.block_context.block_info(),
                            info.transaction_receipt.gas,
                            &fee_type,
                        )
                    } else {
                        info.transaction_receipt.fee
                    };

                    let gas_consumed = info.transaction_receipt.gas.l1_gas;

                    let (unit, gas_price) = match fee_type {
                        FeeType::Eth => (
                            PriceUnit::Wei,
                            self.executor.block_context.block_info().gas_prices.eth_l1_gas_price,
                        ),
                        FeeType::Strk => (
                            PriceUnit::Fri,
                            self.executor.block_context.block_info().gas_prices.strk_l1_gas_price,
                        ),
                    };

                    let fee_info = TxFeeInfo {
                        gas_consumed,
                        gas_price: gas_price.into(),
                        unit,
                        overall_fee: fee.0,
                    };

                    let trace = utils::to_exec_info(info);
                    let receipt = build_receipt(tx.tx_ref(), fee_info, &trace);

                    crate::utils::log_resources(&trace.actual_resources);
                    crate::utils::log_events(receipt.events());

                    // let res = ExecutionResult::new_success(receipt, trace);

                    self.stats.l1_gas_used += receipt.fee().gas_consumed;
                    self.stats.cairo_steps_used +=
                        receipt.resources_used().vm_resources.n_steps as u128;

                    if let Some(reason) = receipt.revert_reason() {
                        info!(target: LOG_TARGET, %reason, "Transaction reverted.");
                    }

                    if let Some((class_hash, compiled, sierra)) = class_decl_artifacts {
                        self.state.0.lock().declared_classes.insert(class_hash, (compiled, sierra));
                    }

                    execution_results.push(ExecutionResult::new_success(receipt, trace));
                }

                Err(e) => match e {
                    TransactionExecutorError::StateError(e) => {
                        execution_results.push(ExecutionResult::new_failed(e));
                    }

                    TransactionExecutorError::TransactionExecutionError(e) => {
                        execution_results.push(ExecutionResult::new_failed(e));
                    }

                    TransactionExecutorError::BlockFull => {
                        // is_full = true;
                        println!("block is full");
                        break;
                    }
                },
            }
        }

        // let block_context = &self.block_context;
        // let flags = &self.simulation_flags;
        // let mut state = self.state.0.lock();

        // for exec_tx in transactions {
        //     // Collect class artifacts if its a declare tx
        //     let class_decl_artifacts = if let ExecutableTx::Declare(tx) = exec_tx.as_ref() {
        //         let class_hash = tx.class_hash();
        //         Some((class_hash, tx.compiled_class.clone(), tx.sierra_class.clone()))
        //     } else {
        //         None
        //     };

        //     let tx = TxWithHash::from(&exec_tx);
        //     let res = utils::transact(&mut state.inner, block_context, flags, exec_tx);

        //     match &res {
        //         ExecutionResult::Success { receipt, trace } => {
        //             self.stats.l1_gas_used += receipt.fee().gas_consumed;
        //             self.stats.cairo_steps_used +=
        //                 receipt.resources_used().vm_resources.n_steps as u128;

        //             if let Some(reason) = receipt.revert_reason() {
        //                 info!(target: LOG_TARGET, %reason, "Transaction reverted.");
        //             }

        //             if let Some((class_hash, compiled, sierra)) = class_decl_artifacts {
        //                 state.declared_classes.insert(class_hash, (compiled, sierra));
        //             }

        //             crate::utils::log_resources(&trace.actual_resources);
        //             crate::utils::log_events(receipt.events());
        //         }

        //         ExecutionResult::Failed { error } => {
        //             info!(target: LOG_TARGET, %error, "Executing transaction.");
        //         }
        //     };

        //     self.transactions.push((tx, res));
        // }

        let txs = dbg!(transactions.into_iter().map(TxWithHash::from).collect::<Vec<_>>());
        self.transactions = txs.into_iter().zip(dbg!(execution_results)).collect();

        Ok(())
    }

    fn take_execution_output(&mut self) -> ExecutorResult<ExecutionOutput> {
        let (output, ..) = self.executor.finalize().unwrap();

        let states = utils::state_update_from_cached_state(&self.state);
        let transactions = std::mem::take(&mut self.transactions);
        let stats = std::mem::take(&mut self.stats);
        Ok(ExecutionOutput { stats, states, transactions })
    }

    fn state(&self) -> Box<dyn StateProvider + 'a> {
        Box::new(self.state.clone())

        // todo!()
    }

    fn transactions(&self) -> &[(TxWithHash, ExecutionResult)] {
        &self.transactions
    }

    fn block_env(&self) -> BlockEnv {
        let eth_l1_gas_price = self.executor.block_context.block_info().gas_prices.eth_l1_gas_price;
        let strk_l1_gas_price =
            self.executor.block_context.block_info().gas_prices.strk_l1_gas_price;

        BlockEnv {
            number: self.executor.block_context.block_info().block_number.0,
            timestamp: self.executor.block_context.block_info().block_timestamp.0,
            sequencer_address: utils::to_address(
                self.executor.block_context.block_info().sequencer_address,
            ),
            l1_gas_prices: KatanaGasPrices {
                eth: eth_l1_gas_price.into(),
                strk: strk_l1_gas_price.into(),
            },
        }
    }
}

impl ExecutorExt for StarknetVMProcessor<'_> {
    fn simulate(
        &self,
        transactions: Vec<ExecutableTxWithHash>,
        flags: SimulationFlag,
    ) -> Vec<ResultAndStates> {
        self.simulate_with(transactions, &flags, |_, (_, result)| ResultAndStates {
            result,
            states: Default::default(),
        })
    }

    fn estimate_fee(
        &self,
        transactions: Vec<ExecutableTxWithHash>,
        flags: SimulationFlag,
    ) -> Vec<Result<TxFeeInfo, ExecutionError>> {
        self.simulate_with(transactions, &flags, |_, (_, res)| match res {
            ExecutionResult::Success { receipt, .. } => {
                // if the transaction was reverted, return as error
                if let Some(reason) = receipt.revert_reason() {
                    info!(target: LOG_TARGET, %reason, "Estimating fee.");
                    Err(ExecutionError::TransactionReverted { revert_error: reason.to_string() })
                } else {
                    Ok(receipt.fee().clone())
                }
            }

            ExecutionResult::Failed { error } => {
                info!(target: LOG_TARGET, %error, "Estimating fee.");
                Err(error)
            }
        })
    }

    fn call(&self, call: EntryPointCall) -> Result<Vec<FieldElement>, ExecutionError> {
        let block_context = &self.executor.block_context;
        let mut state = self.state.0.lock();
        let state = MutRefState::new(&mut state.inner);
        let retdata = utils::call(call, state, block_context, 1_000_000_000)?;
        Ok(retdata)

        // todo!()
    }
}
