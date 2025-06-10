use anyhow::Result;
use clap::Parser;
use log::{error, info, warn};
use serde::Deserialize;
use std::{collections::HashMap, fs, str::FromStr, sync::Arc, time::Duration};
use tokio::{select, signal, sync::mpsc, sync::RwLock, task::JoinHandle};
use tonic::{
    metadata::AsciiMetadataValue,
    transport::{Channel, Endpoint},
    Request, Status,
};

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

// Generated from proto
pub mod geyser {
    tonic::include_proto!("geyser");
}

use geyser::geyser_client::GeyserClient;

#[derive(Parser)]
#[command(name = "geyser-realtime")]
#[command(about = "Real-time Solana slot monitoring via Geyser")]
struct Cli {
    #[arg(short, long, default_value = "../config.yaml")]
    config: String,
}

#[derive(Deserialize, Clone)]
struct Config {
    rpc_url: String,
    geyser_endpoint: String,
    api_key: String,
    transfer: TransferConfig,
    retry: RetryConfig,
}

#[derive(Deserialize, Clone)]
struct TransferConfig {
    source_keypair: String,
    destination: String,
    amount_sol: f64,
}

#[derive(Deserialize, Clone)]
struct RetryConfig {
    max_attempts: u32,
    delay_ms: u64,
}

#[derive(Default)]
struct Stats {
    slots_received: u64,
    transfers_successful: u64,
    transfers_failed: u64,
    last_slot: u64,
}

// type GeyserClientType = GeyserClient<
//     tonic::service::interceptor::InterceptedService<
//         Channel,
//         impl Fn(Request<()>) -> Result<Request<()>, Status>,
//     >,
// >;

struct App {
    config: Config,
    rpc_client: Arc<RpcClient>,
    source_keypair: Keypair,
    destination: Pubkey,
    amount_lamports: u64,
    stats: Arc<RwLock<Stats>>,
}

impl App {
    async fn new(config: Config) -> Result<Self> {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        let source_keypair = read_keypair_file(&config.transfer.source_keypair)
            .map_err(|e| anyhow::anyhow!("Failed to read keypair: {}", e))?;

        let destination = Pubkey::from_str(&config.transfer.destination)
            .map_err(|e| anyhow::anyhow!("Invalid destination: {}", e))?;

        let amount_lamports = (config.transfer.amount_sol * LAMPORTS_PER_SOL as f64) as u64;

        Ok(Self {
            config,
            rpc_client,
            source_keypair,
            destination,
            amount_lamports,
            stats: Arc::new(RwLock::new(Stats::default())),
        })
    }

    async fn run(&self) -> Result<()> {
        let (slot_tx, slot_rx) = mpsc::unbounded_channel();

        let monitor_handle = self.spawn_geyser_monitor(slot_tx);
        let processor_handle = self.spawn_transfer_processor(slot_rx);
        let stats_handle = self.spawn_stats_reporter();

        info!("🚀 Starting real-time Geyser monitoring");
        info!("Endpoint: {}", self.config.geyser_endpoint);
        info!(
            "Transfer: {} SOL to {}",
            self.config.transfer.amount_sol, self.destination
        );

        select! {
            _ = signal::ctrl_c() => {
                info!("🛑 Shutdown signal received");
            }
            result = monitor_handle => {
                error!("Geyser monitor exited: {:?}", result);
            }
            result = processor_handle => {
                error!("Transfer processor exited: {:?}", result);
            }
        }

        stats_handle.abort();
        Ok(())
    }

    fn spawn_geyser_monitor(&self, tx: mpsc::UnboundedSender<u64>) -> JoinHandle<()> {
        let endpoint = self.config.geyser_endpoint.clone();
        let api_key = self.config.api_key.clone();
        let stats = Arc::clone(&self.stats);

        tokio::spawn(async move {
            loop {
                match Self::connect_geyser(&endpoint, &api_key).await {
                    Ok(mut client) => {
                        info!("✅ Connected to Geyser");

                        match Self::subscribe_slots(&mut client, &tx, &stats).await {
                            Ok(_) => info!("Geyser subscription ended normally"),
                            Err(e) => error!("Geyser subscription error: {}", e),
                        }
                    }
                    Err(e) => {
                        error!("Failed to connect to Geyser: {}", e);
                    }
                }

                warn!("Reconnecting to Geyser in 5 seconds...");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    }

    async fn connect_geyser(
        endpoint: &str,
        api_key: &str,
    ) -> Result<
        GeyserClient<
            tonic::service::interceptor::InterceptedService<
                Channel,
                impl Fn(Request<()>) -> Result<Request<()>, Status>,
            >,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let channel = Endpoint::from_shared(endpoint.to_string())?
            .connect()
            .await?;

        let api_key_value: AsciiMetadataValue = api_key
            .parse()
            .map_err(|e| format!("Invalid API key: {}", e))?;

        let client = GeyserClient::with_interceptor(channel, move |mut req: Request<()>| {
            req.metadata_mut().insert("x-token", api_key_value.clone());
            Ok(req)
        });

        Ok(client)
    }

    async fn subscribe_slots(
        client: &mut GeyserClient<
            tonic::service::interceptor::InterceptedService<
                Channel,
                impl Fn(Request<()>) -> Result<Request<()>, Status>,
            >,
        >,
        tx: &mpsc::UnboundedSender<u64>,
        stats: &Arc<RwLock<Stats>>,
    ) -> Result<()> {
        let mut slots = HashMap::new();
        slots.insert(
            "client".to_string(),
            geyser::SubscribeRequestFilterSlots {
                filter_by_commitment: vec!["confirmed".to_string(), "finalized".to_string()],
            },
        );

        let request = geyser::SubscribeRequest {
            slots,
            accounts: HashMap::new(),
            transactions: HashMap::new(),
            transactions_status: HashMap::new(),
            blocks: HashMap::new(),
            blocks_meta: HashMap::new(),
            entry: HashMap::new(),
            accounts_data_slice: vec![],
            ping: None,
        };

        let mut stream = client.subscribe(request).await?.into_inner();
        info!("Subscribed to Geyser slots stream");

        while let Some(update) = stream.message().await? {
            if let Some(geyser::subscribe_update::UpdateOneof::Slot(slot_update)) =
                update.update_oneof
            {
                let slot = slot_update.slot;

                let mut stats = stats.write().await;
                stats.slots_received += 1;
                stats.last_slot = slot;
                drop(stats);

                match slot_update.status() {
                    geyser::CommitmentLevel::Confirmed | geyser::CommitmentLevel::Finalized => {
                        let _ = tx.send(slot);
                        info!("New slot: {} ({:?})", slot, slot_update.status());
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn spawn_transfer_processor(&self, mut rx: mpsc::UnboundedReceiver<u64>) -> JoinHandle<()> {
        let rpc_client = Arc::clone(&self.rpc_client);
        let source_keypair = Keypair::from_bytes(&self.source_keypair.to_bytes()).unwrap();
        let destination = self.destination;
        let amount_lamports = self.amount_lamports;
        let stats = Arc::clone(&self.stats);
        let retry_config = self.config.retry.clone();

        tokio::spawn(async move {
            while let Some(slot) = rx.recv().await {
                let result = Self::send_transfer_with_retry(
                    &rpc_client,
                    &source_keypair,
                    &destination,
                    amount_lamports,
                    &retry_config,
                )
                .await;

                let mut stats = stats.write().await;
                match result {
                    Ok(signature) => {
                        stats.transfers_successful += 1;
                        info!(
                            "✅ Transfer sent for slot {}: {} SOL → {} | Signature: {}",
                            slot,
                            amount_lamports as f64 / LAMPORTS_PER_SOL as f64,
                            &destination.to_string()[..8],
                            signature
                        );
                    }
                    Err(e) => {
                        stats.transfers_failed += 1;
                        error!("❌ Transfer failed for slot {}: {}", slot, e);
                    }
                }
            }
        })
    }

    fn spawn_stats_reporter(&self) -> JoinHandle<()> {
        let stats = Arc::clone(&self.stats);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));

            loop {
                interval.tick().await;
                let stats = stats.read().await;
                let total_transfers = stats.transfers_successful + stats.transfers_failed;
                let success_rate = if total_transfers > 0 {
                    (stats.transfers_successful as f64 / total_transfers as f64) * 100.0
                } else {
                    0.0
                };

                info!(
                    "📊 Stats - Slots: {} | Transfers: {}✅/{}❌ | Success: {:.1}% | Last slot: {}",
                    stats.slots_received,
                    stats.transfers_successful,
                    stats.transfers_failed,
                    success_rate,
                    stats.last_slot
                );
            }
        })
    }

    async fn send_transfer_with_retry(
        client: &RpcClient,
        keypair: &Keypair,
        destination: &Pubkey,
        amount: u64,
        retry_config: &RetryConfig,
    ) -> Result<Signature> {
        for attempt in 0..retry_config.max_attempts {
            match Self::send_transfer(client, keypair, destination, amount).await {
                Ok(sig) => return Ok(sig),
                Err(e) if attempt == retry_config.max_attempts - 1 => return Err(e),
                Err(e) => {
                    warn!(
                        "Transfer attempt {}/{} failed: {}",
                        attempt + 1,
                        retry_config.max_attempts,
                        e
                    );
                    tokio::time::sleep(Duration::from_millis(retry_config.delay_ms)).await;
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
        let balance = client
            .get_balance(&keypair.pubkey())
            .map_err(|e| anyhow::anyhow!("Failed to get balance: {}", e))?;

        if balance < amount + 5000 {
            return Err(anyhow::anyhow!(
                "Insufficient balance: {} lamports needed, {} available",
                amount + 5000,
                balance
            ));
        }

        let ix = system_instruction::transfer(&keypair.pubkey(), destination, amount);
        let blockhash = client
            .get_latest_blockhash()
            .map_err(|e| anyhow::anyhow!("Failed to get blockhash: {}", e))?;

        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&keypair.pubkey()),
            &[keypair],
            blockhash,
        );

        client
            .send_and_confirm_transaction(&tx)
            .map_err(|e| anyhow::anyhow!("Transaction failed: {}", e))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    let config_str = fs::read_to_string(&cli.config)
        .map_err(|e| anyhow::anyhow!("Failed to read config: {}", e))?;
    let config: Config = serde_yaml::from_str(&config_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

    let app = App::new(config).await?;
    app.run().await
}
