use ethers::providers::{Http, Provider};
use num_bigint::BigUint;
use revm::primitives::{B160, U256 as rU256};

use crate::structs_py::{
    AccountInfo, BlockHeader, SimulationError, SimulationParameters, SimulationResult, StateUpdate,
};
use pyo3::prelude::*;
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::runtime::Runtime;

use protosim::evm_simulation::{account_storage, database::SimulationDB, simulation};

fn get_runtime() -> Option<Arc<Runtime>> {
    let runtime = tokio::runtime::Handle::try_current()
        .is_err()
        .then(|| Runtime::new().unwrap())
        .unwrap();

    Some(Arc::new(runtime))
}

fn get_client(rpc_url: &str) -> Arc<Provider<Http>> {
    let client = Provider::<Http>::try_from(rpc_url).unwrap();
    Arc::new(client)
}

/// This class lets you simulate transactions.
///
/// Data will be queried from an Ethereum node*, if needed. You can also override account balance or
/// storage. See the methods.
///
/// *Currently the connection to a node is hardcoded. This will be changed in the future.
///
/// Attributes
/// ----------
/// block: Optional[BlockHeader]
///     Optional BlockHeader. If None, current block will be used.
/// trace: Optional[bool]
///     If set to true, simulations will print the entire execution trace.
#[pyclass]
pub struct SimulationEngine(simulation::SimulationEngine<Provider<Http>>);

#[pymethods]
impl SimulationEngine {
    #[new]
    fn new(rpc_url: &str, block: Option<BlockHeader>, trace: Option<bool>) -> Self {
        let block = block.map(protosim::evm_simulation::database::BlockHeader::from);
        let db = SimulationDB::new(get_client(rpc_url), get_runtime(), block);
        let engine = simulation::SimulationEngine {
            state: db,
            trace: trace.unwrap_or(false),
        };
        Self(engine)
    }

    /// Simulate transaction.
    ///
    /// Pass all details as an instance of `SimulationParameters`. See that class' docs for details.
    fn run_sim(
        mut self_: PyRefMut<Self>,
        params: SimulationParameters,
    ) -> PyResult<SimulationResult> {
        let rust_result = self_
            .0
            .simulate(&simulation::SimulationParameters::from(params));
        match rust_result {
            Ok(sim_res) => Ok(SimulationResult::from(sim_res)),
            Err(sim_err) => Err(PyErr::from(SimulationError::from(sim_err))),
        }
    }

    fn init_account(
        self_: PyRef<Self>,
        address: String,
        account: AccountInfo,
        mocked: bool,
        permanent_storage: Option<HashMap<BigUint, BigUint>>,
    ) {
        let address = B160::from_str(&address).unwrap();
        let account = revm::primitives::AccountInfo::from(account);

        let mut rust_slots: HashMap<rU256, rU256> = HashMap::new();
        if let Some(storage) = permanent_storage {
            for (index, value) in storage {
                rust_slots.insert(
                    rU256::from_str(&index.to_string()).unwrap(),
                    rU256::from_str(&value.to_string()).unwrap(),
                );
            }
        }

        self_
            .0
            .state
            .init_account(address, account, Some(rust_slots), mocked);
    }

    fn update_state(
        mut self_: PyRefMut<Self>,
        updates: HashMap<String, StateUpdate>,
        block: BlockHeader,
    ) -> PyResult<HashMap<String, StateUpdate>> {
        let block = protosim::evm_simulation::database::BlockHeader::from(block);
        let mut rust_updates: HashMap<B160, account_storage::StateUpdate> = HashMap::new();
        for (key, value) in updates {
            rust_updates.insert(
                B160::from_str(&key).unwrap(),
                account_storage::StateUpdate::from(value),
            );
        }

        let reverse_updates = self_.0.state.update_state(&rust_updates, block);

        let mut py_reverse_updates: HashMap<String, StateUpdate> = HashMap::new();
        for (key, value) in reverse_updates {
            py_reverse_updates.insert(key.to_string(), StateUpdate::from(value));
        }
        Ok(py_reverse_updates)
    }

    fn clear_temp_storage(mut self_: PyRefMut<Self>) {
        self_.0.state.clear_temp_storage();
    }
}
