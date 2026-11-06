#![no_std]
//! One-call manual trip of a single asset's price feed on Templar's
//! proxy-oracle runtime.
//!
//! Tripping an asset through governance takes two calls (`create_proposal`
//! with the exact next id, then `execute_proposal`). This contract does both
//! in one transaction: `trip(asset)` reads the governance `next_proposal_id`,
//! creates a `SetManualTrip(Asset::Stellar(asset), true, None)` proposal and
//! executes it immediately, and spends that asset's one-shot budget.
//!
//! # Trust model
//! - Only the configured `caller` can call `trip`. It is OUR key, held in our
//!   Secret Manager and submitted by the keeper's alert-sink route when a
//!   signed Hypernative alert arrives; it is not a Hypernative-held key and
//!   `trip` is not a Hypernative Single Contract Call.
//! - Each asset can be tripped once per owner re-arm: `trip` marks the asset
//!   spent before calling governance and refuses with `TripperError::Spent`
//!   while the mark stands. A governance rejection reverts the mark with the
//!   rest of the call.
//! - The only thing it can do is trip an asset. Untripping and every other
//!   governance action stay with the governance Admin / ManualTripper humans.
//! - The `owner` (OpenZeppelin `Ownable`, two-step transfer) can rotate the
//!   `caller` with `set_caller` and re-arm an asset's budget with `rearm`;
//!   nothing else. It cannot trip, untrip, change the governance address or
//!   upgrade. On mainnet it is the governance Admin's 2-of-3 smart account,
//!   so rotating the key or re-arming is one signing round and the role
//!   grant stays on this contract. Renouncing ownership freezes the caller
//!   and the spent marks for good.
//! - Governance must grant the `ManualTripper` role to THIS contract's
//!   address (`SetRole(tripper, ManualTripper, true)`): the contract passes
//!   its own address as the governance `caller`, so Soroban authorizes the
//!   calls as coming from this contract.
//! - Governance's `SetManualTrip` action TTL must stay 0: with TTL 0 the
//!   proposal is mature immediately, so create + execute fit in one
//!   transaction. If the TTL is raised, the governance calls reject and the
//!   whole `trip` invocation fails atomically.
//! - A compromised `caller` can trip each Stellar-keyed feed at most once
//!   until the owner re-arms it. Recovery is the governance Admin untripping
//!   each asset; the spent marks stay until the owner `rearm`s them.
//! - Only `Asset::Stellar` feeds are reachable (all three mainnet feeds
//!   today).
//! - Pinned to `rev eea0060d (governance v0.1.5, runtime v0.1.4)`.
use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, panic_with_error, Address,
    Env,
};
use stellar_access::ownable::{set_owner, Ownable};
use stellar_macros::only_owner;
use templar_proxy_oracle_soroban_common::{Asset, DEFAULT_TTL_EXTEND_TO, DEFAULT_TTL_THRESHOLD};
use templar_proxy_oracle_soroban_governance_common::{GovernanceAction, GovernanceError, Proposal};

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Governance,
    Caller,
    Spent(Address),
}

/// Errors raised by the tripper itself. 1001 stays clear of the
/// `GovernanceError` codes (1-11), which propagate unchanged through the
/// non-try governance client, and of the runtime's `ContractError`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TripperError {
    /// `trip` already fired for this asset; the owner must `rearm` it first.
    Spent = 1001,
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
    pub fn __constructor(env: Env, governance: Address, caller: Address, owner: Address) {
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Caller, &caller);
        set_owner(&env, &owner);
        extend_self(&env);
        // Fail fast if `governance` is not a governance contract.
        GovernanceClient::new(&env, &governance).next_proposal_id();
    }

    /// Trips `asset` (as `Asset::Stellar(asset)`) on the governed runtime.
    /// Only `caller` may call, and only once per asset until the owner
    /// `rearm`s it (`TripperError::Spent` otherwise). Returns the governance
    /// proposal id it created and executed. Any governance rejection fails
    /// the whole call atomically, including the spent mark.
    pub fn trip(env: Env, asset: Address) -> u64 {
        extend_self(&env);
        let caller = stored_caller(&env);
        caller.require_auth();
        let spent = DataKey::Spent(asset.clone());
        if env.storage().instance().has(&spent) {
            panic_with_error!(&env, TripperError::Spent);
        }
        env.storage().instance().set(&spent, &true);
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

    /// Owner-only: replaces the key allowed to call `trip`. The old key loses
    /// the power in the same call; the governance role stays on this contract.
    #[only_owner]
    pub fn set_caller(env: Env, caller: Address) {
        extend_self(&env);
        env.storage().instance().set(&DataKey::Caller, &caller);
    }

    /// Owner-only: removes `asset`'s spent mark so `trip` can fire for it once
    /// more. This is the tripper's own re-arm, unrelated to governance's
    /// circuit-breaker `Rearm` action. A no-op for an asset never tripped.
    #[only_owner]
    pub fn rearm(env: Env, asset: Address) {
        extend_self(&env);
        env.storage().instance().remove(&DataKey::Spent(asset));
    }

    /// True while `trip` can still fire for `asset`: it has not been tripped
    /// since deployment or the last `rearm` (so true for a never-tripped asset).
    pub fn is_armed(env: Env, asset: Address) -> bool {
        extend_self(&env);
        !env.storage().instance().has(&DataKey::Spent(asset))
    }

    /// Permissionless: renews this contract's instance AND code (keeper
    /// batcher `extend_ttl_contracts` convention).
    pub fn extend_ttl(env: Env) {
        extend_self(&env);
    }
}

/// OpenZeppelin two-step `transfer_ownership` / `accept_ownership`,
/// `renounce_ownership` and `get_owner`.
#[contractimpl(contracttrait)]
impl Ownable for OracleTripper {}

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
