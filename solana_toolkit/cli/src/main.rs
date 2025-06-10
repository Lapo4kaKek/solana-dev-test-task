use clap::{Arg, Command};
use futures::future::join_all;
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    message::Message,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signature},
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use std::{fs, str::FromStr, sync::Arc, time::Instant};
use tokio;

#[derive(Deserialize)]
struct Config {
    rpc_url: String,
    wallets: Wallets,
}

#[derive(Deserialize)]
struct Wallets {
    sources: Vec<WalletInfo>,
    destinations: Vec<DestinationInfo>,
}

#[derive(Deserialize)]
struct WalletInfo {
    keypair: String,
}
#[derive(Deserialize)]
struct DestinationInfo {
    address: String,
}
#[derive(Debug)]
struct TransferResult {
    source: String,
    destination: String,
    signature: Result<Signature, String>,
    duration: std::time::Duration,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let matches = Command::new("solana-transfer")
        .about("Mass SOL transfer tool")
        .arg(
            Arg::new("amount")
                .help("Amount in SOL to transfer from each source wallet")
                .required(true)
                .index(1)
                .value_parser(clap::value_parser!(f64)),
        )
        .get_matches();

    let amount_sol: f64 = *matches.get_one::<f64>("amount").unwrap();
    let amount_lamports = (amount_sol * 1_000_000_000.0) as u64;

    println!("🚀 Starting mass SOL transfer...");
    println!(
        "Amount per transfer: {} SOL ({} lamports)",
        amount_sol, amount_lamports
    );

    let cfg_str = fs::read_to_string("../config.yaml")
        .map_err(|e| anyhow::anyhow!("Can't read config.yaml: {}", e))?;
    let cfg: Config =
        serde_yaml::from_str(&cfg_str).map_err(|e| anyhow::anyhow!("Error parsing YAML: {}", e))?;

    // Validate destinations
    let destinations: Result<Vec<Pubkey>, _> = cfg
        .wallets
        .destinations
        .iter()
        .map(|dest_info| Pubkey::from_str(&dest_info.address))
        .collect();

    let destinations =
        destinations.map_err(|e| anyhow::anyhow!("Invalid destination pubkey: {}", e))?;

    if cfg.wallets.sources.len() != destinations.len() {
        return Err(anyhow::anyhow!(
            "Sources count ({}) != destinations count ({})",
            cfg.wallets.sources.len(),
            destinations.len()
        ));
    }

    let client = Arc::new(RpcClient::new_with_commitment(
        cfg.rpc_url,
        CommitmentConfig::confirmed(),
    ));

    // Create transfer tasks
    let start_time = Instant::now();

    let tasks = cfg
        .wallets
        .sources
        .into_iter()
        .zip(destinations.into_iter())
        .map(|(source_wallet, dest_pubkey)| {
            let client = Arc::clone(&client);

            async move {
                let task_start = Instant::now();

                // Read source keypair
                let source_keypair = match read_keypair_file(&source_wallet.keypair) {
                    Ok(kp) => kp,
                    Err(e) => {
                        return TransferResult {
                            source: source_wallet.keypair.clone(),
                            destination: dest_pubkey.to_string(),
                            signature: Err(format!("Failed to read keypair: {}", e)),
                            duration: task_start.elapsed(),
                        };
                    }
                };

                let source_pubkey = source_keypair.pubkey();

                // Check source balance
                let balance = match client.get_balance(&source_pubkey) {
                    Ok(bal) => bal,
                    Err(e) => {
                        return TransferResult {
                            source: source_pubkey.to_string(),
                            destination: dest_pubkey.to_string(),
                            signature: Err(format!("Failed to get balance: {}", e)),
                            duration: task_start.elapsed(),
                        };
                    }
                };

                if balance < amount_lamports + 5000 {
                    // 5000 lamports for fees
                    return TransferResult {
                        source: source_pubkey.to_string(),
                        destination: dest_pubkey.to_string(),
                        signature: Err(format!(
                            "Insufficient balance: {} lamports (need {} + fees)",
                            balance, amount_lamports
                        )),
                        duration: task_start.elapsed(),
                    };
                }

                // Create transfer instruction
                let transfer_instruction =
                    system_instruction::transfer(&source_pubkey, &dest_pubkey, amount_lamports);

                // Get recent blockhash
                let recent_blockhash = match client.get_latest_blockhash() {
                    Ok(hash) => hash,
                    Err(e) => {
                        return TransferResult {
                            source: source_pubkey.to_string(),
                            destination: dest_pubkey.to_string(),
                            signature: Err(format!("Failed to get blockhash: {}", e)),
                            duration: task_start.elapsed(),
                        };
                    }
                };

                // Create and sign transaction
                let message = Message::new(&[transfer_instruction], Some(&source_pubkey));
                let mut transaction = Transaction::new_unsigned(message);
                transaction.partial_sign(&[&source_keypair], recent_blockhash);

                // Send transaction
                let signature = match client.send_and_confirm_transaction_with_spinner(&transaction)
                {
                    Ok(sig) => Ok(sig),
                    Err(e) => Err(format!("Transaction failed: {}", e)),
                };

                TransferResult {
                    source: source_pubkey.to_string(),
                    destination: dest_pubkey.to_string(),
                    signature,
                    duration: task_start.elapsed(),
                }
            }
        });

    // Execute all transfers concurrently
    println!("📡 Executing {} transfers...", tasks.len());
    let results = join_all(tasks).await;
    let total_duration = start_time.elapsed();

    // Print results and statistics
    println!("\n📊 Transfer Results:");
    println!("{:-<100}", "");

    let mut successful = 0;
    let mut failed = 0;
    let mut total_transfer_time = std::time::Duration::new(0, 0);

    for result in results {
        match &result.signature {
            Ok(sig) => {
                println!(
                    "✅ {} → {} | {} | {:.2}s",
                    &result.source[..8],
                    &result.destination[..8],
                    sig,
                    result.duration.as_secs_f64()
                );
                successful += 1;
            }
            Err(err) => {
                println!(
                    "❌ {} → {} | ERROR: {} | {:.2}s",
                    &result.source[..8],
                    &result.destination[..8],
                    err,
                    result.duration.as_secs_f64()
                );
                failed += 1;
            }
        }
        total_transfer_time += result.duration;
    }

    println!("{:-<100}", "");
    println!("📈 Statistics:");
    println!("  Total transfers: {}", successful + failed);
    println!("  Successful: {} ✅", successful);
    println!("  Failed: {} ❌", failed);
    println!(
        "  Success rate: {:.1}%",
        (successful as f64 / (successful + failed) as f64) * 100.0
    );
    println!(
        "  Total execution time: {:.2}s",
        total_duration.as_secs_f64()
    );
    println!(
        "  Average time per transfer: {:.2}s",
        total_transfer_time.as_secs_f64() / (successful + failed) as f64
    );

    if successful > 0 {
        println!(
            "  Total transferred: {} SOL",
            successful as f64 * amount_sol
        );
    }

    Ok(())
}
