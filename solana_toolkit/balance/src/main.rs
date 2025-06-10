use futures::future::join_all;
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{signature::read_keypair_file, signer::Signer};
use std::fs;
use std::sync::Arc;
use tokio;

#[derive(Deserialize)]
struct Config {
    rpc_url: String,
    wallets: Wallets,
}

#[derive(Deserialize)]
struct Wallets {
    sources: Vec<WalletInfo>,
}

#[derive(Deserialize)]
struct WalletInfo {
    keypair: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("balance task start!");
    let cfg_str = fs::read_to_string("../config.yaml")
        .map_err(|e| anyhow::anyhow!("Can't read config.yaml: {}", e))?;
    let cfg: Config =
        serde_yaml::from_str(&cfg_str).map_err(|e| anyhow::anyhow!("Error parser YAML: {}", e))?;

    let client = Arc::new(RpcClient::new(cfg.rpc_url.clone()));
    let tasks = cfg.wallets.sources.into_iter().map(|wallet| {
        let client = Arc::clone(&client);
        async move {
            // Read keypair
            let kp = read_keypair_file(&wallet.keypair)
                .map_err(|e| anyhow::anyhow!("Can't read keypair {}: {}", wallet.keypair, e))?;
            // Call balance
            let balance = client
                .get_balance(&kp.pubkey())
                .map_err(|e| anyhow::anyhow!("RPC error for {}: {}", kp.pubkey(), e))?;
            println!(
                "{} → {} SOL ({} lamports)",
                kp.pubkey(),
                balance as f64 / 1e9,
                balance
            );
            Ok::<(), anyhow::Error>(())
        }
    });

    let results = join_all(tasks).await;
    for res in results {
        if let Err(e) = res {
            eprintln!("Error in task: {}", e);
        }
    }

    Ok(())
}
