use soroban_sdk::{contractclient, Address, Env, Vec};
use templar_proxy_oracle_soroban_common::{Asset, ContractError};

#[contractclient(name = "ParentIntrospectionClient")]
pub trait ParentIntrospection {
    fn registered_assets(env: Env) -> Result<Vec<Asset>, ContractError>;
    fn get_owner(env: Env) -> Option<Address>;
}

/// The runtime's permissionless `refresh`, used only by the eager build. The
/// adapter ignores the returned `RefreshStatus` and reads the cache the refresh
/// leaves behind, so `Val` spares it a copy of the runtime's type.
#[cfg(feature = "dangerous-eager-refresh")]
#[contractclient(name = "ParentMaintenanceClient")]
pub trait ParentMaintenance {
    fn refresh(env: Env, asset: Asset) -> soroban_sdk::Val;
}
