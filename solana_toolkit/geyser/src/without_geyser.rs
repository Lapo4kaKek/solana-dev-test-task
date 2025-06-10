use anyhow::{Context, Result};
use clap::{Arg, Command};
use log::{debug, error, info, warn};
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature},
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use std::{
    fs, str::FromStr, sync::Arc, time::{Duration, Instant},
};
use tokio::{
    select, signal,
    sync::{mpsc, RwLock},
    task::JoinHandle,
    time::{interval, sleep},
};

#[derive(Deserialize, Clone)]
struct Config {
    rpc_url: String,
    transfer: TransferConfig,
    polling_interval_ms: u64,
    max_retries: u32,
    retry_delay_ms: u64,
}

#[derive(Deserialize, Clone)]
struct TransferConfig {
    source_keypair: String,
    destination: String,
    amount_sol: f64,
}

#[derive(Default)]
struct Stats {
    slots_processed: u64,
    transfers_successful: u64,
    transfers_failed: u64,
    last_slot: u64,
}

struct App {
    rpc_client: Arc<RpcClient>,
    source_keypair: Keypair,
    destination: Pubkey,
    amount_lamports: u64,
    config: Config,
    stats: Arc<RwLock<Stats>>,
}

impl App {
    async fn new(config: Config) -> Result<Self> {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));
        let source_keypair = read_keypair_file(&config.transfer.source_keypair)
            .map_err(|e| anyhow::anyhow!("Failed to read source keypair: {}", e))?;

        let destination = Pubkey::from_str(&config.transfer.destination)
            .map_err(|e| anyhow::anyhow!("Failed to get initial slot: {}", e))?;

        let amount_lamports = (config.transfer.amount_sol * LAMPORTS_PER_SOL as f64) as u64;

        let initial_slot = rpc_client.get_slot()
            .context("Failed to get initial slot")?;

        let stats = Arc::new(RwLock::new(Stats {
            last_slot: initial_slot,
            ..Default::default()
        }));

        Ok(Self {
            rpc_client,
            source_keypair,
            destination,
            amount_lamports,
            config,
            stats,
        })
    }

    async fn run(&self) -> Result<()> {
        let (slot_tx, slot_rx) = mpsc::unbounded_channel();
        let monitor_handle = self.spawn_slot_monitor(slot_tx);
        
        let processor_handle = self.spawn_transfer_processor(slot_rx);
        
        let stats_handle = self.spawn_stats_reporter();

        info!("Started monitoring slots");
        info!("Transfer: {} SOL to {}", self.config.transfer.amount_sol, self.destination);
        info!("Polling interval: {}ms", self.config.polling_interval_ms);

select! {
            _ = signal::ctrl_c() => {
                info!("Shutdown requested");
            }
            result = monitor_handle => {
                error!("Slot monitor exited: {:?}", result);
            }
            result = processor_handle => {
                error!("Transfer processor exited: {:?}", result);
            }
        }

        stats_handle.abort();
        Ok(())
    }
    fn spawn_slot_monitor(&self, tx: mpsc::UnboundedSender<u64>) -> JoinHandle<()> {
        let rpc_client = Arc::clone(&self.rpc_client);
        let stats = Arc::clone(&self.stats);
        let interval_ms = self.config.polling_interval_ms;

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_millis(interval_ms));
            
            loop {
                ticker.tick().await;
                
                match rpc_client.get_slot() {
                    Ok(current_slot) => {
                        let mut stats = stats.write().await;
                        if current_slot > stats.last_slot {
                            let _ = tx.send(current_slot);
                            debug!("New slot detected: {}", current_slot);
                            stats.last_slot = current_slot;
                            stats.slots_processed += 1;
                        }
                    }
                    Err(e) => {
                        error!("Slot fetch error: {}", e);
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        })
    }

    fn spawn_transfer_processor(&self, mut rx: mpsc::UnboundedReceiver<u64>) -> JoinHandle<()> {
        let rpc_client = Arc::clone(&self.rpc_client);
        let source_keypair = Keypair::from_bytes(&self.source_keypair.to_bytes()).unwrap();
        let destination = self.destination;
        let amount_lamports = self.amount_lamports;
        let stats = Arc::clone(&self.stats);
        let max_retries = self.config.max_retries;
        let retry_delay = Duration::from_millis(self.config.retry_delay_ms);

        tokio::spawn(async move {
            while let Some(slot) = rx.recv().await {
                let result = Self::send_transfer_with_retry(
                    &rpc_client,
                    &source_keypair,
                    &destination,
                    amount_lamports,
                    slot,
                    max_retries,
                    retry_delay,
                ).await;

                let mut stats = stats.write().await;
                match result {
                    Ok(sig) => {
                        stats.transfers_successful += 1;
                        info!("Slot {}: {} SOL → {} | {}", 
                            slot, 
                            amount_lamports as f64 / LAMPORTS_PER_SOL as f64,
                            &destination.to_string()[..8],
                            sig
                        );
                    }
                    Err(e) => {
                        stats.transfers_failed += 1;
                        error!("Slot {}: Transfer failed: {}", slot, e);
                    }
                }
            }
        })
    }

    fn spawn_stats_reporter(&self) -> JoinHandle<()> {
        let stats = Arc::clone(&self.stats);
        
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(10));
            
            loop {
                ticker.tick().await;
                let stats = stats.read().await;
                let total = stats.transfers_successful + stats.transfers_failed;
                let success_rate = if total > 0 {
                    (stats.transfers_successful as f64 / total as f64) * 100.0
                } else { 0.0 };

                info!("\nStats: {} slots | {} success | {} failed | {:.1}% success rate\n",
                    stats.slots_processed,
                    stats.transfers_successful,
                    stats.transfers_failed,
                    success_rate
                );
            }
        })
    }

    async fn send_transfer_with_retry(
        client: &RpcClient,
        keypair: &Keypair,
        destination: &Pubkey,
        amount: u64,
        slot: u64,
        max_retries: u32,
        retry_delay: Duration,
    ) -> Result<Signature> {
        for attempt in 0..=max_retries {
            match Self::send_transfer(client, keypair, destination, amount).await {
                Ok(sig) => return Ok(sig),
                Err(e) if attempt == max_retries => return Err(e),
                Err(e) => {
                    warn!("Retry {}/{} for slot {}: {}", attempt + 1, max_retries + 1, slot, e);
                    sleep(retry_delay).await;
                }
            }
        }
        unreachable!()
    }

    async fn send_transfer(
        client: &RpcClient,
        keypair: &Keypair,
        destination: &Pubkey,
        amount: u64,
    ) -> Result<Signature> {
        let balance = client.get_balance(&keypair.pubkey())
            .context("Failed to get balance")?;
        
        if balance < amount + 5000 {
            return Err(anyhow::anyhow!("Insufficient balance: {}", balance));
        }

        let ix = system_instruction::transfer(&keypair.pubkey(), destination, amount);
        let blockhash = client.get_latest_blockhash()
            .context("Failed to get blockhash")?;
        
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&keypair.pubkey()),
            &[keypair],
            blockhash,
        );

        client.send_and_confirm_transaction(&tx)
            .context("Transaction failed")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let matches = Command::new("solana-slot-transfer")
        .about("Monitor Solana slots and send SOL transfers")
        .arg(Arg::new("config")
            .short('c')
            .long("config")
            .default_value("../config.yaml"))
        .get_matches();

    let config_path = matches.get_one::<String>("config").unwrap();
    let config_str = fs::read_to_string(config_path)
        .context("Failed to read config")?;
    let config: Config = serde_yaml::from_str(&config_str)
        .context("Failed to parse config")?;

    let app = App::new(config).await?;
    app.run().await
}