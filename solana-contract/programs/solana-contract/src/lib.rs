use anchor_lang::prelude::*;

declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS");

#[program]
pub mod solana_contract {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let user = &mut ctx.accounts.user;
        user.authority = *ctx.accounts.authority.key;
        user.balance = 0;
        Ok(())
    }
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        let user = &mut ctx.accounts.user;

        let cpi_accounts = anchor_lang::system_program::Transfer {
            from: ctx.accounts.authority.to_account_info(),
            to: ctx.accounts.vault.to_account_info(),
        };
        let cpi_program = ctx.accounts.system_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        anchor_lang::system_program::transfer(cpi_ctx, amount)?;

        user.balance = user.balance.checked_add(amount).ok_or(ErrorCode::Overflow)?;
        Ok(())
    }
    /// Withdraws lamports from the vault PDA back to the authority and updates balance.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        let user = &mut ctx.accounts.user;
        require!(user.balance >= amount, ErrorCode::InsufficientFunds);

        let user_key = user.key();
    let vault_seeds = &[b"vault", user_key.as_ref()];
    let (_vault_pda, bump) = Pubkey::find_program_address(vault_seeds, ctx.program_id);
    
    
        let signer_seeds = &[b"vault", user_key.as_ref(), &[bump]];
    
        // Perform lamport transfer from vault to authority
        **ctx.accounts.vault.to_account_info().try_borrow_mut_lamports()? -= amount;
        **ctx.accounts.authority.to_account_info().try_borrow_mut_lamports()? += amount;

        user.balance = user
            .balance
            .checked_sub(amount)
            .ok_or(ErrorCode::Overflow)?;
        Ok(())
    }
}


#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = authority, space = 8 + 32 + 8)]
    pub user: Account<'info, UserAccount>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    /// Проверяем, что `user.owner == authority.key()`
    #[account(mut, has_one = authority)]
    pub user: Account<'info, UserAccount>,

    #[account(mut, seeds = [b"vault", user.key().as_ref()], bump)]
    /// CHECK: vault PDA to hold funds
    pub vault: UncheckedAccount<'info>,

    /// Подписант — должен совпадать с `user.owner`
    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut, has_one = authority)]
    pub user: Account<'info, UserAccount>,
    #[account(mut, seeds = [b"vault", user.key().as_ref()], bump)]
    /// CHECK: vault PDA to hold funds
    pub vault: UncheckedAccount<'info>,
    #[account(mut)]
    pub authority: Signer<'info>,
}

#[account]
pub struct UserAccount {
    pub authority: Pubkey,
    pub balance: u64,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Insufficient funds for withdrawal")]
    InsufficientFunds,
    #[msg("Numeric overflow occurred")]
    Overflow,
}
