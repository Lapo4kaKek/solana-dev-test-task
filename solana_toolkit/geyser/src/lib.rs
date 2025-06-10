use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub grpc: GrpcConfig,
    pub solana: SolanaConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GrpcConfig {
    pub endpoint: String,
    pub api_key: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SolanaConfig {
    pub rpc_url: String,
    pub keypair_path: String,
    pub recipient_wallet: String,
    pub transfer_amount_sol: f64,
}

pub mod geyser {
    tonic::include_proto!("geyser");
}
