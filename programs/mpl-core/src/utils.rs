use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use mpl_utils::assert_signer;
use num_traits::{FromPrimitive, ToPrimitive};
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program::invoke,
    program_error::ProgramError, program_memory::sol_memcpy, rent::Rent, system_instruction,
    sysvar::Sysvar,
};

use crate::{
    error::MplCoreError,
    plugins::{
        create_meta_idempotent, initialize_plugin, validate_plugin_checks, CheckResult, Plugin,
        PluginHeader, PluginRegistry, PluginType, RegistryRecord, ValidationResult,
    },
    state::{
        Asset, Authority, Collection, Compressible, CompressionProof, CoreAsset, DataBlob,
        HashablePluginSchema, HashedAsset, HashedAssetSchema, Key, SolanaAccount, UpdateAuthority,
    },
};

/// Load the one byte key from the account data at the given offset.
pub fn load_key(account: &AccountInfo, offset: usize) -> Result<Key, ProgramError> {
    let key =
        Key::from_u8((*account.data).borrow()[offset]).ok_or(MplCoreError::DeserializationError)?;

    Ok(key)
}

/// Assert that the account info address is in the authorities array.
pub fn assert_authority<T: CoreAsset>(
    asset: &T,
    authority_info: &AccountInfo,
    authorities: &Authority,
) -> ProgramResult {
    solana_program::msg!("Update authority: {:?}", asset.update_authority());
    solana_program::msg!(
        "Check if {:?} matches {:?}",
        authority_info.key,
        authorities
    );
    match authorities {
        Authority::None => (),
        Authority::Owner => {
            if asset.owner() == authority_info.key {
                return Ok(());
            }
        }
        Authority::UpdateAuthority => {
            if asset.update_authority().key() == *authority_info.key {
                return Ok(());
            }
        }
        Authority::Pubkey { address } => {
            if authority_info.key == address {
                return Ok(());
            }
        }
        Authority::Permanent { address } => {
            if authority_info.key == address {
                return Ok(());
            }
        }
    }

    Err(MplCoreError::InvalidAuthority.into())
}

/// Assert that the account info address is in the authorities array.
pub fn assert_collection_authority(
    asset: &Collection,
    authority_info: &AccountInfo,
    authority: &Authority,
) -> ProgramResult {
    match authority {
        Authority::None | Authority::Owner => (),
        Authority::UpdateAuthority => {
            if &asset.update_authority == authority_info.key {
                return Ok(());
            }
        }
        Authority::Pubkey { address } => {
            if authority_info.key == address {
                return Ok(());
            }
        }
        Authority::Permanent { address } => {
            if authority_info.key == address {
                return Ok(());
            }
        }
    }

    Err(MplCoreError::InvalidAuthority.into())
}

/// Fetch the core data from the account; asset, plugin header (if present), and plugin registry (if present).
pub fn fetch_core_data<T: DataBlob + SolanaAccount>(
    account: &AccountInfo,
) -> Result<(T, Option<PluginHeader>, Option<PluginRegistry>), ProgramError> {
    let asset = T::load(account, 0)?;

    if asset.get_size() != account.data_len() {
        let plugin_header = PluginHeader::load(account, asset.get_size())?;
        let plugin_registry = PluginRegistry::load(account, plugin_header.plugin_registry_offset)?;

        Ok((asset, Some(plugin_header), Some(plugin_registry)))
    } else {
        Ok((asset, None, None))
    }
}

/// Check that a compression proof results in same on-chain hash.
pub fn verify_proof(
    hashed_asset: &AccountInfo,
    compression_proof: &CompressionProof,
) -> Result<(Asset, Vec<HashablePluginSchema>), ProgramError> {
    let asset = Asset::from(compression_proof.clone());
    let asset_hash = asset.hash()?;

    let mut sorted_plugins = compression_proof.plugins.clone();
    sorted_plugins.sort_by(HashablePluginSchema::compare_indeces);

    let plugin_hashes = sorted_plugins
        .iter()
        .map(|plugin| plugin.hash())
        .collect::<Result<Vec<[u8; 32]>, ProgramError>>()?;

    let hashed_asset_schema = HashedAssetSchema {
        asset_hash,
        plugin_hashes,
    };

    let hashed_asset_schema_hash = hashed_asset_schema.hash()?;

    let current_account_hash = HashedAsset::load(hashed_asset, 0)?.hash;
    if hashed_asset_schema_hash != current_account_hash {
        return Err(MplCoreError::IncorrectAssetHash.into());
    }

    Ok((asset, sorted_plugins))
}

pub(crate) fn close_program_account<'a>(
    account_to_close_info: &AccountInfo<'a>,
    funds_dest_account_info: &AccountInfo<'a>,
) -> ProgramResult {
    let rent = Rent::get()?;

    let account_size = account_to_close_info.data_len();
    let account_rent = rent.minimum_balance(account_size);
    let one_byte_rent = rent.minimum_balance(1);

    let amount_to_return = account_rent
        .checked_sub(one_byte_rent)
        .ok_or(MplCoreError::NumericalOverflowError)?;

    // Transfer lamports from the account to the destination account.
    let dest_starting_lamports = funds_dest_account_info.lamports();
    **funds_dest_account_info.lamports.borrow_mut() = dest_starting_lamports
        .checked_add(amount_to_return)
        .ok_or(MplCoreError::NumericalOverflowError)?;
    **account_to_close_info.try_borrow_mut_lamports()? -= amount_to_return;

    account_to_close_info.realloc(1, false)?;
    account_to_close_info.data.borrow_mut()[0] = Key::Uninitialized.to_u8().unwrap();

    Ok(())
}

/// Resize an account using realloc and retain any lamport overages, modified from Solana Cookbook
pub(crate) fn resize_or_reallocate_account<'a>(
    target_account: &AccountInfo<'a>,
    funding_account: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    new_size: usize,
) -> ProgramResult {
    let rent = Rent::get()?;
    let new_minimum_balance = rent.minimum_balance(new_size);
    let current_minimum_balance = rent.minimum_balance(target_account.data_len());
    let account_infos = &[
        funding_account.clone(),
        target_account.clone(),
        system_program.clone(),
    ];

    if new_minimum_balance >= current_minimum_balance {
        let lamports_diff = new_minimum_balance.saturating_sub(current_minimum_balance);
        invoke(
            &system_instruction::transfer(funding_account.key, target_account.key, lamports_diff),
            account_infos,
        )?;
    } else {
        // return lamports to the compressor
        let lamports_diff = current_minimum_balance.saturating_sub(new_minimum_balance);

        **funding_account.try_borrow_mut_lamports()? += lamports_diff;
        **target_account.try_borrow_mut_lamports()? -= lamports_diff
    }

    target_account.realloc(new_size, false)?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Validate asset permissions using lifecycle validations for asset, collection, and plugins.
pub fn validate_asset_permissions<'a>(
    authority: &AccountInfo<'a>,
    asset: &AccountInfo<'a>,
    collection: Option<&AccountInfo<'a>>,
    new_owner: Option<&AccountInfo<'a>>,
    asset_check_fp: fn() -> CheckResult,
    collection_check_fp: fn() -> CheckResult,
    plugin_check_fp: fn(&PluginType) -> CheckResult,
    asset_validate_fp: fn(&Asset, &AccountInfo) -> Result<ValidationResult, ProgramError>,
    collection_validate_fp: fn(&Collection, &AccountInfo) -> Result<ValidationResult, ProgramError>,
    plugin_validate_fp: fn(
        &Plugin,
        &AccountInfo,
        Option<&AccountInfo>,
        &Authority,
    ) -> Result<ValidationResult, ProgramError>,
) -> Result<(Asset, Option<PluginHeader>, Option<PluginRegistry>), ProgramError> {
    let (deserialized_asset, plugin_header, plugin_registry) = fetch_core_data::<Asset>(asset)?;

    let mut checks: BTreeMap<PluginType, (Key, CheckResult, RegistryRecord)> = BTreeMap::new();

    // The asset approval overrides the collection approval.
    let asset_approval = asset_check_fp();
    let core_check = match asset_approval {
        CheckResult::None => (Key::Collection, collection_check_fp()),
        _ => (Key::Asset, asset_approval),
    };

    // Check the collection plugins first.
    if let Some(collection_info) = collection {
        fetch_core_data::<Collection>(collection_info).map(|(_, _, registry)| {
            registry.map(|r| {
                r.check_registry(Key::Collection, plugin_check_fp, &mut checks);
                r
            })
        })?;
    }

    // Next check the asset plugins. Plugins on the asset override the collection plugins,
    // so we don't need to validate the collection plugins if the asset has a plugin.
    if let Some(registry) = plugin_registry.as_ref() {
        registry.check_registry(Key::Asset, plugin_check_fp, &mut checks);
    }

    solana_program::msg!("checks: {:#?}", checks);

    // Do the core validation.
    let mut approved = matches!(
        core_check,
        (
            Key::Asset | Key::Collection,
            CheckResult::CanApprove | CheckResult::CanReject
        )
    ) && {
        (match core_check.0 {
            Key::Collection => collection_validate_fp(
                &Collection::load(collection.ok_or(MplCoreError::InvalidCollection)?, 0)?,
                authority,
            )?,
            Key::Asset => asset_validate_fp(&Asset::load(asset, 0)?, authority)?,
            _ => return Err(MplCoreError::IncorrectAccount.into()),
        }) == ValidationResult::Approved
    };
    solana_program::msg!("approved: {:#?}", approved);

    approved = validate_plugin_checks(
        Key::Collection,
        &checks,
        authority,
        new_owner,
        asset,
        collection,
        plugin_validate_fp,
    )? || approved;

    approved = validate_plugin_checks(
        Key::Asset,
        &checks,
        authority,
        new_owner,
        asset,
        collection,
        plugin_validate_fp,
    )? || approved;

    if !approved {
        return Err(MplCoreError::InvalidAuthority.into());
    }

    Ok((deserialized_asset, plugin_header, plugin_registry))
}

/// Validate collection permissions using lifecycle validations for collection and plugins.
pub fn validate_collection_permissions<'a>(
    authority: &AccountInfo<'a>,
    collection: &AccountInfo<'a>,
    collection_check_fp: fn() -> CheckResult,
    plugin_check_fp: fn(&PluginType) -> CheckResult,
    collection_validate_fp: fn(&Collection, &AccountInfo) -> Result<ValidationResult, ProgramError>,
    plugin_validate_fp: fn(
        &Plugin,
        &AccountInfo,
        Option<&AccountInfo>,
        &Authority,
    ) -> Result<ValidationResult, ProgramError>,
) -> Result<(Collection, Option<PluginHeader>, Option<PluginRegistry>), ProgramError> {
    let (deserialized_collection, plugin_header, plugin_registry) =
        fetch_core_data::<Collection>(collection)?;

    // let checks: [(Key, CheckResult); PluginType::COUNT + 2];

    let mut approved = false;
    match collection_check_fp() {
        CheckResult::CanApprove | CheckResult::CanReject => {
            match collection_validate_fp(&deserialized_collection, authority)? {
                ValidationResult::Approved => {
                    approved = true;
                }
                ValidationResult::Rejected => return Err(MplCoreError::InvalidAuthority.into()),
                ValidationResult::Pass => (),
            }
        }
        CheckResult::None => (),
    };

    if let Some(plugin_registry) = &plugin_registry {
        for record in plugin_registry.registry.iter() {
            if matches!(
                plugin_check_fp(&record.plugin_type),
                CheckResult::CanApprove | CheckResult::CanReject
            ) {
                let result = plugin_validate_fp(
                    &Plugin::load(collection, record.offset)?,
                    authority,
                    None,
                    &record.authority,
                )?;
                if result == ValidationResult::Rejected {
                    return Err(MplCoreError::InvalidAuthority.into());
                } else if result == ValidationResult::Approved {
                    approved = true;
                }
            }
        }
    };

    if !approved {
        return Err(MplCoreError::InvalidAuthority.into());
    }

    Ok((deserialized_collection, plugin_header, plugin_registry))
}

/// Take an `Asset` and Vec of `HashablePluginSchema` and rebuild the asset in account space.
pub fn rebuild_account_state_from_proof_data<'a>(
    asset: Asset,
    plugins: Vec<HashablePluginSchema>,
    asset_info: &AccountInfo<'a>,
    payer: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
) -> ProgramResult {
    let serialized_data = asset.try_to_vec()?;
    resize_or_reallocate_account(asset_info, payer, system_program, serialized_data.len())?;

    sol_memcpy(
        &mut asset_info.try_borrow_mut_data()?,
        &serialized_data,
        serialized_data.len(),
    );

    // Add the plugins.
    if !plugins.is_empty() {
        create_meta_idempotent::<Asset>(asset_info, payer, system_program)?;

        for plugin in plugins {
            initialize_plugin::<Asset>(
                &plugin.plugin,
                &plugin.authority,
                asset_info,
                payer,
                system_program,
            )?;
        }
    }

    Ok(())
}

/// Take `Asset` and `PluginRegistry` for a decompressed asset, and compress into account space.
pub fn compress_into_account_space<'a>(
    asset: Asset,
    plugin_registry: Option<PluginRegistry>,
    asset_info: &AccountInfo<'a>,
    payer: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
) -> Result<CompressionProof, ProgramError> {
    let asset_hash = asset.hash()?;
    let mut compression_proof = CompressionProof::new(asset, vec![]);
    let mut plugin_hashes = vec![];
    if let Some(plugin_registry) = plugin_registry {
        let mut registry_records = plugin_registry.registry;

        // It should already be sorted but we just want to make sure.
        registry_records.sort_by(RegistryRecord::compare_offsets);

        for (i, record) in registry_records.into_iter().enumerate() {
            let plugin = Plugin::deserialize(&mut &(*asset_info.data).borrow()[record.offset..])?;

            let hashable_plugin_schema = HashablePluginSchema {
                index: i,
                authority: record.authority,
                plugin,
            };

            let plugin_hash = hashable_plugin_schema.hash()?;
            plugin_hashes.push(plugin_hash);

            compression_proof.plugins.push(hashable_plugin_schema);
        }
    }

    let hashed_asset_schema = HashedAssetSchema {
        asset_hash,
        plugin_hashes,
    };

    let hashed_asset = HashedAsset::new(hashed_asset_schema.hash()?);
    let serialized_data = hashed_asset.try_to_vec()?;

    resize_or_reallocate_account(asset_info, payer, system_program, serialized_data.len())?;

    sol_memcpy(
        &mut asset_info.try_borrow_mut_data()?,
        &serialized_data,
        serialized_data.len(),
    );

    Ok(compression_proof)
}

pub(crate) fn resolve_to_authority(
    authority_info: &AccountInfo,
    maybe_collection_info: Option<&AccountInfo>,
    asset: &Asset,
) -> Result<Authority, ProgramError> {
    let authority_type = if authority_info.key == &asset.owner {
        Authority::Owner
    } else if asset.update_authority == UpdateAuthority::Address(*authority_info.key) {
        Authority::UpdateAuthority
    } else if let UpdateAuthority::Collection(collection_address) = asset.update_authority {
        match maybe_collection_info {
            Some(collection_info) => {
                if collection_info.key != &collection_address {
                    return Err(MplCoreError::InvalidCollection.into());
                }
                let collection: Collection = Collection::load(collection_info, 0)?;
                if authority_info.key == &collection.update_authority {
                    Authority::UpdateAuthority
                } else {
                    Authority::Pubkey {
                        address: *authority_info.key,
                    }
                }
            }
            None => return Err(MplCoreError::InvalidCollection.into()),
        }
    } else {
        Authority::Pubkey {
            address: *authority_info.key,
        }
    };
    Ok(authority_type)
}

/// Resolves the payer for the transaction for an optional payer pattern.
pub(crate) fn resolve_payer<'a>(
    authority: &'a AccountInfo<'a>,
    payer: Option<&'a AccountInfo<'a>>,
) -> Result<&'a AccountInfo<'a>, ProgramError> {
    match payer {
        Some(payer) => {
            assert_signer(payer).unwrap();
            Ok(payer)
        }
        None => Ok(authority),
    }
}
