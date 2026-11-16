#![no_std]
// Soroban contract entry points take `env: Env` and arguments by value.
#![allow(clippy::needless_pass_by_value)]
//! UNAUDITED: no auditor has reviewed this contract.
//!
//! LOSSY SEP-40 passthrough that serves another SEP-40 oracle's
//! prices at 7 decimals, the precision a Precept pool requires. Every read is
//! forwarded live to the source; nothing is cached, so no keeper is involved.
//! See README.md before deploying.

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Map, Vec};
use templar_proxy_oracle_soroban_common::{Asset, PriceData, PriceFeedClient, PriceFeedTrait};

/// Output precision. A Precept pool traps on any other value.
pub const DECIMALS: u32 = 7;

const DAY_LEDGERS: u32 = 17_280;
const TTL_THRESHOLD: u32 = 30 * DAY_LEDGERS;
const TTL_EXTEND_TO: u32 = 60 * DAY_LEDGERS;

#[contracttype]
pub struct Config {
    /// The SEP-40 oracle every read is forwarded to.
    pub source: Address,
    /// Asset this adapter is asked for → key the source serves it under, e.g.
    /// `Stellar(<token>)` → `Other("earnUSDC_PPS")`. Unlisted assets pass as-is.
    pub aliases: Map<Asset, Asset>,
}

#[contracttype]
enum Key {
    Config,
}

/// Immutable: no admin, no upgrade. Repointing or re-aliasing means a new deploy.
#[contract]
pub struct Sep40RescaleAdapter;

#[contractimpl]
impl Sep40RescaleAdapter {
    pub fn __constructor(env: Env, source: Address, aliases: Map<Asset, Asset>) {
        env.storage()
            .instance()
            .set(&Key::Config, &Config { source, aliases });
        extend_ttl(&env);
    }

    pub fn config(env: Env) -> Config {
        load(&env)
    }

    /// Permissionless instance-TTL maintenance; reads also extend it.
    pub fn extend_ttl(env: Env) {
        extend_ttl(&env);
    }
}

#[contractimpl]
impl PriceFeedTrait for Sep40RescaleAdapter {
    fn base(env: Env) -> Asset {
        source(&env).base()
    }

    /// The source's own keys, not the aliases.
    fn assets(env: Env) -> Vec<Asset> {
        source(&env).assets()
    }

    fn decimals(_env: Env) -> u32 {
        DECIMALS
    }

    fn resolution(env: Env) -> u32 {
        source(&env).resolution()
    }

    fn price(env: Env, asset: Asset, timestamp: u64) -> Option<PriceData> {
        let (source, asset) = route(&env, asset);
        let decimals = source.try_decimals().ok()?.ok()?;
        let price = source.try_price(&asset, &timestamp).ok()?.ok()??;
        rescale(price, decimals)
    }

    /// All or nothing: one unrepresentable record fails the whole read, and an
    /// empty history is `None`, as in `pool-oracle-adapter`.
    fn prices(env: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>> {
        let (source, asset) = route(&env, asset);
        let decimals = source.try_decimals().ok()?.ok()?;
        let history = source.try_prices(&asset, &records).ok()?.ok()??;
        let mut out = Vec::new(&env);
        for price in history.iter() {
            out.push_back(rescale(price, decimals)?);
        }
        (!out.is_empty()).then_some(out)
    }

    fn lastprice(env: Env, asset: Asset) -> Option<PriceData> {
        let (source, asset) = route(&env, asset);
        // Read live on every call: the source may change its decimals.
        let decimals = source.try_decimals().ok()?.ok()?;
        let price = source.try_lastprice(&asset).ok()?.ok()??;
        rescale(price, decimals)
    }
}

/// LOSSY. Scaling down truncates toward zero, so a positive price loses up to
/// one unit of the 7th decimal. Scaling up is exact. Overflow, or a result that
/// is not positive, is no price at all.
fn rescale(price: PriceData, from_decimals: u32) -> Option<PriceData> {
    let scaled = if from_decimals >= DECIMALS {
        price.price / 10_i128.checked_pow(from_decimals - DECIMALS)?
    } else {
        price
            .price
            .checked_mul(10_i128.checked_pow(DECIMALS - from_decimals)?)?
    };
    (scaled > 0).then_some(PriceData {
        price: scaled,
        timestamp: price.timestamp,
    })
}

fn load(env: &Env) -> Config {
    extend_ttl(env);
    // Set by the constructor, which always runs at deploy.
    env.storage().instance().get(&Key::Config).unwrap()
}

fn source(env: &Env) -> PriceFeedClient<'_> {
    PriceFeedClient::new(env, &load(env).source)
}

fn route(env: &Env, asset: Asset) -> (PriceFeedClient<'_>, Asset) {
    let config = load(env);
    let asset = config.aliases.get(asset.clone()).unwrap_or(asset);
    (PriceFeedClient::new(env, &config.source), asset)
}

fn extend_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

#[cfg(test)]
mod tests;
