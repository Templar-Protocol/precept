//! Tests against the REAL proxy-oracle governance and runtime contracts
//! (pinned at the v0.1.5 governance release), registered in a soroban test Env.

use soroban_sdk::testutils::{
    Address as _, Deployer as _, Ledger as _, LedgerInfo, MockAuth, MockAuthInvoke,
};
use soroban_sdk::xdr::{ScErrorCode, ScErrorType};
use soroban_sdk::{contract, contractimpl, Address, Env, Error, IntoVal, Symbol, Vec};
use templar_proxy_oracle_soroban_common::{
    Asset, PriceData, ProxyConfig, ProxyOracleClient, SourceConfig, DEFAULT_TTL_EXTEND_TO,
    DEFAULT_TTL_THRESHOLD,
};
use templar_proxy_oracle_soroban_contract::{
    RefreshStatus, SorobanProxyOracle, SorobanProxyOracleClient,
};
use templar_proxy_oracle_soroban_governance_common::{
    GovernanceAction, GovernanceError, OperationKind, Role,
};
use templar_proxy_oracle_soroban_governance_contract::{
    ProxyOracleGovernance, ProxyOracleGovernanceClient,
};

use crate::{OracleTripper, OracleTripperClient, TripperError};

/// A failure raised by a governance contract call made from inside `trip`.
/// The tripper uses the non-try governance client, so the sub-contract error
/// aborts the whole invocation and surfaces unchanged as the host error.
fn gov_err(e: GovernanceError) -> Error {
    Error::from_contract_error(e as u32)
}

/// The tripper's own refusal of a second `trip` on a spent asset. Raised in
/// the tripper itself, so it surfaces directly as a contract error.
fn spent_err() -> Error {
    Error::from_contract_error(TripperError::Spent as u32)
}

/// Host error for a missing `require_auth` authorization. The host raises it
/// as Auth/InvalidAction, but the outer try_ boundary escalates the auth
/// failure of the whole invocation to a Context error.
fn auth_err() -> Error {
    Error::from_type_and_code(ScErrorType::Context, ScErrorCode::InvalidAction)
}

/// Minimal upstream feed: serves `decimals()` for `SetProxy` validation and
/// a fixed fresh price (the ledger's current timestamp) for `refresh`.
#[contract]
struct FakeFeed;

#[contractimpl]
impl FakeFeed {
    pub fn base(env: Env) -> Asset {
        Asset::Other(Symbol::new(&env, "USD"))
    }

    pub fn decimals(_env: Env) -> u32 {
        8
    }

    pub fn lastprice(env: Env, _asset: Asset) -> Option<PriceData> {
        Some(PriceData {
            price: 100_000_000,
            timestamp: env.ledger().timestamp(),
        })
    }
}

/// Who receives the governance `ManualTripper` role in `Setup`.
#[derive(Clone, Copy)]
enum RoleHolder {
    Nobody,
    /// The tripper contract address (the intended deployment).
    Tripper,
    /// The caller key (wrong: governance sees the tripper as the
    /// governance caller, not this key).
    Caller,
}

struct Setup {
    env: Env,
    admin: Address,
    /// The stored caller key (our Secret-Manager key, submitted by the sink).
    caller: Address,
    /// The tripper's OpenZeppelin owner (mainnet: the Admin smart account).
    owner: Address,
    asset: Address,
    runtime_id: Address,
    governance_id: Address,
    tripper_id: Address,
}

impl Setup {
    /// Deploys runtime (owned by governance), governance, one configured
    /// Stellar asset, and the tripper. `grant_role` controls whether the
    /// tripper contract address receives `ManualTripper`.
    ///
    /// Returns with `mock_all_auths` still enabled for admin-side setup;
    /// tests that exercise `trip` switch to precise auths via `mock_trip`.
    ///
    /// Deliberate shortcuts, none of which affect `trip`: the runtime's
    /// two-step `accept_ownership` runs under `mock_all_auths` (the governance
    /// contract cannot sign it in a test, but ownership is the only thing it
    /// establishes); every governance action TTL is 0 (the same value `trip`
    /// requires of `SetManualTrip`; other actions' TTL only delays setup); the
    /// admin is a plain generated address instead of the production 2-of-3
    /// smart account (governance only checks `require_auth` on it, and `trip`
    /// never involves the admin).
    fn new(grant_role: bool) -> Self {
        Self::with_role(if grant_role {
            RoleHolder::Tripper
        } else {
            RoleHolder::Nobody
        })
    }

    fn with_role(holder: RoleHolder) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set(LedgerInfo {
            timestamp: 100,
            protocol_version: 25,
            sequence_number: 100,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
            ..Default::default()
        });

        let admin = Address::generate(&env);
        let caller = Address::generate(&env);
        let owner = Address::generate(&env);
        let asset = Address::generate(&env);
        let base_usd = Asset::Other(Symbol::new(&env, "USD"));

        let runtime_id = env.register(SorobanProxyOracle, (&admin, &base_usd));
        let governance_id = env.register(ProxyOracleGovernance, (&admin, &runtime_id, 0_u64));
        let runtime = SorobanProxyOracleClient::new(&env, &runtime_id);
        let live_until_ledger = env.ledger().max_live_until_ledger();
        runtime.transfer_ownership(&governance_id, &live_until_ledger);
        runtime.accept_ownership();

        let tripper_id = env.register(OracleTripper, (&governance_id, &caller, &owner));

        let s = Self {
            env,
            admin,
            caller,
            owner,
            asset,
            runtime_id,
            governance_id,
            tripper_id,
        };

        // Configure a proxy for the asset (manual trip requires one to exist).
        let feed = s.env.register(FakeFeed, ());
        let mut sources = Vec::new(&s.env);
        sources.push_back(SourceConfig {
            oracle: feed,
            asset: s.stellar(),
        });
        s.admin_execute(GovernanceAction::SetProxy(
            s.stellar(),
            ProxyConfig {
                sources,
                min_sources: 1,
                max_age_secs: Some(300),
                max_clock_drift_secs: Some(60),
            },
        ));
        match holder {
            RoleHolder::Nobody => {}
            RoleHolder::Tripper => {
                s.admin_execute(GovernanceAction::SetRole(
                    s.tripper_id.clone(),
                    Role::ManualTripper,
                    true,
                ));
            }
            RoleHolder::Caller => {
                s.admin_execute(GovernanceAction::SetRole(
                    s.caller.clone(),
                    Role::ManualTripper,
                    true,
                ));
            }
        }
        s
    }

    fn stellar(&self) -> Asset {
        Asset::Stellar(self.asset.clone())
    }

    fn governance(&self) -> ProxyOracleGovernanceClient<'_> {
        ProxyOracleGovernanceClient::new(&self.env, &self.governance_id)
    }

    fn runtime(&self) -> SorobanProxyOracleClient<'_> {
        SorobanProxyOracleClient::new(&self.env, &self.runtime_id)
    }

    fn tripper(&self) -> OracleTripperClient<'_> {
        OracleTripperClient::new(&self.env, &self.tripper_id)
    }

    /// Admin create + execute (requires mock_all_auths to be active).
    fn admin_execute(&self, action: GovernanceAction) -> u64 {
        let id = self.governance().next_proposal_id();
        self.governance()
            .create_proposal(&self.admin, &id, &action, &0);
        self.governance().execute_proposal(&self.admin, &id);
        id
    }

    /// Admin create + cancel (requires mock_all_auths to be active).
    fn admin_create_and_cancel(&self, action: GovernanceAction) -> u64 {
        let id = self.governance().next_proposal_id();
        self.governance()
            .create_proposal(&self.admin, &id, &action, &0);
        self.governance().cancel_proposal(&self.admin, &id);
        id
    }

    /// Replaces all auth mocking with exactly one authorization: `who` signs
    /// the top-level `trip(asset)` invocation. Nothing else is mocked, so the
    /// tripper's own calls into governance rely on real contract-invoker auth.
    fn mock_trip(&self, who: &Address) {
        self.mock_trip_asset(who, &self.asset);
    }

    fn mock_trip_asset(&self, who: &Address, asset: &Address) {
        self.env.mock_auths(&[MockAuth {
            address: who,
            invoke: &MockAuthInvoke {
                contract: &self.tripper_id,
                fn_name: "trip",
                args: (asset.clone(),).into_val(&self.env),
                sub_invokes: &[],
            },
        }]);
    }

    /// Like `mock_trip`, for `set_caller(new)` signed by `who`.
    fn mock_set_caller(&self, who: &Address, new: &Address) {
        self.env.mock_auths(&[MockAuth {
            address: who,
            invoke: &MockAuthInvoke {
                contract: &self.tripper_id,
                fn_name: "set_caller",
                args: (new.clone(),).into_val(&self.env),
                sub_invokes: &[],
            },
        }]);
    }

    /// Like `mock_trip`, for `rearm(asset)` signed by `who`.
    fn mock_rearm(&self, who: &Address, asset: &Address) {
        self.env.mock_auths(&[MockAuth {
            address: who,
            invoke: &MockAuthInvoke {
                contract: &self.tripper_id,
                fn_name: "rearm",
                args: (asset.clone(),).into_val(&self.env),
                sub_invokes: &[],
            },
        }]);
    }

    fn is_tripped(&self) -> bool {
        self.runtime()
            .get_breaker_set_view(&self.stellar())
            .unwrap()
            .is_manually_tripped
    }
}

#[test]
fn caller_trips_asset_in_one_call() {
    let s = Setup::new(true);
    assert!(!s.is_tripped());
    let expected_id = s.governance().next_proposal_id();

    s.mock_trip(&s.caller);
    let id = s.tripper().trip(&s.asset);

    assert_eq!(id, expected_id);
    assert!(s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), expected_id + 1);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn trip_uses_current_id_after_unrelated_proposals() {
    let s = Setup::new(true);
    // Move the id counter: one executed and two cancelled proposals.
    s.admin_execute(GovernanceAction::SetActionTtl(
        OperationKind::SetManualTrip,
        0,
    ));
    s.admin_create_and_cancel(GovernanceAction::SetActionTtl(
        OperationKind::SetManualTrip,
        0,
    ));
    s.admin_create_and_cancel(GovernanceAction::SetActionTtl(
        OperationKind::SetManualTrip,
        0,
    ));
    let expected_id = s.governance().next_proposal_id();
    assert!(s.governance().active_ids().is_empty());

    s.mock_trip(&s.caller);
    let id = s.tripper().trip(&s.asset);

    assert_eq!(id, expected_id);
    assert!(s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), expected_id + 1);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn other_address_cannot_trip() {
    let s = Setup::new(true);
    let id_before = s.governance().next_proposal_id();
    let stranger = Address::generate(&s.env);

    // Only the stranger authorizes; the stored caller does not.
    s.mock_trip(&stranger);
    assert_eq!(s.tripper().try_trip(&s.asset), Err(Ok(auth_err())));

    assert!(!s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), id_before);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn caller_without_any_auth_cannot_trip() {
    let s = Setup::new(true);
    let id_before = s.governance().next_proposal_id();

    s.env.set_auths(&[]);
    assert_eq!(s.tripper().try_trip(&s.asset), Err(Ok(auth_err())));

    assert!(!s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), id_before);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn trip_fails_without_manual_tripper_role() {
    let s = Setup::new(false);
    let id_before = s.governance().next_proposal_id();

    s.mock_trip(&s.caller);
    assert_eq!(
        s.tripper().try_trip(&s.asset),
        Err(Ok(gov_err(GovernanceError::Unauthorized)))
    );

    assert!(!s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), id_before);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn trip_fails_when_role_granted_to_the_caller_key() {
    // The role must sit on the tripper contract: governance authorizes the
    // address that calls it, which is the tripper, not the caller key.
    let s = Setup::with_role(RoleHolder::Caller);
    let id_before = s.governance().next_proposal_id();

    s.mock_trip(&s.caller);
    assert_eq!(
        s.tripper().try_trip(&s.asset),
        Err(Ok(gov_err(GovernanceError::Unauthorized)))
    );

    assert!(!s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), id_before);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn nonzero_set_manual_trip_ttl_fails_atomically() {
    let s = Setup::new(true);
    s.admin_execute(GovernanceAction::SetActionTtl(
        OperationKind::SetManualTrip,
        60_000_000_000,
    ));
    let id_before = s.governance().next_proposal_id();

    s.mock_trip(&s.caller);
    // create_proposal succeeds with a non-zero ttl, execute_proposal then
    // rejects the immature proposal; the whole call must roll back.
    assert_eq!(
        s.tripper().try_trip(&s.asset),
        Err(Ok(gov_err(GovernanceError::ProposalNotMature)))
    );

    assert!(s.governance().active_ids().is_empty());
    assert_eq!(s.governance().next_proposal_id(), id_before);
    assert!(!s.is_tripped());
}

#[test]
fn trip_fails_for_unconfigured_asset() {
    let s = Setup::new(true);
    let id_before = s.governance().next_proposal_id();
    let unconfigured = Address::generate(&s.env);

    // The runtime rejects SetManualTrip for an asset without a proxy
    // (ContractError::InvalidInput). Governance invokes it non-try, so the
    // runtime's contract error propagates as-is and the whole call rolls
    // back. That code (4) numerically collides with
    // GovernanceError::ProposalNotFound (4); the state assertions below are
    // what prove no proposal was created and no trip landed.
    s.mock_trip_asset(&s.caller, &unconfigured);
    assert_eq!(
        s.tripper().try_trip(&unconfigured),
        Err(Ok(Error::from_contract_error(
            templar_proxy_oracle_soroban_common::ContractError::InvalidInput as u32
        )))
    );

    assert!(s.governance().active_ids().is_empty());
    assert_eq!(s.governance().next_proposal_id(), id_before);
    assert!(!s.is_tripped());
}

#[test]
fn second_trip_of_same_asset_is_refused_until_rearm() {
    // The one-shot budget: a second `trip` on the same asset is refused with
    // `TripperError::Spent` and touches no governance state; the owner's
    // `rearm` restores it and the next trip gets a fresh, consecutive id.
    let s = Setup::new(true);
    assert!(s.tripper().is_armed(&s.asset));

    s.mock_trip(&s.caller);
    let first = s.tripper().trip(&s.asset);
    assert!(s.is_tripped());
    assert!(!s.tripper().is_armed(&s.asset));
    let id_after_first = s.governance().next_proposal_id();

    s.mock_trip(&s.caller);
    assert_eq!(s.tripper().try_trip(&s.asset), Err(Ok(spent_err())));
    assert_eq!(s.governance().next_proposal_id(), id_after_first);
    assert!(s.governance().active_ids().is_empty());
    assert!(s.is_tripped());

    s.mock_rearm(&s.owner, &s.asset);
    s.tripper().rearm(&s.asset);
    assert!(s.tripper().is_armed(&s.asset));

    s.mock_trip(&s.caller);
    let second = s.tripper().trip(&s.asset);
    assert_eq!(second, first + 1);
    assert!(s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), second + 1);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn budget_is_per_asset() {
    // Spending one asset's budget leaves every other asset armed.
    let s = Setup::new(true);
    let other = Address::generate(&s.env);
    let other_stellar = Asset::Stellar(other.clone());
    let mut sources = Vec::new(&s.env);
    sources.push_back(SourceConfig {
        oracle: s.env.register(FakeFeed, ()),
        asset: other_stellar.clone(),
    });
    s.admin_execute(GovernanceAction::SetProxy(
        other_stellar,
        ProxyConfig {
            sources,
            min_sources: 1,
            max_age_secs: Some(300),
            max_clock_drift_secs: Some(60),
        },
    ));

    s.mock_trip(&s.caller);
    s.tripper().trip(&s.asset);
    assert!(!s.tripper().is_armed(&s.asset));
    assert!(s.tripper().is_armed(&other));

    s.mock_trip_asset(&s.caller, &other);
    let id = s.tripper().trip(&other);
    assert!(!s.tripper().is_armed(&other));
    assert_eq!(s.governance().next_proposal_id(), id + 1);
}

#[test]
fn a_failed_governance_call_does_not_spend_the_budget() {
    // The spent mark is written before the governance calls, so a rejected
    // trip must revert it with the rest of the invocation: without the
    // ManualTripper role the trip fails (as `trip_fails_without_manual_tripper_role`
    // does) and the asset is still armed to trip later.
    let s = Setup::with_role(RoleHolder::Nobody);
    let id_before = s.governance().next_proposal_id();

    s.mock_trip(&s.caller);
    assert_eq!(
        s.tripper().try_trip(&s.asset),
        Err(Ok(gov_err(GovernanceError::Unauthorized)))
    );

    assert!(!s.is_tripped());
    assert!(s.tripper().is_armed(&s.asset));
    assert_eq!(s.governance().next_proposal_id(), id_before);
    assert!(s.governance().active_ids().is_empty());
}

#[test]
fn trip_blocks_the_oracle_price() {
    let s = Setup::new(true);
    let reader = ProxyOracleClient::new(&s.env, &s.runtime_id);

    // Before the trip the fake feed's price is accepted and served.
    assert!(matches!(
        s.runtime().refresh(&s.stellar()),
        RefreshStatus::Accepted(_)
    ));
    assert!(reader.aggregated_latest(&s.stellar()).is_some());

    s.mock_trip(&s.caller);
    s.tripper().trip(&s.asset);

    // The trip drops the cached price and refresh now reports Blocked even
    // though the feed still serves a valid price.
    assert!(reader.aggregated_latest(&s.stellar()).is_none());
    assert!(matches!(
        s.runtime().refresh(&s.stellar()),
        RefreshStatus::Blocked(_)
    ));
    assert!(reader.aggregated_latest(&s.stellar()).is_none());
}

#[test]
fn extend_ttl_needs_no_auth() {
    let s = Setup::new(true);
    let deployer = s.env.deployer();

    // Age the contract until both TTLs are under the renewal threshold but
    // still live.
    s.env.ledger().set_sequence_number(100 + 2_700_000);
    let instance_before = deployer.get_contract_instance_ttl(&s.tripper_id);
    let code_before = deployer.get_contract_code_ttl(&s.tripper_id);
    assert!(instance_before > 0 && instance_before < DEFAULT_TTL_THRESHOLD);
    assert!(code_before > 0 && code_before < DEFAULT_TTL_THRESHOLD);

    s.env.set_auths(&[]);
    s.tripper().extend_ttl();

    let instance_after = deployer.get_contract_instance_ttl(&s.tripper_id);
    let code_after = deployer.get_contract_code_ttl(&s.tripper_id);
    assert!(instance_after > instance_before);
    assert!(code_after > code_before);
    assert_eq!(instance_after, DEFAULT_TTL_EXTEND_TO);
    assert_eq!(code_after, DEFAULT_TTL_EXTEND_TO);
}

#[test]
#[should_panic(expected = "next_proposal_id")]
fn constructor_rejects_a_non_governance_address() {
    let env = Env::default();
    let not_governance = Address::generate(&env);
    let caller = Address::generate(&env);
    let owner = Address::generate(&env);
    env.register(OracleTripper, (&not_governance, &caller, &owner));
}

#[test]
fn getters_return_constructor_values() {
    let s = Setup::new(true);
    assert_eq!(s.tripper().governance(), s.governance_id);
    assert_eq!(s.tripper().caller(), s.caller);
    assert_eq!(s.tripper().get_owner(), Some(s.owner.clone()));
}

#[test]
fn owner_rotates_the_caller_without_a_new_role_grant() {
    let s = Setup::new(true);
    let new_caller = Address::generate(&s.env);

    s.mock_set_caller(&s.owner, &new_caller);
    s.tripper().set_caller(&new_caller);
    assert_eq!(s.tripper().caller(), new_caller);

    // The old key lost the power in the same call.
    s.mock_trip(&s.caller);
    assert_eq!(s.tripper().try_trip(&s.asset), Err(Ok(auth_err())));
    assert!(!s.is_tripped());

    // The new key trips through the role the contract already holds.
    s.mock_trip(&new_caller);
    s.tripper().trip(&s.asset);
    assert!(s.is_tripped());
}

#[test]
fn only_the_owner_can_set_the_caller() {
    let s = Setup::new(true);
    let new_caller = Address::generate(&s.env);
    let stranger = Address::generate(&s.env);

    // Neither the current caller, the governance admin nor a stranger.
    for who in [&s.caller, &s.admin, &stranger] {
        s.mock_set_caller(who, &new_caller);
        assert_eq!(s.tripper().try_set_caller(&new_caller), Err(Ok(auth_err())));
    }
    assert_eq!(s.tripper().caller(), s.caller);
}

#[test]
fn only_the_owner_can_rearm() {
    let s = Setup::new(true);
    let stranger = Address::generate(&s.env);
    s.mock_trip(&s.caller);
    s.tripper().trip(&s.asset);

    // Neither the current caller, the governance admin nor a stranger.
    for who in [&s.caller, &s.admin, &stranger] {
        s.mock_rearm(who, &s.asset);
        assert_eq!(s.tripper().try_rearm(&s.asset), Err(Ok(auth_err())));
    }
    assert!(!s.tripper().is_armed(&s.asset));
}
