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

use crate::{OracleTripper, OracleTripperClient};

/// A failure raised by a governance contract call made from inside `trip`.
/// The tripper uses the non-try governance client, so the sub-contract error
/// aborts the whole invocation and surfaces unchanged as the host error.
fn gov_err(e: GovernanceError) -> Error {
    Error::from_contract_error(e as u32)
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
    /// The Hypernative caller key (wrong: governance sees the tripper as the
    /// governance caller, not this key).
    Caller,
}

struct Setup {
    env: Env,
    admin: Address,
    /// The stored Hypernative caller.
    caller: Address,
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
        let asset = Address::generate(&env);
        let base_usd = Asset::Other(Symbol::new(&env, "USD"));

        let runtime_id = env.register(SorobanProxyOracle, (&admin, &base_usd));
        let governance_id = env.register(ProxyOracleGovernance, (&admin, &runtime_id, 0_u64));
        let runtime = SorobanProxyOracleClient::new(&env, &runtime_id);
        let live_until_ledger = env.ledger().max_live_until_ledger();
        runtime.transfer_ownership(&governance_id, &live_until_ledger);
        runtime.accept_ownership();

        let tripper_id = env.register(OracleTripper, (&governance_id, &caller));

        let s = Self {
            env,
            admin,
            caller,
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
    // address that calls it, which is the tripper, not the Hypernative key.
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
fn tripping_an_already_tripped_asset_succeeds_with_a_new_id() {
    // Proven: a second `trip` on a tripped asset returns Ok with a fresh,
    // consecutive proposal id, leaves nothing active, and the asset stays
    // tripped.
    let s = Setup::new(true);

    s.mock_trip(&s.caller);
    let first = s.tripper().trip(&s.asset);
    assert!(s.is_tripped());

    s.mock_trip(&s.caller);
    let second = s.tripper().trip(&s.asset);

    assert_eq!(second, first + 1);
    assert!(s.is_tripped());
    assert_eq!(s.governance().next_proposal_id(), second + 1);
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
    env.register(OracleTripper, (&not_governance, &caller));
}

#[test]
fn getters_return_constructor_values() {
    let s = Setup::new(true);
    assert_eq!(s.tripper().governance(), s.governance_id);
    assert_eq!(s.tripper().caller(), s.caller);
}
