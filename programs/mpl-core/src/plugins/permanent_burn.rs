use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{account_info::AccountInfo, program_error::ProgramError};

use crate::state::{Authority, DataBlob};

use super::{Plugin, PluginValidation, ValidationResult};

/// The permanent burn plugin allows any authority to burn the asset.
/// The default authority for this plugin is the update authority.
#[repr(C)]
#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, Debug, Default, PartialEq, Eq)]
pub struct PermanentBurn {}

impl DataBlob for PermanentBurn {
    fn get_initial_size() -> usize {
        0
    }

    fn get_size(&self) -> usize {
        0
    }
}

impl PluginValidation for PermanentBurn {
    fn validate_add_plugin(
        &self,
        _authority: &AccountInfo,
        _authorities: &Authority,
        _new_plugin: Option<&Plugin>,
    ) -> Result<ValidationResult, ProgramError> {
        // This plugin can only be added at creation time, so we
        // always reject it.
        Ok(ValidationResult::Rejected)
    }

    fn validate_revoke_plugin_authority(
        &self,
        _authority: &AccountInfo,
        _authorities: &Authority,
        _plugin_to_revoke: Option<&Plugin>,
    ) -> Result<ValidationResult, ProgramError> {
        Ok(ValidationResult::Approved)
    }

    fn validate_burn(
        &self,
        _authority: &AccountInfo,
        authorities: &Authority,
        resolved_authority: Option<&Authority>,
    ) -> Result<ValidationResult, ProgramError> {
        if let Some(resolved_authority) = resolved_authority {
            if resolved_authority == authorities {
                return Ok(ValidationResult::ForceApproved);
            }
        }

        Ok(ValidationResult::Pass)
    }
}
