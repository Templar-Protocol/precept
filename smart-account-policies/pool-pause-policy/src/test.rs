//! Integration tests for [`PoolPausePolicy`] against the **deployed** mainnet
//! OpenZeppelin admin smart account and the deployed `simple_threshold` policy,
//! both registered from the bytes in `testdata/` (see its README).
//!
//! The fixtures are the real authorization stack: `__check_auth` runs from the
//! fetched account wasm, the threshold check from the fetched policy wasm, and
//! only the policy under test is compiled from this crate's source.
//!
//! Fixture shape (mirrors the precept admin account):
//! - rule 0 — `Default`, three approver contracts, `simple_threshold` with
//!   threshold 2. Created by the account's constructor, like the deployed one.
//! - rule 1 — `CallContract(pool)`, the pauser contract, `simple_threshold`
//!   with threshold 1 plus this policy, `valid_until = VALID_UNTIL`.
//!
//! Error codes are load-bearing in these tests: `#1` is this policy
//! ([`PauseError::NotAllowed`][crate::PauseError]), `#3202` is
//! `SimpleThresholdError::NotAllowed`, and `#3002`/`#3016` are
//! `SmartAccountError::{UnvalidatedContext, UnauthorizedSigner}`. Asserting
//! `#1` therefore proves the refusal came from this policy and not from a
//! neighbour.
//!
//! A `Signer::Delegated` is authenticated by the host through
//! `require_auth_for_args((AuthDigestPreimage,))`, so every authorized call
//! needs two authorization entries: the account's (carrying the `AuthPayload`)
//! and one per delegated signer (root invocation `__check_auth` on the account,
//! argument the preimage). [`Fixture::arm`] builds those by hand, the way an
//! off-chain client does.
extern crate std;

use sha2::{Digest, Sha256};
use soroban_sdk::{
    auth::{Context, CustomAccountInterface},
    contract, contractimpl,
    crypto::Hash,
    events::Event as _,
    testutils::{
        storage::Persistent as _, // for `get_ttl` on persistent storage
        ContractEvents,
        Events,
        Ledger,
        Logs,
    },
    xdr::{
        Hash as XdrHash, HashIdPreimage, HashIdPreimageSorobanAuthorization, InvokeContractArgs,
        Limits, ScVal, SorobanAddressCredentials, SorobanAuthorizationEntry,
        SorobanAuthorizedFunction, SorobanAuthorizedInvocation, SorobanCredentials, VecM, WriteXdr,
    },
    Address, Bytes, BytesN, Env, IntoVal, Map, String, TryFromVal, Val, Vec,
};
use std::cell::{Cell, RefCell};
use stellar_accounts::{
    policies::simple_threshold::SimpleThresholdAccountParams,
    smart_account::{
        add_context_rule, get_context_rule, get_context_rules_count, AuthDigestPreimage,
        AuthPayload, ContextRule, ContextRuleAdded, ContextRuleType, Signer, SmartAccountError,
        SmartAccountStorageKey,
    },
};

use crate::PoolPausePolicy;

const ACCOUNT_WASM: &[u8] = include_bytes!("../testdata/admin_smart_account.wasm");
const THRESHOLD_WASM: &[u8] = include_bytes!("../testdata/simple_threshold_policy.wasm");

/// Pinned hashes of the fetched fixtures; see `testdata/README.md`.
const ACCOUNT_WASM_SHA256: &str =
    "c514d35ea67eaea9bfc1fb3327854c0c06111bc1ddebd4fe217c504c1004b9e1";
const THRESHOLD_WASM_SHA256: &str =
    "f72a11de6e2d5d2495d98fe61e04cd13544e64156a5a3be83ef8c2e403d14a94";

/// Rule 0's threshold; the deployed admin account is 2-of-3.
const THRESHOLD: u32 = 2;
/// Business expiry of the pause rule, in ledgers from the start of a test.
/// Must stay above [`RENT_JUMP`] so the rent test moves the ledger while the
/// rule is still time-valid, and below it only [`an_expired_pause_rule_refuses_the_pause_call`]
/// walks past it.
const VALID_UNTIL: u32 = 100_000;
/// The network minimum persistent-entry TTL every fixture data entry gets,
/// kept below [`VALID_UNTIL`] so the rent test can lapse the entries while
/// the rule stays time-valid. Reading a lapsed entry relies on the test
/// host's automatic restore (protocol 23+), which the rent test pins.
const MIN_DATA_TTL: u32 = 50_000;
/// How far past every data entry's live-until the rent test walks.
const RENT_JUMP: u32 = MIN_DATA_TTL + 2;
/// Instance/code entries are pinned so that only data entries can lapse.
const PINNED_TTL: u32 = 1_000_000;
/// Authorization entries outlive every call the tests make.
const AUTH_VALID_FOR: u32 = 1_000;

/// Pool storage keys, shared with the fixture's seeding helper.
const ADMIN_KEY: u32 = 1;
const STATUS_KEY: u32 = 2;

/// On-Ice: the only status this policy authorizes.
const ON_ICE: u32 = 2;
/// The rule [`Fixture::add_pause_rule`] creates.
const PAUSE_RULE: u32 = 1;

/// This policy refuses.
const NOT_ALLOWED: &str = "#1";
/// The threshold policy refuses.
const THRESHOLD_DENIED: &str = "#3202";
/// The account finds no rule for the context.
const UNVALIDATED: &str = "#3002";
/// The account finds the signer in no selected rule.
const UNAUTHORIZED: &str = "#3016";

/// Stand-in for the Precept XLM/USDC pool: `set_status` is authorized by the
/// pool's stored admin (the smart account), never by the caller, like the real
/// pool. `other` is a second admin-gated function on the same pool that writes
/// nothing, so refusing it needs no storage write.
#[contract]
struct FakePool;

#[contractimpl]
impl FakePool {
    pub fn __constructor(e: Env, admin: Address) {
        e.storage().persistent().set(&ADMIN_KEY, &admin);
    }

    pub fn set_status(e: Env, status: u32) {
        let admin: Address = e.storage().persistent().get(&ADMIN_KEY).unwrap();
        admin.require_auth();
        e.storage().persistent().set(&STATUS_KEY, &status);
    }

    pub fn other(e: Env, _status: u32) {
        let admin: Address = e.storage().persistent().get(&ADMIN_KEY).unwrap();
        admin.require_auth();
    }

    pub fn get_status(e: Env) -> u32 {
        e.storage().persistent().get(&STATUS_KEY).unwrap_or(0)
    }
}

/// Stand-in for a delegated signer, modeled on OpenZeppelin's
/// `ApprovingAccount` test helper: anything asked of it is approved. Signers
/// are identified by address, so each one is registered separately.
#[contract]
struct Delegated;

#[contractimpl]
impl CustomAccountInterface for Delegated {
    type Error = SmartAccountError;
    type Signature = Val;

    fn __check_auth(
        _e: Env,
        _signature_payload: Hash<32>,
        _signature: Val,
        _auth_contexts: Vec<Context>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn bundled_wasm_matches_pinned_sha256() {
    assert_eq!(sha256(ACCOUNT_WASM), ACCOUNT_WASM_SHA256);
    assert_eq!(sha256(THRESHOLD_WASM), THRESHOLD_WASM_SHA256);
}

fn sha256(bytes: &[u8]) -> std::string::String {
    let digest = Sha256::digest(bytes);
    let mut s = std::string::String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        use core::fmt::Write as _;
        write!(&mut s, "{b:02x}").unwrap();
    }
    s
}

// ################## FIXTURE ##################

struct Fixture {
    e: Env,
    account: Address,
    pool: Address,
    other_pool: Address,
    pauser: Address,
    approvers: std::vec::Vec<Address>,
    threshold_policy: Address,
    pause_policy: Address,
    nonce: Cell<i64>,
    /// The contract events published by the most recent [`Fixture::add_rule`].
    /// `e.events().all()` only ever returns the last contract invocation's
    /// events, so they have to be captured inside the `as_contract` frame
    /// that publishes them, before any later call overwrites the snapshot.
    rule_events: RefCell<Option<ContractEvents>>,
}

impl Fixture {
    /// Registers the fixtures. Rule 0 comes from the account's constructor; the
    /// pause rule is created by [`Fixture::add_pause_rule`] so a test can watch
    /// the installation happen.
    fn new() -> Self {
        let e = Env::default();
        let mut info = e.ledger().get();
        info.min_persistent_entry_ttl = MIN_DATA_TTL;
        e.ledger().set(info);

        let pauser = e.register(Delegated, ());
        let approvers = std::vec![
            e.register(Delegated, ()),
            e.register(Delegated, ()),
            e.register(Delegated, ()),
        ];
        let threshold_policy = e.register(THRESHOLD_WASM, ());
        let pause_policy = e.register(PoolPausePolicy, ());

        let signers = Vec::from_iter(&e, approvers.iter().cloned().map(Signer::Delegated));
        let mut policies: Map<Address, Val> = Map::new(&e);
        policies.set(
            threshold_policy.clone(),
            SimpleThresholdAccountParams {
                threshold: THRESHOLD,
            }
            .into_val(&e),
        );
        let account = e.register(ACCOUNT_WASM, (signers, policies));

        let pool = e.register(FakePool, (account.clone(),));
        let other_pool = e.register(FakePool, (account.clone(),));

        for addr in std::vec![
            &account,
            &pool,
            &other_pool,
            &pauser,
            &threshold_policy,
            &pause_policy
        ]
        .into_iter()
        .chain(approvers.iter())
        {
            e.deployer()
                .extend_ttl(addr.clone(), PINNED_TTL, PINNED_TTL);
        }

        Self {
            e,
            account,
            pool,
            other_pool,
            pauser,
            approvers,
            threshold_policy,
            pause_policy,
            nonce: Cell::new(1_000),
            rule_events: RefCell::new(None),
        }
    }

    /// Installs `simple_threshold` (1-of-1) plus the pause policy on a
    /// `CallContract(pool)` rule authorized by the pauser.
    fn add_pause_rule(&self) -> ContextRule {
        self.add_rule(
            ContextRuleType::CallContract(self.pool.clone()),
            "pause-xlm-usdc",
            Some(VALID_UNTIL),
        )
    }

    /// Same installation, on a `Default` rule: nothing to pin the call against.
    /// The name stays within `MAX_NAME_SIZE` (20 bytes) or `add_context_rule`
    /// refuses with `#3015` before the rule exists.
    fn add_default_pause_rule(&self) -> ContextRule {
        self.add_rule(ContextRuleType::Default, "default-pause", None)
    }

    fn add_rule(
        &self,
        context_type: ContextRuleType,
        name: &str,
        valid_until: Option<u32>,
    ) -> ContextRule {
        let e = &self.e;
        let mut policies: Map<Address, Val> = Map::new(e);
        policies.set(
            self.threshold_policy.clone(),
            SimpleThresholdAccountParams { threshold: 1 }.into_val(e),
        );
        policies.set(self.pause_policy.clone(), ().into_val(e));
        e.as_contract(&self.account, || {
            let rule = add_context_rule(
                e,
                &context_type,
                &String::from_str(e, name),
                valid_until,
                &Vec::from_iter(e, [Signer::Delegated(self.pauser.clone())]),
                &policies,
            );
            *self.rule_events.borrow_mut() = Some(e.events().all());
            rule
        })
    }

    fn pauser_signer(&self) -> Signer {
        Signer::Delegated(self.pauser.clone())
    }

    fn approver_signers(&self) -> Vec<Signer> {
        Vec::from_iter(
            &self.e,
            self.approvers.iter().cloned().map(Signer::Delegated),
        )
    }

    fn stranger_signer(&self) -> Signer {
        Signer::Delegated(self.e.register(Delegated, ()))
    }

    fn rule(&self, id: u32) -> ContextRule {
        let e = &self.e;
        e.as_contract(&self.account, || get_context_rule(e, id))
    }

    fn rule_count(&self) -> u32 {
        let e = &self.e;
        e.as_contract(&self.account, || get_context_rules_count(e))
    }

    /// Writes a status without going through authorization.
    fn seed_status(&self, pool: &Address, status: u32) {
        let e = &self.e;
        e.as_contract(pool, || e.storage().persistent().set(&STATUS_KEY, &status));
    }

    fn status(&self, pool: &Address) -> u32 {
        FakePoolClient::new(&self.e, pool).get_status()
    }

    /// `pool::set_status(status)`, expecting the host to accept it.
    fn set_status(&self, pool: &Address, status: u32, rule_ids: &[u32], signers: &[&Signer]) {
        self.arm(pool, "set_status", status, rule_ids, signers);
        FakePoolClient::new(&self.e, pool).set_status(&status);
    }

    /// `pool::set_status(status)`, returning a diagnostic string for the
    /// failure.
    fn try_set_status(
        &self,
        pool: &Address,
        status: u32,
        rule_ids: &[u32],
        signers: &[&Signer],
    ) -> std::string::String {
        self.try_call(pool, "set_status", status, rule_ids, signers)
    }

    /// `pool::other(status)`, returning a diagnostic string for the failure.
    fn try_other(
        &self,
        pool: &Address,
        status: u32,
        rule_ids: &[u32],
        signers: &[&Signer],
    ) -> std::string::String {
        self.try_call(pool, "other", status, rule_ids, signers)
    }

    /// Runs `pool::fn_name(status)` expecting failure, and returns the client
    /// result together with the diagnostic events recorded by this call only.
    ///
    /// The host deliberately hides the refusing `__check_auth`'s error from
    /// the client: a recoverable contract error is turned into an auth error
    /// and `try_call` narrows every non-contract host error to a single
    /// `Error(Context, InvalidAction)`. The refusing component's real
    /// `Error(Contract, #N)` survives in the `"VM call trapped with
    /// HostError"` diagnostic the test host records, so that is what the
    /// error-code assertions match against. Slicing to this call's events
    /// means a refusal from an earlier call can never satisfy the assertion.
    fn try_call(
        &self,
        pool: &Address,
        fn_name: &str,
        status: u32,
        rule_ids: &[u32],
        signers: &[&Signer],
    ) -> std::string::String {
        let e = &self.e;
        let before = e.logs().all().len();
        self.arm(pool, fn_name, status, rule_ids, signers);
        let client = FakePoolClient::new(e, pool);
        let result = match fn_name {
            "set_status" => client.try_set_status(&status),
            _ => client.try_other(&status),
        };
        let logs = e.logs().all();
        std::format!("{result:?}\n{}", logs[before..].join("\n"))
    }

    /// Builds and installs the authorization entries an off-chain client sends
    /// for `pool::fn_name(status)` authorized by the account under `rule_ids`:
    /// the account's entry carrying the `AuthPayload`, plus one nested entry per
    /// delegated signer authorizing the digest preimage.
    fn arm(
        &self,
        pool: &Address,
        fn_name: &str,
        status: u32,
        rule_ids: &[u32],
        signers: &[&Signer],
    ) {
        let e = &self.e;
        let root = contract_fn(pool, fn_name, std::vec![to_scval(e, status)]);
        let expiration = e.ledger().sequence() + AUTH_VALID_FOR;
        let account_nonce = self.next_nonce();

        let mut claimed = Map::new(e);
        for s in signers {
            claimed.set((*s).clone(), Bytes::new(e));
        }
        let mut entries = std::vec![auth_entry(
            &self.account,
            account_nonce,
            expiration,
            to_scval(
                e,
                AuthPayload {
                    signers: claimed,
                    context_rule_ids: rule_ids_vec(e, rule_ids),
                }
            ),
            root.clone(),
        )];

        let preimage = AuthDigestPreimage {
            account: self.account.clone(),
            signature_payload: signature_payload(e, account_nonce, expiration, &root),
            context_rule_ids: rule_ids_vec(e, rule_ids),
        };
        for s in signers {
            if let Signer::Delegated(addr) = s {
                entries.push(auth_entry(
                    addr,
                    self.next_nonce(),
                    expiration,
                    ScVal::Void,
                    contract_fn(
                        &self.account,
                        "__check_auth",
                        std::vec![to_scval(e, preimage.clone())],
                    ),
                ));
            }
        }
        e.set_auths(entries.as_slice());
    }

    fn next_nonce(&self) -> i64 {
        let n = self.nonce.get();
        self.nonce.set(n + 1);
        n
    }

    /// Moves the ledger past every data entry's live-until.
    fn expire_all_data(&self) {
        self.e.ledger().set_sequence_number(RENT_JUMP);
    }

    /// Remaining ledgers on one of the account's persistent entries. Must run
    /// inside the account's frame; storage has no context otherwise.
    fn data_ttl(&self, key: &SmartAccountStorageKey) -> u32 {
        let e = &self.e;
        e.as_contract(&self.account, || e.storage().persistent().get_ttl(key))
    }
}

fn rule_ids_vec(e: &Env, ids: &[u32]) -> Vec<u32> {
    Vec::from_iter(e, ids.iter().copied())
}

fn contract_fn(
    contract: &Address,
    name: &str,
    args: std::vec::Vec<ScVal>,
) -> SorobanAuthorizedInvocation {
    SorobanAuthorizedInvocation {
        function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
            contract_address: contract.into(),
            function_name: name.try_into().unwrap(),
            args: args.try_into().unwrap(),
        }),
        sub_invocations: VecM::default(),
    }
}

/// The hash the host hands to `__check_auth` for an entry with this nonce,
/// expiration and root invocation. Delegated signers commit to it through
/// [`AuthDigestPreimage`], so the client has to compute it the same way.
fn signature_payload(
    e: &Env,
    nonce: i64,
    expiration: u32,
    invocation: &SorobanAuthorizedInvocation,
) -> BytesN<32> {
    let preimage = HashIdPreimage::SorobanAuthorization(HashIdPreimageSorobanAuthorization {
        network_id: XdrHash(e.ledger().get().network_id),
        nonce,
        signature_expiration_ledger: expiration,
        invocation: invocation.clone(),
    });
    let xdr = preimage.to_xdr(Limits::none()).unwrap();
    e.crypto().sha256(&Bytes::from_slice(e, &xdr)).to_bytes()
}

fn auth_entry(
    address: &Address,
    nonce: i64,
    expiration: u32,
    signature: ScVal,
    root_invocation: SorobanAuthorizedInvocation,
) -> SorobanAuthorizationEntry {
    SorobanAuthorizationEntry {
        credentials: SorobanCredentials::Address(SorobanAddressCredentials {
            address: address.into(),
            nonce,
            signature_expiration_ledger: expiration,
            signature,
        }),
        root_invocation,
    }
}

fn to_scval<T: IntoVal<Env, Val>>(e: &Env, value: T) -> ScVal {
    ScVal::try_from_val(e, &value.into_val(e)).unwrap()
}

fn assert_err(err: &std::string::String, code: &str) {
    assert!(
        err.contains(&std::format!("Error(Contract, {code})")),
        "expected {code} in: {err}"
    );
}

// ################## (a) rule 0 is untouched ##################

#[test]
fn adding_the_pause_rule_leaves_rule_zero_as_the_constructor_left_it() {
    let f = Fixture::new();
    let before = f.rule(0);
    f.add_pause_rule();

    let after = f.rule(0);
    assert_eq!(after, before);
    // Read back from the fetched account wasm, not from a local recompile.
    assert_eq!(after.name, String::from_str(&f.e, "multisig"));
    assert_eq!(after.context_type, ContextRuleType::Default);
    assert_eq!(after.valid_until, None);
    assert_eq!(
        after.signers,
        Vec::from_iter(&f.e, f.approvers.iter().cloned().map(Signer::Delegated))
    );
    assert_eq!(
        after.policies,
        Vec::from_iter(&f.e, [f.threshold_policy.clone()])
    );
    assert_eq!(
        f.rule(PAUSE_RULE).context_type,
        ContextRuleType::CallContract(f.pool.clone())
    );
    assert_eq!(f.rule_count(), 2);
}

// ################## (b) the rule is appended and announced ##################

#[test]
fn adding_the_pause_rule_appends_id_one_and_emits_context_rule_added() {
    let f = Fixture::new();
    let rule = f.add_pause_rule();

    assert_eq!(rule.id, PAUSE_RULE);
    assert_eq!(rule.name, String::from_str(&f.e, "pause-xlm-usdc"));
    assert_eq!(
        rule.context_type,
        ContextRuleType::CallContract(f.pool.clone())
    );
    assert_eq!(rule.valid_until, Some(VALID_UNTIL));
    assert_eq!(rule.signers, Vec::from_iter(&f.e, [f.pauser_signer()]));
    // The account keeps a rule's policies as `Map<Address, Val>` keys and
    // reads them back in host key order, not installation order, so the
    // assertion is exact membership: both policies, nothing else.
    assert_eq!(rule.policies.len(), 2);
    assert!(rule.policies.contains(&f.threshold_policy));
    assert!(rule.policies.contains(&f.pause_policy));
    assert_eq!(f.rule_count(), 2);

    let expected = ContextRuleAdded {
        context_rule_id: rule.id,
        name: rule.name.clone(),
        context_type: rule.context_type.clone(),
        valid_until: rule.valid_until,
        signer_ids: rule.signer_ids.clone(),
        policy_ids: rule.policy_ids.clone(),
    }
    .to_xdr(&f.e, &f.account);
    let events = f
        .rule_events
        .borrow()
        .clone()
        .expect("add_rule must have captured the account's events");
    assert!(
        events.events().contains(&expected),
        "no context_rule_added for rule {}: {events:?}",
        rule.id
    );
}

// ################## (c) exactly set_status(2) on that pool ##################

#[test]
fn the_pauser_can_set_the_pool_on_ice_from_any_status() {
    let f = Fixture::new();
    let rule = f.add_pause_rule();
    let pauser = f.pauser_signer();

    for from in [0u32, 1, 3, 5, 6] {
        f.seed_status(&f.pool, from);
        f.set_status(&f.pool, ON_ICE, &[rule.id], &[&pauser]);
        assert_eq!(f.status(&f.pool), ON_ICE);
    }
}

#[test]
fn the_pause_rule_refuses_any_other_status_or_function() {
    let f = Fixture::new();
    let rule = f.add_pause_rule();
    let pauser = f.pauser_signer();

    for status in [0u32, 1, 3, 4, ON_ICE + 1] {
        f.seed_status(&f.pool, 1);
        assert_err(
            &f.try_set_status(&f.pool, status, &[rule.id], &[&pauser]),
            NOT_ALLOWED,
        );
        assert_eq!(f.status(&f.pool), 1, "a refused call must not write");
    }

    // Same pool, different function: the context still matches the rule, so it
    // is this policy that refuses, not the account.
    f.seed_status(&f.pool, 1);
    assert_err(
        &f.try_other(&f.pool, ON_ICE, &[rule.id], &[&pauser]),
        NOT_ALLOWED,
    );
    assert_eq!(f.status(&f.pool), 1);
}

#[test]
fn the_pause_rule_refuses_the_same_call_on_another_pool() {
    let f = Fixture::new();
    let rule = f.add_pause_rule();
    let pauser = f.pauser_signer();

    // The context names a contract the rule does not cover, so the account
    // rejects it before any policy runs.
    assert_err(
        &f.try_set_status(&f.other_pool, ON_ICE, &[rule.id], &[&pauser]),
        UNVALIDATED,
    );
    assert_eq!(f.status(&f.other_pool), 0);
}

// ################## (d)(e) only the rule's own signers ##################

#[test]
fn a_stranger_signer_is_rejected_before_the_policy_runs() {
    let f = Fixture::new();
    let rule = f.add_pause_rule();
    let stranger = f.stranger_signer();

    assert_err(
        &f.try_set_status(&f.pool, ON_ICE, &[rule.id], &[&stranger]),
        UNAUTHORIZED,
    );
    assert_eq!(f.status(&f.pool), 0);
}

#[test]
fn the_pauser_cannot_authorize_under_rule_zero() {
    let f = Fixture::new();
    f.add_pause_rule();
    let pauser = f.pauser_signer();

    // The pauser signs for rule 1 only; naming rule 0 is rejected even though
    // rule 0 would authorize any call.
    assert_err(
        &f.try_set_status(&f.pool, ON_ICE, &[0], &[&pauser]),
        UNAUTHORIZED,
    );
    assert_eq!(f.status(&f.pool), 0);
}

// ################## (f) the rule's own expiry ##################

#[test]
fn an_expired_pause_rule_refuses_the_pause_call() {
    let f = Fixture::new();
    let rule = f.add_pause_rule();
    let pauser = f.pauser_signer();

    f.e.ledger().set_sequence_number(VALID_UNTIL + 1);
    assert_err(
        &f.try_set_status(&f.pool, ON_ICE, &[rule.id], &[&pauser]),
        UNVALIDATED,
    );
    assert_eq!(f.status(&f.pool), 0);
}

// ################## (g) rule 0 keeps its full authority ##################

#[test]
fn the_rule_zero_signers_are_not_restricted_by_the_pause_rule() {
    let f = Fixture::new();
    f.add_pause_rule();
    let approvers = f.approver_signers();
    let first = approvers.get_unchecked(0);
    let second = approvers.get_unchecked(1);

    // Anything the multisig could do before, it can still do: an un-pause on
    // this very pool, and pausing through rule 0 instead of rule 1.
    f.set_status(&f.pool, 0, &[0], &[&first, &second]);
    assert_eq!(f.status(&f.pool), 0);
    f.set_status(&f.pool, ON_ICE, &[0], &[&first, &second]);
    assert_eq!(f.status(&f.pool), ON_ICE);

    // One signer of a 2-of-3 is the threshold policy refusing, not this one.
    let third = approvers.get_unchecked(2);
    assert_err(
        &f.try_set_status(&f.pool, 3, &[0], &[&third]),
        THRESHOLD_DENIED,
    );
    assert_eq!(f.status(&f.pool), ON_ICE);
}

// ################## (h) the policy needs a CallContract rule ##################

#[test]
fn the_pause_policy_on_a_default_rule_refuses_even_the_pause_call() {
    let f = Fixture::new();
    let rule = f.add_default_pause_rule();
    let pauser = f.pauser_signer();

    // Fail closed: with no pinned pool there is nothing to compare the call
    // against, so even set_status(2) is refused.
    assert_err(
        &f.try_set_status(&f.pool, ON_ICE, &[rule.id], &[&pauser]),
        NOT_ALLOWED,
    );
    assert_eq!(f.status(&f.pool), 0);
}

// ################## (i) rent expiry ##################

/// Rent expiry must not change what the rule authorizes.
///
/// Production would need a restore operation for an archived entry; the test
/// host has restored them automatically since protocol 23, so what this proves
/// is the safety property: with the rule, the signer registry and the threshold
/// configuration all lapsed, the account still refuses what it refused before,
/// the refusal is still this policy's, and the rule reads back intact with its
/// `valid_until` preserved and its TTL live again.
#[test]
fn rent_expiry_does_not_change_what_the_pause_rule_authorizes() {
    let f = Fixture::new();
    let rule = f.add_pause_rule();
    let pauser = f.pauser_signer();
    let key = SmartAccountStorageKey::ContextRuleData(rule.id);

    assert!(f.data_ttl(&key) > 0);
    f.expire_all_data();

    assert_err(
        &f.try_other(&f.pool, ON_ICE, &[rule.id], &[&pauser]),
        NOT_ALLOWED,
    );

    let after = f.rule(rule.id);
    assert_eq!(after, rule);
    assert_eq!(after.valid_until, Some(VALID_UNTIL));
    assert!(f.data_ttl(&key) > 0);
    assert_err(
        &f.try_other(&f.pool, ON_ICE, &[rule.id], &[&pauser]),
        NOT_ALLOWED,
    );
}
