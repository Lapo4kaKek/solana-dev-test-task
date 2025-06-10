import * as anchor from "@project-serum/anchor";
import { Program } from "@project-serum/anchor";
import { SolanaContract } from "../target/types/solana_contract";

describe("solana-contract", () => {
  // Configure the client to use the local cluster.
  anchor.setProvider(anchor.AnchorProvider.env());

  const program = anchor.workspace.SolanaContract as Program<SolanaContract>;

  it("Is initialized!", async () => {
    // Add your test here.
    const tx = await program.methods.initialize().rpc();
    console.log("Your transaction signature", tx);
  });
});
