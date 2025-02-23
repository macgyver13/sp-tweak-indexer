use secp256k1::XOnlyPublicKey;
use bitcoin::consensus::encode::deserialize_hex;
use bitcoin::block::Block;
use bitcoin::{ScriptBuf, Transaction, WitnessVersion};
use silentpayments::utils::receiving;
use silentpayments::secp256k1::PublicKey;
use silentpayments::Error as SPError;
use silentpayments::secp256k1::Error as SECPError;
use std::error::Error;
use std::process::{Command, Stdio};
use tracing::{error,warn,debug};
use serde::{Serialize, Deserialize};
use serde_json;
use tokio::task;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PreviousScript {
    txid: String,
    vout: u32,
    script: String,
}

pub struct Tweak {
    pub tx_id: String,
    pub tweak: String,
}

#[derive(Debug)]
enum ChainError {
    TxOutputNotFound,
    PubKeyFromInput,
    SegWitVersionGE2,
    ParseInputTransaction,
}
impl std::error::Error for ChainError {}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ChainError::TxOutputNotFound => write!(f, "Could not find previous output transaction"),
            ChainError::PubKeyFromInput => write!(f, "Pub Key From Input error"),
            ChainError::SegWitVersionGE2 => write!(f, "Segwit version 2 or higher not allowed"),
            ChainError::ParseInputTransaction => write!(f, "Unable to parse previous output transaction")
        }
    }
}

// take json transaction output and parse with serde to product Vec<PreviousScript>
pub fn get_block_input_transactions(block_hash: &str) -> Result<Vec<PreviousScript>, Box<dyn Error>> {
    let transactions_json = match get_block_with_input(&block_hash) {
        Ok(block_str) => block_str,
        Err(err) => {
            error!("Error fetching block: {}", err);
            return Err(Box::new(ChainError::ParseInputTransaction));
        }
    };
    
    let previous_scripts: Vec<PreviousScript> = match serde_json::from_str(&transactions_json) {
        Ok(scripts) => scripts,
        Err(err) => {
            error!("Error parsing json transactions: {}", err);
            return Err(Box::new(ChainError::ParseInputTransaction));
        }
    };
    
    Ok(previous_scripts)
}

pub fn get_block_count() -> Result<String, String> {
    bcli(&["getblockcount"])
}

pub fn get_block_hash(height: u32) -> Result<String, String> {
    bcli(&["getblockhash", &height.to_string()])
}

pub fn get_block(block_hash: &str) -> Result<String, String> {
    bcli(&["getblock", block_hash, "0"])
}

// Fetch the long form output to include input previous out (faster than using RPC for each transaction in a block)
pub fn get_block_with_input(block_hash: &str) -> Result<String, String> {
    let first_cmd = Command::new("bitcoin-cli")
        .args(["getblock", block_hash, "3"]) 
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to execute bitcoin-cli: {}", e))?;

    // Second command: Processing JSON with jq
    let result = Command::new("jq")
        .args(&["-c", "[.tx[].vin[] | select(.txid != null) | {txid, vout, script: .prevout.scriptPubKey.hex}]"])
        .stdin(Stdio::from(first_cmd.stdout.unwrap())) // Pipe stdout from first command
        .output()
        .map_err(|e| format!("Failed to execute jq: {}", e))?;

    if !result.status.success() {
        return Err(format!(
            "bitcoin-cli error: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }

    return Ok(String::from_utf8(result.stdout).unwrap().trim().to_string());
}

pub fn get_transaction(txid: &str) -> Result<String, String> {
    bcli(&["getrawtransaction", txid])
}

pub fn bcli(args: &[&str]) -> Result<String, String> {
    let result = Command::new("bitcoin-cli")
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute bitcoin-cli: {}", e))?;

    if !result.status.success() {
        return Err(format!(
            "bitcoin-cli error: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }

    return Ok(String::from_utf8(result.stdout).unwrap().trim().to_string());
}

#[derive(Clone)]
pub struct Chain {
    previous_scripts: Option<Vec<PreviousScript>>
}

impl Chain {
    pub fn new() -> Self {
        Self { previous_scripts: None }
    }

    //Should be set once per block
    pub fn set_previous_scripts(&mut self, previous_scripts: Vec<PreviousScript>) {
        self.previous_scripts = Some(previous_scripts);
    }

    //Return the matching previous output string given txid and vout
    pub fn find_previous_script(&self, tx_id: &str, vout: u32) -> Option<&PreviousScript> {
        self.previous_scripts.as_ref()?.iter().find(|ps| ps.txid == tx_id && ps.vout == vout)
    }

    //Determine if this spend script is using segwit version 2 or higher
    fn is_segwit_gt_v1(&self, script_pubkey: &ScriptBuf) -> bool {
        if let Some(version) = script_pubkey.witness_version() {
            match version {
                WitnessVersion::V0 | WitnessVersion::V1 => false, // v0 and v1 accepted
                _ => true, // reject all other versions v2, ...
            }
        } else {
            false // Not segwit pass
        }
    }

    // Heavy inspiration from sp-client (https://github.com/cygnet3/sp-client) and rust-silentpayments (https://github.com/cygnet3/rust-silentpayments)
    async fn process_transaction(&self, transaction: &Transaction) -> Result<Vec<Tweak>, Box<dyn Error + Send + Sync>> {
        let mut tweaks = Vec::new();

        //Calculate input pub keys
        let mut input_pubkeys: Vec<PublicKey> = vec![];    
        for input in transaction.input.iter() {
            if input.previous_output.is_null() {
                return Ok(tweaks);
            }

            // Fetch the previous transaction
            let previous_script = if let Some(prev_script) = self.find_previous_script(&input.previous_output.txid.to_string(), input.previous_output.vout) {
                ScriptBuf::from_hex(&prev_script.script)?
            } else {
                warn!("Had to fetch previous input transaction using RPC (txid): {}",transaction.compute_txid());
                let previous_tx_hex = get_transaction(&input.previous_output.txid.to_string())?;
                let previous_tx: Transaction = deserialize_hex::<Transaction>(&previous_tx_hex)?;
                assert!(previous_tx.compute_txid() == input.previous_output.txid);

                match previous_tx.output.get(input.previous_output.vout as usize) {
                    Some(output) => output.script_pubkey.clone(),
                    None => return Err(Box::new(ChainError::TxOutputNotFound)),
                }
            };
            
            // Filter transactions by BIP352 consensus on allowed transactions
            if self.is_segwit_gt_v1(&previous_script) {
                warn!("Segwit > v1: {}",previous_script.to_hex_string());
                return Err(Box::new(ChainError::SegWitVersionGE2));
            }

            // Collect all input pub keys
            match receiving::get_pubkey_from_input(
                &input.script_sig.to_bytes(), 
                &input.witness.to_vec(), 
                &previous_script.to_bytes(),
            ) {
                Ok(Some(pubkey)) => {
                    input_pubkeys.push(pubkey);
                    debug!("Input Previous Output: {}:{} -> {}", input.previous_output.txid, input.previous_output.vout, pubkey.to_string());
                }
                Ok(None) => {
                    debug!("No public key found in input {}:{}", input.previous_output.txid, input.previous_output.vout);
                }
                Err(_) => {
                    return Err(Box::new(ChainError::PubKeyFromInput));
                }
            }
        }

        // Get the reference to a vector of public keys for further calculations
        let pubkeys_ref: Vec<&PublicKey> = input_pubkeys.iter().collect();

        //Calculate outpoints
        let outpoints: Vec<(String, u32)> = transaction
        .input
        .iter()
        .map(|i| {
            let outpoint = i.previous_output;
            (outpoint.txid.to_string(), outpoint.vout)
        })
        .collect();

        // Calculate the tweak data based on the public keys and outpoints
        let tweak_data = match receiving::calculate_tweak_data(&pubkeys_ref, &outpoints) {
            Ok(tweak_key) => tweak_key,
            Err(err) => {
                match err {
                    SPError::Secp256k1Error(SECPError::InvalidPublicKeySum) => {
                        debug!("Invalid public key sum: {}", err);
                        return Ok(tweaks);
                    },
                    _ => return Err(Box::new(err))
                }
            }
        };

        tweaks.push(Tweak {
            tx_id: transaction.compute_txid().to_string(),
            tweak: tweak_data.to_string(),
        });

        Ok(tweaks)
    }

    /// Deserializes a block but tracks how much data was consumed
    pub async fn process_transactions(&mut self, block_hex: &String) -> Result<Vec<Tweak>, Box<dyn Error + Send + Sync>>{
        let block = deserialize_hex::<Block>(block_hex)
            .map_err(|e| format!("Failed to decode block: {}", e))?;
        
        let mut tasks = vec![];
        let mut block_tweaks = vec![];

        for tx in block.txdata.iter() {
            let chain = self.clone();
            let tx = tx.clone();
            let task = task::spawn(async move {
                // Filter transactions by BIP352 consensus on allowed transactions
                // Only process transactions with outputs that have a valid P2TR scriptpubkey
                debug!("Spawning process tx tasks {}", tx.compute_txid());
                let mut has_taproot: bool = false;
                for output in tx.output.iter() {
                    if output.script_pubkey.is_p2tr() && XOnlyPublicKey::from_slice(&output.script_pubkey.as_bytes()[2..]).is_ok()  {
                        has_taproot = true;
                        break;
                    }
                }
                if has_taproot {
                    match chain.process_transaction(&tx).await {
                        Ok(tweaks) => {
                            debug!("Completed process tx tasks {}", tx.compute_txid());
                            Ok(tweaks)
                        },
                        Err(err) => {
                            debug!("Error processing tx: {}, block: {}: err: {}", tx.compute_txid(), block.header.block_hash(), err);
                            Err(err)
                        }
                    }
                } else {
                    debug!("Completed process tx tasks {}, no tweaks found", tx.compute_txid());
                    Ok(vec![])
                }
                
            });
            tasks.push(task);
        }

        for task in tasks {
            match task.await {
                Ok(Ok(tweaks)) => {
                    if !tweaks.is_empty() {
                        block_tweaks.extend(tweaks);
                    }
                }
                Ok(Err(err)) => warn!("Error in task: {}", err),
                Err(err) => warn!("Task panicked: {}", err),
            }
        }

        Ok(block_tweaks)
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::blockdata::script::Builder;
    use bitcoin::blockdata::opcodes::all::{*};

    #[test]
    fn test_is_segwit_gt_v1() {
        let chain = Chain::new();

        // Test empty script
        assert_eq!(chain.is_segwit_gt_v1(&Builder::new().into_script()), false);

        // Test with SegWit version 0
        let script_pubkey_v0 = Builder::new().push_opcode(OP_PUSHBYTES_0).into_script();
        assert_eq!(chain.is_segwit_gt_v1(&script_pubkey_v0), false);

        // Test with 0x0101
        let script_pubkey_v1 = Builder::new().push_opcode(OP_PUSHBYTES_1).push_slice([0]).into_script();
        assert_eq!(chain.is_segwit_gt_v1(&script_pubkey_v1), false);

        // Test with Taproot version 1
        let script_pubkey_v1 = Builder::new().push_opcode(OP_PUSHNUM_1).push_slice([1,2,3,4]).into_script();
        assert_eq!(chain.is_segwit_gt_v1(&script_pubkey_v1), false);

        // Test with future version 2
        let script_pubkey_v2 = Builder::new().push_opcode(OP_PUSHNUM_2).push_slice([1,2,3,4,5,6]).into_script();
        assert_eq!(chain.is_segwit_gt_v1(&script_pubkey_v2), true);

        // Test with P2SH script
        let p2sh_script = Builder::new().push_opcode(OP_HASH160).push_slice(&[0x8b, 0xc9, 0xba, 0xf0, 0xcc, 0x16, 0x73, 0xad, 0x8e, 0xdd, 0x14, 0xbe, 0x27, 0xff, 0x2f, 0x07, 0x2f, 0x92, 0xb1, 0x05]).push_opcode(OP_EQUAL).into_script();
        assert_eq!(chain.is_segwit_gt_v1(&p2sh_script), false);
    }
}