#![no_std]
// UNAUDITED: the `dangerous-eager-refresh` feature (refresh-on-read in
// `lastprice`) is not part of the Guardian-reviewed adapter. Only the default
// build, with the feature off, is the audited artifact.
// Soroban contract entry points require `env: Env` and `Address` by value;
// every `#[contractimpl]` method in this crate is an ABI entry point.
#![allow(clippy::needless_pass_by_value)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Symbol, Vec,
};
use templar_proxy_oracle_soroban_common::{
    extend_instance_ttl, normalized_to_sep40, Asset, ContractError, NormalizedPrice, PriceData,
    PriceFeedTrait, ProxyOracleClient,
};

pub mod parent;
use parent::ParentIntrospectionClient;

const CONFIG: Symbol = symbol_short!("CONFIG");
const MAX_HISTORY_RECORDS: u32 = 32;

soroban_sdk::contractmeta!(key = "sep", val = "40");

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolOracleConfig {
    pub parent_oracle: Address,
    /// Construction-time parent owner snapshot, never live authority.
    pub deployment_owner: Address,
    pub decimals: u32,
    pub resolution: u32,
    pub base: Asset,
}

#[contracterror]
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AdapterError {
    AlreadyInitialized = 1,
    InvalidConfig = 2,
    ParentUnavailable = 3,
    ParentMismatch = 4,
}

/// Immutable SEP-40 view with deliberately no admin or upgrade surface.
#[contract]
pub struct PoolOracleAdapter;

#[contractimpl]
impl PoolOracleAdapter {
    pub fn __constructor(
        env: Env,
        deployment_owner: Address,
        parent_oracle: Address,
        decimals: u32,
        resolution: u32,
        base: Asset,
    ) -> Result<(), AdapterError> {
        if env.storage().instance().has(&CONFIG) {
            return Err(AdapterError::AlreadyInitialized);
        }
        if decimals > 18 || resolution == 0 {
            return Err(AdapterError::InvalidConfig);
        }
        let parent = ProxyOracleClient::new(&env, &parent_oracle);
        let source_base = match parent.try_source_base() {
            Ok(Ok(Some(value))) => value,
            _ => return Err(AdapterError::ParentUnavailable),
        };
        if source_base != base {
            return Err(AdapterError::ParentMismatch);
        }
        let introspection = ParentIntrospectionClient::new(&env, &parent_oracle);
        let assets = match introspection.try_registered_assets() {
            Ok(Ok(value)) => value,
            _ => return Err(AdapterError::ParentUnavailable),
        };
        if assets.is_empty() {
            return Err(AdapterError::ParentMismatch);
        }
        let owner = match introspection.try_get_owner() {
            Ok(Ok(Some(value))) => value,
            _ => return Err(AdapterError::ParentUnavailable),
        };
        if owner != deployment_owner {
            return Err(AdapterError::ParentMismatch);
        }
        extend_instance_ttl(&env);
        env.storage().instance().set(
            &CONFIG,
            &PoolOracleConfig {
                parent_oracle,
                deployment_owner,
                decimals,
                resolution,
                base,
            },
        );
        Ok(())
    }

    pub fn config(env: Env) -> PoolOracleConfig {
        load_config(&env)
    }

    pub fn interface_version() -> u32 {
        1
    }

    pub fn storage_version() -> u32 {
        1
    }

    pub fn extend_ttl(env: Env) {
        extend_instance_ttl(&env);
    }
}

/// Latest inherits parent freshness; history inherits only parent history gates.
/// Historical reads never consult latest or synthesize adapter freshness policy.
#[contractimpl]
impl PriceFeedTrait for PoolOracleAdapter {
    fn base(env: Env) -> Asset {
        load_config(&env).base
    }

    /// Exact parent vector, including order; parent failure traps, never an empty fallback.
    fn assets(env: Env) -> Vec<Asset> {
        let config = load_config(&env);
        let parent = ParentIntrospectionClient::new(&env, &config.parent_oracle);
        match parent.try_registered_assets() {
            Ok(Ok(assets)) => assets,
            _ => env.panic_with_error(AdapterError::ParentUnavailable),
        }
    }

    fn decimals(env: Env) -> u32 {
        load_config(&env).decimals
    }

    fn resolution(env: Env) -> u32 {
        load_config(&env).resolution
    }

    fn price(env: Env, asset: Asset, timestamp: u64) -> Option<PriceData> {
        let config = active_config(&env)?;
        // Audited form: `%` traps on a zero resolution, `is_multiple_of` would not.
        #[allow(clippy::manual_is_multiple_of)]
        if timestamp % u64::from(config.resolution) != 0 {
            return None;
        }
        let parent = ProxyOracleClient::new(&env, &config.parent_oracle);
        let history = parent
            .try_aggregated_history(&asset, &MAX_HISTORY_RECORDS)
            .ok()?
            .ok()??;
        for entry in history.iter().rev() {
            if bucket_timestamp(entry.timestamp, config.resolution) == timestamp {
                return normalized_to_adapter(&entry, config.decimals, config.resolution).ok();
            }
        }
        None
    }

    fn prices(env: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>> {
        let config = active_config(&env)?;
        if records == 0 {
            return None;
        }
        let parent = ProxyOracleClient::new(&env, &config.parent_oracle);
        let history = parent
            .try_aggregated_history(&asset, &records)
            .ok()?
            .ok()??;
        let mut prices: Vec<PriceData> = Vec::new(&env);
        for entry in history.iter() {
            let price = normalized_to_adapter(&entry, config.decimals, config.resolution).ok()?;
            let previous_index = prices.len().saturating_sub(1);
            if prices
                .get(previous_index)
                .is_some_and(|previous| previous.timestamp == price.timestamp)
            {
                prices.set(previous_index, price);
            } else {
                prices.push_back(price);
            }
        }
        (!prices.is_empty()).then_some(prices)
    }

    fn lastprice(env: Env, asset: Asset) -> Option<PriceData> {
        let config = active_config(&env)?;
        let parent = ProxyOracleClient::new(&env, &config.parent_oracle);
        // DANGEROUS and UNAUDITED, see README: refresh the parent so the read below
        // serves a price pulled in this transaction. The outcome is ignored on purpose:
        // a failed or blocked refresh leaves a failed cache, so `aggregated_latest`
        // returns None.
        #[cfg(feature = "dangerous-eager-refresh")]
        let _ =
            parent::ParentMaintenanceClient::new(&env, &config.parent_oracle).try_refresh(&asset);
        let price = parent.try_aggregated_latest(&asset).ok()?.ok()??;
        // Guardian L-05: rounding down to the bucket start would hide a parent price
        // published after the current ledger from the pool's own future-dated check.
        if price.timestamp > env.ledger().timestamp() {
            return None;
        }
        normalized_to_adapter(&price, config.decimals, config.resolution).ok()
    }
}

// SEP-40 metadata cannot be optional: missing config is a fail-closed trap.
#[allow(clippy::expect_used)]
fn load_config(env: &Env) -> PoolOracleConfig {
    extend_instance_ttl(env);
    env.storage().instance().get(&CONFIG).expect("CONFIG")
}

fn active_config(env: &Env) -> Option<PoolOracleConfig> {
    let config = load_config(env);
    let parent = ProxyOracleClient::new(env, &config.parent_oracle);
    if !matches!(parent.try_source_base(), Ok(Ok(Some(base))) if base == config.base) {
        return None;
    }
    Some(config)
}

fn normalized_to_adapter(
    price: &NormalizedPrice,
    decimals: u32,
    resolution: u32,
) -> Result<PriceData, ContractError> {
    let mut projected = normalized_to_sep40(price, decimals)?;
    projected.timestamp = bucket_timestamp(projected.timestamp, resolution);
    Ok(projected)
}

fn bucket_timestamp(timestamp: u64, resolution: u32) -> u64 {
    timestamp - timestamp % u64::from(resolution)
}

#[cfg(test)]
mod tests;
