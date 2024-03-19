use crate::{
    constants::{
        M2_AUCTION_HOUSE, M2_PREFIX, M2_PROGRAM, MAX_METADATA_CREATOR_ROYALTY_BP,
        MAX_REFERRAL_FEE_BP, MAX_TOTAL_PRICE, MIN_SOL_ESCROW_BALANCE_BP, POOL_PREFIX,
    },
    errors::MMMErrorCode,
    state::*,
};
use anchor_lang::{prelude::*, solana_program::log::sol_log_data};
use anchor_spl::token_interface::Mint;
use m2_interface::{
    withdraw_by_mmm_ix_with_program_id, WithdrawByMMMArgs, WithdrawByMmmIxArgs, WithdrawByMmmKeys,
};
use mpl_token_metadata::{
    accounts::{MasterEdition, Metadata},
    types::TokenStandard,
};
use open_creator_protocol::state::Policy;
use solana_program::program::invoke_signed;
use spl_token_2022::{
    extension::{
        group_member_pointer::GroupMemberPointer, metadata_pointer::MetadataPointer,
        BaseStateWithExtensions, StateWithExtensions,
    },
    state::Mint as Token22Mint,
};
use spl_token_group_interface::state::TokenGroupMember;
use spl_token_metadata_interface::state::TokenMetadata;
use std::convert::TryFrom;

#[macro_export]
macro_rules! index_ra {
    ($iter:ident, $i:expr) => {
        $iter
            .get($i)
            .ok_or(MMMErrorCode::InvalidRemainingAccounts)?
    };
}

// copied from mpl-token-metadata
fn check_master_edition(master_edition_account_info: &AccountInfo) -> bool {
    let version = master_edition_account_info.data.borrow()[0];
    version == 2 || version == 6
}

pub fn check_allowlists(allowlists: &[Allowlist]) -> Result<()> {
    for allowlist in allowlists.iter() {
        if !allowlist.valid() {
            msg!("InvalidAllowLists: invalid entry");
            return Err(MMMErrorCode::InvalidAllowLists.into());
        }
    }

    Ok(())
}

pub fn check_allowlists_for_mint(
    allowlists: &[Allowlist],
    mint: &InterfaceAccount<Mint>,
    metadata: &AccountInfo,
    master_edition: Option<&AccountInfo>,
    allowlist_aux: Option<String>,
) -> Result<Metadata> {
    // We need to check the following validation rules
    // 1. make sure the metadata is correctly derived from the metadata pda with the mint
    // 2. make sure mint+metadata(e.g. first verified creator address) can match one of the allowlist
    // 3. note that the allowlist is unioned together, not intersection
    // 4. skip if the allowlist.is_empty()
    // 5. verify that nft either does not have master edition or is master edition

    if *metadata.owner != mpl_token_metadata::ID {
        return Err(ErrorCode::AccountOwnedByWrongProgram.into());
    }
    if Metadata::find_pda(&mint.key()).0 != metadata.key() {
        return Err(ErrorCode::ConstraintSeeds.into());
    }
    let parsed_metadata = Metadata::safe_deserialize(&metadata.data.borrow())?;
    if let Some(master_edition) = master_edition {
        if MasterEdition::find_pda(&mint.key()).0 != master_edition.key() {
            return Err(ErrorCode::ConstraintSeeds.into());
        }
        if !master_edition.data_is_empty() {
            if master_edition.owner.ne(&mpl_token_metadata::ID) {
                return Err(ErrorCode::AccountOwnedByWrongProgram.into());
            }
            if !check_master_edition(master_edition) {
                return Err(MMMErrorCode::InvalidMasterEdition.into());
            }
        }
    }

    if allowlists
        .iter()
        .any(|&val| val.kind == ALLOWLIST_KIND_METADATA)
    {
        // If allowlist_aux is not passed in, do not validate URI.
        if let Some(ref aux_key) = allowlist_aux {
            // Handle URI padding.
            if !parsed_metadata.uri.trim().starts_with(aux_key) {
                msg!(
                    "Failed metadata validation. Expected URI: |{}| but got |{}|",
                    *aux_key,
                    parsed_metadata.uri
                );
                return Err(MMMErrorCode::UnexpectedMetadataUri.into());
            }
        }
    }

    for allowlist_val in allowlists.iter() {
        match allowlist_val.kind {
            ALLOWLIST_KIND_EMPTY => {}
            ALLOWLIST_KIND_ANY => {
                // any is a special case, we don't need to check anything else
                return Ok(parsed_metadata);
            }
            ALLOWLIST_KIND_FVCA => {
                if let Some(ref creators) = parsed_metadata.creators {
                    // TODO: can we make sure we only take master_edition here?
                    if !creators.is_empty()
                        && creators[0].address == allowlist_val.value
                        && creators[0].verified
                    {
                        return Ok(parsed_metadata);
                    }
                }
            }
            ALLOWLIST_KIND_MINT => {
                if mint.key() == allowlist_val.value {
                    return Ok(parsed_metadata);
                }
            }
            ALLOWLIST_KIND_MCC => {
                if let Some(ref collection_data) = parsed_metadata.collection {
                    if collection_data.key == allowlist_val.value && collection_data.verified {
                        return Ok(parsed_metadata);
                    }
                }
            }
            ALLOWLIST_KIND_METADATA => {
                // Do not validate URI here, as we already did it above.
                // These checks are separate since allowlist values are unioned together.
                continue;
            }
            _ => {
                return Err(MMMErrorCode::InvalidAllowLists.into());
            }
        }
    }

    // at the end, we didn't find a match, thus return err
    Err(MMMErrorCode::InvalidAllowLists.into())
}

pub fn check_curve(curve_type: u8, curve_delta: u64) -> Result<()> {
    // So far we only allow linear and exponential curves
    // 0: linear
    // 1: exp
    if curve_type > 1 {
        return Err(MMMErrorCode::InvalidCurveType.into());
    }

    // If the curve type is exp, then the curve_delta should follow bp format,
    // which is less than 10000
    if curve_type == 1 && curve_delta > 10000 {
        return Err(MMMErrorCode::InvalidCurveDelta.into());
    }

    Ok(())
}

pub fn get_buyside_seller_receives(
    total_sol_price: u64,
    lp_fee_bp: u16,
    royalty_bp: u16,
    buyside_creator_royalty_bp: u16,
) -> Result<u64> {
    let royalty_part = u128::from(royalty_bp)
        .checked_mul(u128::from(buyside_creator_royalty_bp))
        .ok_or(MMMErrorCode::NumericOverflow)?;
    let all_fees = u128::from(lp_fee_bp)
        .checked_mul(10000)
        .and_then(|v| v.checked_add(royalty_part))
        .and_then(|v| v.checked_add(10000 * 10000))
        .ok_or(MMMErrorCode::NumericOverflow)?;
    u128::from(total_sol_price)
        .checked_mul(10000 * 10000)
        .and_then(|v| v.checked_div(all_fees))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or(MMMErrorCode::NumericOverflow.into())
}

pub fn get_lp_fee_bp(pool: &Pool, buyside_sol_escrow_balance: u64) -> u16 {
    if pool.sellside_asset_amount < 1 {
        return 0;
    }

    if buyside_sol_escrow_balance < pool.spot_price {
        return 0;
    }

    pool.lp_fee_bp
}

pub fn get_sol_lp_fee(
    pool: &Pool,
    buyside_sol_escrow_balance: u64,
    total_sol_price: u64,
) -> Result<u64> {
    let lp_fee_bp = get_lp_fee_bp(pool, buyside_sol_escrow_balance);

    Ok(((total_sol_price as u128)
        .checked_mul(lp_fee_bp as u128)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_div(10000)
        .ok_or(MMMErrorCode::NumericOverflow)?) as u64)
}

pub fn get_sol_fee(total_sol_price: u64, fee_bp: i16) -> Result<i64> {
    i64::try_from(
        (total_sol_price as i128)
            .checked_mul(fee_bp as i128)
            .ok_or(MMMErrorCode::NumericOverflow)?
            .checked_div(10000)
            .ok_or(MMMErrorCode::NumericOverflow)?,
    )
    .map_err(|_| MMMErrorCode::NumericOverflow.into())
}

pub fn get_sol_total_price_and_next_price(
    pool: &Pool,
    n: u64,
    fulfill_buy: bool,
) -> Result<(u64, u64)> {
    // the price needs to go down
    let p = pool.spot_price;
    let delta = pool.curve_delta;
    let ret = match fulfill_buy {
        true => {
            match pool.curve_type {
                CURVE_KIND_LINEAR => {
                    // n*(2*p-(n-1)*delta)/2
                    let total_price = n
                        .checked_mul(
                            p.checked_mul(2)
                                .ok_or(MMMErrorCode::NumericOverflow)?
                                .checked_sub(
                                    n.checked_sub(1)
                                        .ok_or(MMMErrorCode::NumericOverflow)?
                                        .checked_mul(delta)
                                        .ok_or(MMMErrorCode::NumericOverflow)?,
                                )
                                .ok_or(MMMErrorCode::NumericOverflow)?,
                        )
                        .ok_or(MMMErrorCode::NumericOverflow)?
                        .checked_div(2)
                        .ok_or(MMMErrorCode::NumericOverflow)?;
                    // p - n * delta
                    let final_price = p
                        .checked_sub(n.checked_mul(delta).ok_or(MMMErrorCode::NumericOverflow)?)
                        .ok_or(MMMErrorCode::NumericOverflow)?;
                    Ok((total_price, final_price))
                }
                CURVE_KIND_EXP => {
                    // for loop to prevent overflow
                    let mut total_price: u64 = 0;
                    let mut curr_price: u128 = p as u128;
                    for _ in 0..n {
                        total_price = total_price
                            .checked_add(curr_price as u64)
                            .ok_or(MMMErrorCode::NumericOverflow)?;
                        curr_price = curr_price
                            .checked_mul(10000)
                            .ok_or(MMMErrorCode::NumericOverflow)?
                            .checked_div(
                                (delta as u128)
                                    .checked_add(10000)
                                    .ok_or(MMMErrorCode::NumericOverflow)?,
                            )
                            .ok_or(MMMErrorCode::NumericOverflow)?;
                    }
                    Ok((total_price, curr_price as u64))
                }
                _ => Err(MMMErrorCode::InvalidCurveType.into()),
            }
        }
        false => {
            // for sales, all prices will be one "step" away from the spot price to prevent pool drain
            match pool.curve_type {
                CURVE_KIND_LINEAR => {
                    // n*(2*p+(n+1)*delta)/2
                    let total_price = n
                        .checked_mul(
                            p.checked_mul(2)
                                .ok_or(MMMErrorCode::NumericOverflow)?
                                .checked_add(
                                    n.checked_add(1)
                                        .ok_or(MMMErrorCode::NumericOverflow)?
                                        .checked_mul(delta)
                                        .ok_or(MMMErrorCode::NumericOverflow)?,
                                )
                                .ok_or(MMMErrorCode::NumericOverflow)?,
                        )
                        .ok_or(MMMErrorCode::NumericOverflow)?
                        .checked_div(2)
                        .ok_or(MMMErrorCode::NumericOverflow)?;
                    // p - n * delta
                    let final_price = p
                        .checked_add(n.checked_mul(delta).ok_or(MMMErrorCode::NumericOverflow)?)
                        .ok_or(MMMErrorCode::NumericOverflow)?;
                    Ok((total_price, final_price))
                }
                CURVE_KIND_EXP => {
                    let mut total_price: u64 = 0;
                    let mut curr_price: u128 = p as u128;
                    for _ in 0..n {
                        curr_price = curr_price
                            .checked_mul(
                                (delta as u128)
                                    .checked_add(10000)
                                    .ok_or(MMMErrorCode::NumericOverflow)?,
                            )
                            .ok_or(MMMErrorCode::NumericOverflow)?
                            .checked_div(10000)
                            .ok_or(MMMErrorCode::NumericOverflow)?;
                        total_price = total_price
                            .checked_add(curr_price as u64)
                            .ok_or(MMMErrorCode::NumericOverflow)?;
                    }
                    Ok((total_price, curr_price as u64))
                }
                _ => Err(MMMErrorCode::InvalidCurveType.into()),
            }
        }
    };

    match ret {
        Ok((total_price, final_price)) => {
            if total_price == 0 {
                return Err(MMMErrorCode::NumericOverflow.into());
            }

            if total_price > MAX_TOTAL_PRICE {
                return Err(MMMErrorCode::NumericOverflow.into());
            }
            Ok((total_price, final_price))
        }
        Err(e) => Err(e),
    }
}

pub fn try_close_pool<'info>(pool: &Account<'info, Pool>, owner: AccountInfo<'info>) -> Result<()> {
    if pool.sellside_asset_amount != 0 {
        return Ok(());
    }

    if pool.buyside_payment_amount != 0 {
        return Ok(());
    }

    if pool.using_shared_escrow() && pool.shared_escrow_count != 0 {
        return Ok(());
    }

    pool.to_account_info()
        .data
        .borrow_mut()
        .copy_from_slice(&[0; Pool::LEN]);

    let curr_lamports = pool.to_account_info().lamports();
    **pool.to_account_info().lamports.borrow_mut() = 0;
    **owner.lamports.borrow_mut() = owner.lamports().checked_add(curr_lamports).unwrap();
    Ok(())
}

pub fn try_close_escrow<'info>(
    escrow: &AccountInfo<'info>,
    pool: &Account<'info, Pool>,
    system_program: &Program<'info, System>,
    escrow_seeds: &[&[&[u8]]],
) -> Result<()> {
    // minimum rent needed to sustain a 0 data account
    let min_rent = Rent::get()?.minimum_balance(0);
    // if the balance is less than a small percentage of the spot price, then close the escrow
    let min_escrow_balance: u64 = if pool.reinvest_fulfill_sell && pool.sellside_asset_amount > 0 {
        // pool balance can increase, so we just use min_rent as default amount
        min_rent
    } else {
        // pool balance cannot increase without manual deposit, so we calculate the actual value
        (u128::from(pool.spot_price))
            .checked_mul(u128::from(MIN_SOL_ESCROW_BALANCE_BP))
            .and_then(|v| v.checked_div(10000))
            .and_then(|v| u64::try_from(v).ok())
            .ok_or(MMMErrorCode::NumericOverflow)?
    };
    let escrow_lamports = escrow.lamports();
    if escrow_lamports == 0 || escrow_lamports > std::cmp::max(min_rent, min_escrow_balance) {
        Ok(())
    } else {
        anchor_lang::solana_program::program::invoke_signed(
            &anchor_lang::solana_program::system_instruction::transfer(
                escrow.key,
                &pool.key(),
                escrow_lamports,
            ),
            &[
                escrow.clone(),
                pool.to_account_info(),
                system_program.to_account_info(),
            ],
            escrow_seeds,
        )?;
        Ok(())
    }
}

pub fn try_close_sell_state<'info>(
    sell_state: &Account<'info, SellState>,
    owner: AccountInfo<'info>,
) -> Result<()> {
    if sell_state.asset_amount != 0 {
        return Ok(());
    }

    sell_state
        .to_account_info()
        .data
        .borrow_mut()
        .copy_from_slice(&[0; SellState::LEN]);

    let curr_lamports = sell_state.to_account_info().lamports();
    **sell_state.to_account_info().lamports.borrow_mut() = 0;
    **owner.lamports.borrow_mut() = owner.lamports().checked_add(curr_lamports).unwrap();
    Ok(())
}

pub fn get_metadata_royalty_bp(
    total_price: u64,
    parsed_metadata: &Metadata,
    policy: Option<&Account<'_, Policy>>,
) -> u16 {
    match policy {
        None => parsed_metadata.seller_fee_basis_points,
        Some(p) => match &p.dynamic_royalty {
            None => parsed_metadata.seller_fee_basis_points,
            Some(dynamic_royalty) => {
                dynamic_royalty.get_royalty_bp(total_price, parsed_metadata.seller_fee_basis_points)
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub fn pay_creator_fees_in_sol<'info>(
    buyside_creator_royalty_bp: u16,
    total_price: u64,
    parsed_metadata: &Metadata,
    creator_accounts: &[AccountInfo<'info>],
    payer: AccountInfo<'info>,
    metadata_royalty_bp: u16,
    payer_seeds: &[&[&[u8]]],
    system_program: AccountInfo<'info>,
) -> Result<u64> {
    // total royalty paid by the buyer, it's one of the following
    //   - buyside_sol_escrow_account (when fulfill buy)
    //   - payer                      (when fulfill sell)
    // returns the total royalty paid
    //   royalty = spot_price * (royalty_bp / 10000) * (buyside_creator_royalty_bp / 10000)
    let royalty = ((total_price as u128)
        .checked_mul(metadata_royalty_bp as u128)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_div(10000)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_mul(buyside_creator_royalty_bp as u128)
        .ok_or(MMMErrorCode::NumericOverflow)?
        .checked_div(10000)
        .ok_or(MMMErrorCode::NumericOverflow)?) as u64;

    if royalty == 0 {
        return Ok(0);
    }

    let creators = if let Some(creators) = &parsed_metadata.creators {
        creators
    } else {
        return Ok(0);
    };

    if payer.lamports() < royalty {
        return Err(MMMErrorCode::NotEnoughBalance.into());
    }

    // hardcoded the max threshold for InvalidMetadataCreatorRoyalty
    if parsed_metadata.seller_fee_basis_points > MAX_METADATA_CREATOR_ROYALTY_BP {
        return Err(MMMErrorCode::InvalidMetadataCreatorRoyalty.into());
    }
    let min_rent = Rent::get()?.minimum_balance(0);
    let mut total_royalty: u64 = 0;

    let creator_accounts_iter = &mut creator_accounts.iter();
    for (index, creator) in creators.iter().enumerate() {
        let creator_fee = if index == creators.len() - 1 {
            royalty
                .checked_sub(total_royalty)
                .ok_or(MMMErrorCode::NumericOverflow)?
        } else {
            (royalty as u128)
                .checked_mul(creator.share as u128)
                .ok_or(MMMErrorCode::NumericOverflow)?
                .checked_div(100)
                .ok_or(MMMErrorCode::NumericOverflow)? as u64
        };
        let current_creator_info = next_account_info(creator_accounts_iter)?;
        if creator.address.ne(current_creator_info.key) {
            return Err(MMMErrorCode::InvalidCreatorAddress.into());
        }
        let current_creator_lamports = current_creator_info.lamports();
        if creator_fee > 0
            && current_creator_lamports
                .checked_add(creator_fee)
                .ok_or(MMMErrorCode::NumericOverflow)?
                > min_rent
        {
            anchor_lang::solana_program::program::invoke_signed(
                &anchor_lang::solana_program::system_instruction::transfer(
                    payer.key,
                    current_creator_info.key,
                    creator_fee,
                ),
                &[
                    payer.to_account_info(),
                    current_creator_info.to_account_info(),
                    system_program.to_account_info(),
                ],
                payer_seeds,
            )?;
            total_royalty = total_royalty
                .checked_add(creator_fee)
                .ok_or(MMMErrorCode::NumericOverflow)?;
        }
    }
    Ok(total_royalty)
}

pub fn log_pool(prefix: &str, pool: &Pool) -> Result<()> {
    msg!(prefix);
    sol_log_data(&[&pool.try_to_vec()?]);
    Ok(())
}

pub fn assert_is_programmable(parsed_metadata: &Metadata) -> Result<()> {
    if parsed_metadata.token_standard == Some(TokenStandard::ProgrammableNonFungible) {
        Ok(())
    } else {
        Err(MMMErrorCode::InvalidTokenStandard.into())
    }
}

pub fn assert_valid_fees_bp(maker_fee_bp: i16, taker_fee_bp: i16) -> Result<()> {
    let bound = MAX_REFERRAL_FEE_BP;
    if !(0..=bound).contains(&taker_fee_bp) {
        return Err(MMMErrorCode::InvalidMakerOrTakerFeeBP.into());
    }

    if !(-bound..=bound).contains(&maker_fee_bp) {
        return Err(MMMErrorCode::InvalidMakerOrTakerFeeBP.into());
    }

    let sum = maker_fee_bp + taker_fee_bp;
    if !(0..=bound).contains(&sum) {
        return Err(MMMErrorCode::InvalidMakerOrTakerFeeBP.into());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn withdraw_m2<'info>(
    pool: &Account<'info, Pool>,
    pool_bump: u8,
    to: &AccountInfo<'info>,
    m2_buyer_escrow: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    m2_program: &AccountInfo<'info>,
    wallet: Pubkey,
    amount: u64,
) -> Result<()> {
    let pool_seeds: &[&[&[u8]]] = &[&[
        POOL_PREFIX.as_bytes(),
        pool.owner.as_ref(),
        pool.uuid.as_ref(),
        &[pool_bump],
    ]];

    let ix = withdraw_by_mmm_ix_with_program_id(
        M2_PROGRAM,
        WithdrawByMmmKeys {
            mmm_pool: pool.key(),
            to: to.key(),
            escrow_payment_account: m2_buyer_escrow.key(),
            system_program: system_program.key(),
        },
        WithdrawByMmmIxArgs {
            args: WithdrawByMMMArgs {
                wallet,
                auction_house: M2_AUCTION_HOUSE,
                amount,
                mmm_pool_uuid: pool.uuid,
            },
        },
    )?;

    invoke_signed(
        &ix,
        &[
            pool.to_account_info(),
            to.to_account_info(),
            m2_buyer_escrow.to_account_info(),
            system_program.to_account_info(),
            m2_program.to_account_info(),
        ],
        pool_seeds,
    )?;

    Ok(())
}

pub fn check_remaining_accounts_for_m2(
    remaining_accounts: &[AccountInfo],
    pool_owner: &Pubkey,
) -> Result<()> {
    // check the remaining accounts at position 0 and 1
    // 0 has to be the m2_program
    // 1 has to be the shared_escrow_account pda of the m2_program
    if remaining_accounts.len() < 2 {
        return Err(MMMErrorCode::InvalidRemainingAccounts.into());
    }

    if *remaining_accounts[0].key != M2_PROGRAM {
        return Err(MMMErrorCode::InvalidRemainingAccounts.into());
    }

    let shared_escrow_account = &remaining_accounts[1];

    let (m2_shared_escrow_pda, _) = Pubkey::find_program_address(
        &[
            M2_PREFIX.as_bytes(),
            M2_AUCTION_HOUSE.as_ref(),
            pool_owner.as_ref(),
        ],
        &M2_PROGRAM,
    );
    if m2_shared_escrow_pda != shared_escrow_account.key() {
        return Err(MMMErrorCode::InvalidRemainingAccounts.into());
    }

    Ok(())
}

pub fn check_allowlists_for_mint_ext(
    allowlists: &[Allowlist],
    token_mint: &AccountInfo,
    allowlist_aux: Option<String>,
) -> Result<TokenMetadata> {
    if token_mint.owner != &spl_token_2022::ID || token_mint.data_is_empty() {
        return Err(MMMErrorCode::InvalidTokenMint.into());
    }
    let borrowed_data = token_mint.data.borrow();
    let mint_deserialized = StateWithExtensions::<Token22Mint>::unpack(&borrowed_data)?;
    if !mint_deserialized.base.is_initialized {
        return Err(MMMErrorCode::InvalidTokenMint.into());
    }

    // verify metadata extension
    if let Ok(metadata_ptr) = mint_deserialized.get_extension::<MetadataPointer>() {
        if Option::<Pubkey>::from(metadata_ptr.metadata_address) != Some(*token_mint.key) {
            return Err(MMMErrorCode::InvalidTokenMetadataExtension.into());
        }
    }
    let parsed_metadata = mint_deserialized
        .get_variable_len_extension::<TokenMetadata>()
        .unwrap();

    if allowlists
        .iter()
        .any(|&val| val.kind == ALLOWLIST_KIND_METADATA)
    {
        // If allowlist_aux is not passed in, do not validate URI.
        if let Some(ref aux_key) = allowlist_aux {
            // Handle URI padding.
            if !parsed_metadata.uri.trim().starts_with(aux_key) {
                msg!(
                    "Failed metadata validation. Expected URI: |{}| but got |{}|",
                    *aux_key,
                    parsed_metadata.uri
                );
                return Err(MMMErrorCode::UnexpectedMetadataUri.into());
            }
        }
    }

    // verify group member extension
    if let Ok(group_member_ptr) = mint_deserialized.get_extension::<GroupMemberPointer>() {
        if Some(*token_mint.key) != Option::<Pubkey>::from(group_member_ptr.member_address) {
            msg!("group member pointer does not point to itself");
            return Err(MMMErrorCode::InvalidTokenMemberExtension.into());
        }
    }

    for allowlist_val in allowlists.iter() {
        match allowlist_val.kind {
            ALLOWLIST_KIND_EMPTY => {}
            ALLOWLIST_KIND_ANY => {
                // any is a special case, we don't need to check anything else
                return Ok(parsed_metadata);
            }
            ALLOWLIST_KIND_FVCA => {
                return Err(MMMErrorCode::InvalidAllowLists.into());
            }
            ALLOWLIST_KIND_MINT => {
                if token_mint.key() == allowlist_val.value {
                    return Ok(parsed_metadata);
                }
            }
            ALLOWLIST_KIND_MCC => {
                return Err(MMMErrorCode::InvalidAllowLists.into());
            }
            ALLOWLIST_KIND_GROUP => {
                let group_address = assert_valid_group(&mint_deserialized, token_mint)?;
                if group_address != Some(allowlist_val.value) {
                    msg!("group address |{}| is not allowed", group_address.unwrap());
                    return Err(MMMErrorCode::InvalidAllowLists.into());
                }
                return Ok(parsed_metadata);
            }
            ALLOWLIST_KIND_METADATA => {
                // Do not validate URI here, as we already did it above.
                // Group is validated in a separate function.
                // These checks are separate since allowlist values are unioned together.
                continue;
            }
            _ => {
                return Err(MMMErrorCode::InvalidAllowLists.into());
            }
        }
    }

    // at the end, we didn't find a match, thus return err
    Err(MMMErrorCode::InvalidAllowLists.into())
}

pub fn assert_valid_group(
    mint_deserialized: &StateWithExtensions<'_, Token22Mint>,
    token_mint: &AccountInfo,
) -> Result<Option<Pubkey>> {
    if let Ok(group_member) = mint_deserialized.get_extension::<TokenGroupMember>() {
        // counter spoof check
        if group_member.mint != *token_mint.key {
            msg!("group member mint does not match the token mint");
            return Err(MMMErrorCode::InvalidTokenMemberExtension.into());
        }
        return Ok(Some(group_member.group));
    }
    Err(MMMErrorCode::InvalidTokenMemberExtension.into())
}

pub struct PoolPriceInfo<'info> {
    pub total_price: u64,
    pub next_price: u64,
    pub lp_fee: u64,
    pub maker_fee: i64,
    pub taker_fee: i64,
    pub referral_fee: u64,
    pub transfer_sol_to: AccountInfo<'info>,
}

pub fn get_sell_fulfill_pool_price_info<'info>(
    pool: &Pool,
    owner: &UncheckedAccount<'info>,
    buyside_sol_escrow_account: &AccountInfo<'info>,
    asset_amount: u64,
    maker_fee_bp: i16,
    taker_fee_bp: i16,
) -> Result<PoolPriceInfo<'info>> {
    let (total_price, next_price) = get_sol_total_price_and_next_price(pool, asset_amount, false)?;
    let lp_fee = get_sol_lp_fee(pool, buyside_sol_escrow_account.lamports(), total_price)?;

    assert_valid_fees_bp(maker_fee_bp, taker_fee_bp)?;
    let maker_fee = get_sol_fee(total_price, maker_fee_bp)?;
    let taker_fee = get_sol_fee(total_price, taker_fee_bp)?;
    let referral_fee = u64::try_from(
        maker_fee
            .checked_add(taker_fee)
            .ok_or(MMMErrorCode::NumericOverflow)?,
    )
    .map_err(|_| MMMErrorCode::NumericOverflow)?;

    let transfer_sol_to = if pool.reinvest_fulfill_sell {
        buyside_sol_escrow_account.to_account_info()
    } else {
        owner.to_account_info()
    };

    Ok(PoolPriceInfo {
        total_price,
        next_price,
        lp_fee,
        maker_fee,
        taker_fee,
        referral_fee,
        transfer_sol_to,
    })
}
