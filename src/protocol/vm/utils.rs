// TODO: remove skip for clippy dead_code check
#![allow(dead_code)]
use ethabi::{self, decode, ParamType};

use ethers::{
    abi::Abi,
    core::utils::keccak256,
    types::{Address, H256},
};
use hex::FromHex;
use mini_moka::sync::Cache;
use reqwest::{blocking::Client, StatusCode};
use serde_json::json;
use std::{
    collections::HashMap,
    env,
    fs::File,
    io::Read,
    path::Path,
    sync::{Arc, LazyLock},
    time::Duration,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("HTTP Error: {0}")]
    Http(reqwest::Error),
    #[error("RPC Error: {0}. Status code: {1}")]
    Rpc(String, StatusCode),
    #[error("Invalid Response: {0}")]
    InvalidResponse(String),
    #[error("Out of Gas: {0}. Pool state: {1}")]
    OutOfGas(String, String),
}

pub fn maybe_coerce_error(
    err: RpcError,
    pool_state: &str,
    gas_limit: Option<u64>,
    gas_used: Option<u64>,
) -> Result<(), RpcError> {
    match err {
        // Check for revert situation (if error message starts with "0x")
        RpcError::InvalidResponse(ref details) if details.starts_with("0x") => {
            let reason = parse_solidity_error_message(details);
            let err = RpcError::InvalidResponse(format!("Revert! Reason: {}", reason));

            // Check if we are running out of gas
            if let (Some(gas_limit), Some(gas_used)) = (gas_limit, gas_used) {
                // if we used up 97% or more issue a OutOfGas error.
                let usage = gas_used as f64 / gas_limit as f64;
                if usage >= 0.97 {
                    return Err(RpcError::OutOfGas(
                        format!(
                            "SimulationError: Likely out-of-gas. Used {:.2}% of gas limit. Original error: {}",
                            usage * 100.0,
                            err
                        ),
                        pool_state.to_string(),
                    ));
                }
            }
            Err(err)
        }

        // Check if "OutOfGas" is part of the error message
        RpcError::InvalidResponse(ref details) if details.contains("OutOfGas") => {
            let usage_msg = if let (Some(gas_limit), Some(gas_used)) = (gas_limit, gas_used) {
                let usage = gas_used as f64 / gas_limit as f64;
                format!("Used: {:.2}% of gas limit. ", usage * 100.0)
            } else {
                String::new()
            };

            Err(RpcError::OutOfGas(
                format!("SimulationError: out-of-gas. {}Original error: {}", usage_msg, details),
                pool_state.to_string(),
            ))
        }

        // Otherwise return the original error
        _ => Err(err),
    }
}

fn parse_solidity_error_message(data: &str) -> String {
    let data_bytes = match Vec::from_hex(&data[2..]) {
        Ok(bytes) => bytes,
        Err(_) => return format!("Failed to decode: {}", data),
    };

    // Check for specific error selectors:
    // Solidity Error(string) signature: 0x08c379a0
    if data_bytes.starts_with(&[0x08, 0xc3, 0x79, 0xa0]) {
        if let Ok(decoded) = decode(&[ParamType::String], &data_bytes[4..]) {
            if let Some(ethabi::Token::String(error_string)) = decoded.first() {
                return error_string.clone();
            }
        }

        // Solidity Panic(uint256) signature: 0x4e487b71
    } else if data_bytes.starts_with(&[0x4e, 0x48, 0x7b, 0x71]) {
        if let Ok(decoded) = decode(&[ParamType::Uint(256)], &data_bytes[4..]) {
            if let Some(ethabi::Token::Uint(error_code)) = decoded.first() {
                let panic_codes = get_solidity_panic_codes();
                return panic_codes
                    .get(&error_code.as_u64())
                    .cloned()
                    .unwrap_or_else(|| format!("Panic({})", error_code));
            }
        }
    }

    // Try decoding as a string (old Solidity revert case)
    if let Ok(decoded) = decode(&[ParamType::String], &data_bytes) {
        if let Some(ethabi::Token::String(error_string)) = decoded.first() {
            return error_string.clone();
        }
    }

    // Custom error, try to decode string again with offset
    if let Ok(decoded) = decode(&[ParamType::String], &data_bytes[4..]) {
        if let Some(ethabi::Token::String(error_string)) = decoded.first() {
            return error_string.clone();
        }
    }

    // Fallback if no decoding succeeded
    format!("Failed to decode: {}", data)
}

pub type SlotHash = H256;

/// Get storage slot index of a value stored at a certain key in a mapping
///
/// # Arguments
///
/// * `key`: Key in a mapping. Can be any H160 value (such as an address).
/// * `mapping_slot`: An `H256` representing the storage slot at which the mapping itself is stored.
///   See the examples for more explanation.
///
/// # Returns
///
/// An `H256` representing the  index of a storage slot where the value at the given
/// key is stored.
///
/// # Examples
///
/// If a mapping is declared as a first variable in Solidity code, its storage slot
/// is 0 (e.g. `balances` in our mocked ERC20 contract). Here's how to compute
/// a storage slot where balance of a given account is stored:
///
/// ```
/// use protosim::protocol::vm::utils::{get_storage_slot_index_at_key, H256};
/// use ethers::types::Address;
/// let address: Address = "0xC63135E4bF73F637AF616DFd64cf701866BB2628".parse().expect("Invalid address");
/// get_storage_slot_index_at_key(address, H256::from_low_u64_be(0));
/// ```
///
/// For nested mappings, we need to apply the function twice. An example of this is
/// `allowances` in ERC20. It is a mapping of form:
/// `HashMap<Owner, HashMap<Spender, U256>>`. In our mocked ERC20 contract, `allowances`
/// is a second variable, so it is stored at slot 1. Here's how to get a storage slot
/// where an allowance of `address_spender` to spend `address_owner`'s money is stored:
///
/// ```
/// use protosim::protocol::vm::utils::{get_storage_slot_index_at_key, H256};
/// use ethers::types::Address;
/// let address_spender: Address = "0xC63135E4bF73F637AF616DFd64cf701866BB2628".parse().expect("Invalid address");
/// let address_owner: Address = "0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05".parse().expect("Invalid address");
/// get_storage_slot_index_at_key(address_spender, get_storage_slot_index_at_key(address_owner, H256::from_low_u64_be(1)));
/// ```
///
/// # See Also
///
/// [Solidity Storage Layout documentation](https://docs.soliditylang.org/en/v0.8.13/internals/layout_in_storage.html#mappings-and-dynamic-arrays)
pub fn get_storage_slot_index_at_key(key: Address, mapping_slot: SlotHash) -> SlotHash {
    let mut key_bytes = key.as_bytes().to_vec();
    key_bytes.resize(32, 0); // Right pad with zeros

    let mut mapping_slot_bytes = [0u8; 32];
    mapping_slot_bytes.copy_from_slice(mapping_slot.as_bytes());

    let slot_bytes = keccak256([&key_bytes[..], &mapping_slot_bytes[..]].concat());
    SlotHash::from_slice(&slot_bytes)
}

fn get_solidity_panic_codes() -> HashMap<u64, String> {
    let mut panic_codes = HashMap::new();
    panic_codes.insert(0, "GenericCompilerPanic".to_string());
    panic_codes.insert(1, "AssertionError".to_string());
    panic_codes.insert(17, "ArithmeticOver/Underflow".to_string());
    panic_codes.insert(18, "ZeroDivisionError".to_string());
    panic_codes.insert(33, "UnknownEnumMember".to_string());
    panic_codes.insert(34, "BadStorageByteArrayEncoding".to_string());
    panic_codes.insert(51, "EmptyArray".to_string());
    panic_codes.insert(0x32, "OutOfBounds".to_string());
    panic_codes.insert(0x41, "OutOfMemory".to_string());
    panic_codes.insert(0x51, "BadFunctionPointer".to_string());
    panic_codes
}

fn exec_rpc_method(
    url: &str,
    method: &str,
    params: Vec<serde_json::Value>,
    timeout: u64,
) -> Result<serde_json::Value, RpcError> {
    let client = Client::new();
    let payload = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1,
    });

    let response = client
        .post(url)
        .json(&payload)
        .timeout(Duration::from_secs(timeout))
        .send()
        .map_err(RpcError::Http)?;

    if response.status().is_client_error() || response.status().is_server_error() {
        let status = response.status();
        let body = response
            .text()
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(RpcError::Rpc(body, status));
    }

    let data: serde_json::Value = response
        .json()
        .map_err(RpcError::Http)?;

    if let Some(result) = data.get("result") {
        Ok(result.clone())
    } else if let Some(error) = data.get("error") {
        Err(RpcError::InvalidResponse(format!(
            "RPC failed to call {} with Error: {}",
            method, error
        )))
    } else {
        Ok(serde_json::Value::Null)
    }
}

pub fn get_code_for_address(
    address: &str,
    connection_string: Option<String>,
) -> Result<Option<Vec<u8>>, RpcError> {
    // Get the connection string, defaulting to the RPC_URL environment variable
    let connection_string = connection_string.or_else(|| env::var("RPC_URL").ok());

    let connection_string = match connection_string {
        Some(url) => url,
        None => {
            return Err(RpcError::InvalidResponse(
                "RPC_URL environment variable is not set".to_string(),
            ))
        }
    };

    let method = "eth_getCode";
    let params = vec![json!(address), json!("latest")];

    match exec_rpc_method(&connection_string, method, params, 240) {
        Ok(code) => {
            if let Some(code_str) = code.as_str() {
                let code_bytes = hex::decode(&code_str[2..]).map_err(|_| {
                    RpcError::InvalidResponse(format!(
                        "Failed to decode hex string for address {}",
                        address
                    ))
                })?;
                Ok(Some(code_bytes))
            } else {
                Ok(None)
            }
        }
        Err(e) => {
            println!("Error fetching code for address {}: {:?}", address, e);
            Err(e)
        }
    }
}

static BYTECODE_CACHE: LazyLock<Cache<Arc<String>, Vec<u8>>> = LazyLock::new(|| Cache::new(1_000));

pub fn get_contract_bytecode(path: &str) -> std::io::Result<Vec<u8>> {
    if let Some(bytecode) = BYTECODE_CACHE.get(&Arc::new(path.to_string())) {
        return Ok(bytecode.clone());
    }

    let mut file = File::open(Path::new(path))?;
    let mut code = Vec::new();

    file.read_to_end(&mut code)?;
    BYTECODE_CACHE.insert(Arc::new(path.to_string()), code.clone());

    Ok(code)
}

pub fn load_swap_abi() -> Result<Abi, std::io::Error> {
    let swap_abi_path = Path::new(file!())
        .parent()
        .unwrap()
        .join("assets")
        .join("ISwapAdapter.abi");

    let mut file = File::open(&swap_abi_path).expect("Failed to open the swap ABI file");
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .expect("Failed to read the swap ABI file");
    let abi: Abi = serde_json::from_str(&contents).expect("Swap ABI is malformed.");
    Ok(abi)
}

pub fn load_erc20_abi() -> Result<Abi, std::io::Error> {
    let erc20_abi_path = Path::new(file!())
        .parent()
        .unwrap()
        .join("assets")
        .join("ERC20.abi");

    let mut file = File::open(&erc20_abi_path).expect("Failed to open the ERC20 ABI file");
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .expect("Failed to read the ERC20 ABI file");

    let abi: Abi = serde_json::from_str(&contents).expect("ERC20 ABI is malformed.");
    Ok(abi)
}

#[cfg(test)]
mod tests {
    use dotenv::dotenv;
    use std::{fs::remove_file, io::Write};
    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    #[cfg_attr(not(feature = "network_tests"), ignore)]
    fn test_get_code_for_address() {
        let rpc_url = env::var("ETH_RPC_URL").unwrap_or_else(|_| {
            dotenv().expect("Missing .env file");
            env::var("ETH_RPC_URL").expect("Missing ETH_RPC_URL in .env file")
        });

        let address = "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640";
        let result = get_code_for_address(address, Some(rpc_url));

        assert!(result.is_ok(), "Network call should not fail");

        let code_bytes = result.unwrap();
        match code_bytes {
            Some(bytes) => {
                assert!(!bytes.is_empty(), "Code should not be empty");
            }
            None => {
                panic!("There should be some code for the address");
            }
        }
    }

    #[test]
    fn test_maybe_coerce_error_revert_no_gas_info() {
        let err = RpcError::InvalidResponse("0x08c379a000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000011496e76616c6964206f7065726174696f6e000000000000000000000000000000".to_string());

        let result = maybe_coerce_error(err, "test_pool", None, None);

        assert!(result.is_err());
        if let Err(RpcError::InvalidResponse(message)) = result {
            assert!(message.contains("Revert! Reason: Invalid operation"));
        } else {
            panic!("Expected InvalidResponse error");
        }
    }

    #[test]
    fn test_maybe_coerce_error_out_of_gas() {
        // Test out-of-gas situation with gas limit and gas used provided
        let err = RpcError::InvalidResponse("OutOfGas".to_string());

        let result = maybe_coerce_error(err, "test_pool", Some(1000), Some(980));

        assert!(result.is_err());
        if let Err(RpcError::OutOfGas(message, pool_state)) = result {
            assert!(message.contains("Used: 98.00% of gas limit."));
            assert_eq!(pool_state, "test_pool");
        } else {
            panic!("Expected OutOfGas error");
        }
    }

    #[test]
    fn test_maybe_coerce_error_no_gas_limit_info() {
        // Test out-of-gas situation without gas limit info
        let err = RpcError::InvalidResponse("OutOfGas".to_string());

        let result = maybe_coerce_error(err, "test_pool", None, None);

        assert!(result.is_err());
        if let Err(RpcError::OutOfGas(message, pool_state)) = result {
            assert!(message.contains("Original error: OutOfGas"));
            assert_eq!(pool_state, "test_pool");
        } else {
            panic!("Expected OutOfGas error");
        }
    }

    #[test]
    fn test_maybe_coerce_error_no_match() {
        // Test for non-revert, non-out-of-gas errors
        let err = RpcError::Rpc("Some other error".to_string(), StatusCode::BAD_REQUEST);

        let result = maybe_coerce_error(err, "test_pool", None, None);

        assert!(result.is_err());
        if let Err(RpcError::Rpc(message, status)) = result {
            assert_eq!(message, "Some other error");
            assert_eq!(status, StatusCode::BAD_REQUEST);
        } else {
            panic!("Expected Rpc error");
        }
    }

    #[test]
    fn test_parse_solidity_error_message_error_string() {
        // Test parsing Solidity Error(string) message
        let data = "0x08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e416d6f756e7420746f6f206c6f77000000000000000000000000000000000000";

        let result = parse_solidity_error_message(data);

        assert_eq!(result, "Amount too low");
    }

    #[test]
    fn test_parse_solidity_error_message_panic_code() {
        // Test parsing Solidity Panic(uint256) message
        let data = "0x4e487b710000000000000000000000000000000000000000000000000000000000000001";

        let result = parse_solidity_error_message(data);

        assert_eq!(result, "AssertionError");
    }

    #[test]
    fn test_parse_solidity_error_message_failed_to_decode() {
        // Test failed decoding with invalid data
        let data = "0x1234567890";

        let result = parse_solidity_error_message(data);

        assert!(result.contains("Failed to decode"));
    }

    #[test]
    fn test_get_contract_bytecode() {
        // Create a temporary file with some test data
        let mut temp_file = NamedTempFile::new().unwrap();
        let test_data = b"Test contract bytecode";
        temp_file.write_all(test_data).unwrap();
        let temp_path = temp_file.path().to_str().unwrap();

        // First call to get_contract_bytecode
        let result1 = get_contract_bytecode(temp_path).unwrap();
        assert_eq!(result1, test_data);

        // Second call to get_contract_bytecode (should use cached data)
        // Verify that the cache was used (file is not read twice)
        remove_file(&temp_file).unwrap(); // This removes the temporary file
        let result2 = get_contract_bytecode(temp_path).unwrap();
        assert_eq!(result2, test_data);
    }

    #[test]
    fn test_get_contract_bytecode_error() {
        let result = get_contract_bytecode("non_existent_file.txt");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_swap_abi() {
        let result = load_swap_abi();
        assert!(result.is_ok());

        let abi: Abi = result.expect("Failed to retrieve swap ABI result");
        assert!(!abi.functions.is_empty(), "The swap ABI should contain functions.");
    }

    #[test]
    fn test_load_erc20_abi() {
        let result = load_erc20_abi();
        assert!(result.is_ok());
        let abi: Abi = result.expect("Failed to retrieve ERC20 ABI result");
        assert!(!abi.functions.is_empty(), "The ERC20 ABI should contain functions.");
    }
}
