#![no_std]
//! One-call manual trip of a single asset's price feed on Templar's
//! proxy-oracle runtime.
//!
//! A Hypernative "Single Contract Call" action has a fixed function and fixed
//! arguments, but tripping an asset through governance takes two calls
//! (`create_proposal` with the exact next id, then `execute_proposal`). This
//! contract does both in one transaction: `trip(asset)` reads the governance
//! `next_proposal_id`, creates a `SetManualTrip(Asset::Stellar(asset), true,
//! None)` proposal and executes it immediately.
//!
//! # Trust model
//! - Only the configured `caller` (the Hypernative-held key) can call `trip`.
//! - The only thing it can do is trip an asset. Untripping and every other
//!   governance action stay with the governance Admin / ManualTripper humans.
//! - There is no admin, setter or upgrade path. Rotation means redeploying
//!   this contract and re-granting the role.
//! - Governance must grant the `ManualTripper` role to THIS contract's
//!   address (`SetRole(tripper, ManualTripper, true)`): the contract passes
//!   its own address as the governance `caller`, so Soroban authorizes the
//!   calls as coming from this contract.
//! - Governance's `SetManualTrip` action TTL must stay 0: with TTL 0 the
//!   proposal is mature immediately, so create + execute fit in one
//!   transaction. If the TTL is raised, the governance calls reject and the
//!   whole `trip` invocation fails atomically.
//! - A compromised `caller` can trip every Stellar-keyed feed, repeatedly;
//!   there is no rate limit. Recovery is the governance Admin untripping each
//!   asset.
//! - Only `Asset::Stellar` feeds are reachable (all three mainnet feeds
//!   today).
//! - Pinned to `rev eea0060d (governance v0.1.5, runtime v0.1.4)`.
use soroban_sdk::{contract, contractclient, contractimpl, contracttype, Address, Env};
use templar_proxy_oracle_soroban_common::{Asset, DEFAULT_TTL_EXTEND_TO, DEFAULT_TTL_THRESHOLD};
use templar_proxy_oracle_soroban_governance_common::{GovernanceAction, GovernanceError, Proposal};

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Governance,
    Caller,
}

/// Subset of the governance contract used by the tripper, pinned to
/// `rev eea0060d (governance v0.1.5, runtime v0.1.4)`. Local
/// client on purpose: depending on the governance contract crate outside
/// dev-deps would link its exports into our wasm.
#[contractclient(name = "GovernanceClient")]
pub trait Governance {
    fn next_proposal_id(env: Env) -> Result<u64, GovernanceError>;
    fn create_proposal(
        env: Env,
        caller: Address,
        id: u64,
        operation: GovernanceAction,
        requested_ttl: u64,
    ) -> Result<Proposal, GovernanceError>;
    fn execute_proposal(env: Env, caller: Address, id: u64) -> Result<(), GovernanceError>;
}

#[contract]
pub struct OracleTripper;

#[contractimpl]
impl OracleTripper {
    pub fn __constructor(env: Env, governance: Address, caller: Address) {
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Caller, &caller);
        extend_self(&env);
        // Fail fast if `governance` is not a governance contract.
        GovernanceClient::new(&env, &governance).next_proposal_id();
    }

    /// Trips `asset` (as `Asset::Stellar(asset)`) on the governed runtime.
    /// Only `caller` may call. Returns the governance proposal id it created
    /// and executed. Any governance rejection fails the whole call
    /// atomically.
    pub fn trip(env: Env, asset: Address) -> u64 {
        extend_self(&env);
        let caller = stored_caller(&env);
        caller.require_auth();
        let governance = GovernanceClient::new(&env, &stored_governance(&env));
        let me = env.current_contract_address();
        let id = governance.next_proposal_id();
        governance.create_proposal(
            &me,
            &id,
            &GovernanceAction::SetManualTrip(Asset::Stellar(asset), true, None),
            &0,
        );
        governance.execute_proposal(&me, &id);
        id
    }

    pub fn governance(env: Env) -> Address {
        extend_self(&env);
        stored_governance(&env)
    }

    pub fn caller(env: Env) -> Address {
        extend_self(&env);
        stored_caller(&env)
    }

    /// Permissionless: renews this contract's instance AND code (keeper
    /// batcher `extend_ttl_contracts` convention).
    pub fn extend_ttl(env: Env) {
        extend_self(&env);
    }
}

fn extend_self(env: &Env) {
    env.deployer().extend_ttl(
        env.current_contract_address(),
        DEFAULT_TTL_THRESHOLD,
        DEFAULT_TTL_EXTEND_TO,
    );
}

fn stored_governance(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::Governance)
        .expect("not initialized")
}

fn stored_caller(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::Caller)
        .expect("not initialized")
}

#[cfg(test)]
mod test;
