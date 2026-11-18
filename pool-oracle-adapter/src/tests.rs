// Trait impls and the SEP-40 ABI take `env: Env` by value; the host requires it.
#![allow(clippy::needless_pass_by_value)]

extern crate std;

use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{storage::Instance as _, Address as _, Ledger},
    Address, Env, Symbol, Vec,
};
use templar_proxy_oracle_soroban_common::{
    Asset, ContractError, NormalizedPrice, PriceData, PriceFeedTrait, ProxyOracleTrait,
    DEFAULT_TTL_EXTEND_TO, DEFAULT_TTL_THRESHOLD,
};

use crate::{AdapterError, PoolOracleAdapter, PoolOracleAdapterClient, PoolOracleConfig};

fn environment() -> Env {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|ledger| {
        ledger.protocol_version = 25;
        ledger.timestamp = 100;
        ledger.sequence_number = 100;
        // +1: the host clamps an extension to `sequence + max_entry_ttl - 1`.
        ledger.max_entry_ttl = DEFAULT_TTL_EXTEND_TO + 1;
    });
    env
}

fn normalized(mantissa: i64, expo: i32, timestamp: u64) -> NormalizedPrice {
    NormalizedPrice {
        mantissa,
        expo,
        timestamp,
    }
}

fn projected(price: i128, timestamp: u64) -> PriceData {
    PriceData { price, timestamp }
}

mod mock_parent {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// Test-side description of the parent's knobs. Presence/absence of base and
    /// owner is expressed by storing or removing the key, never by an `Option`
    /// wire field (the user-defined `Asset` has no no-Env ScVal conversion).
    #[derive(Clone)]
    struct State {
        base: Option<Asset>,
        assets: Vec<Asset>,
        owner: Option<Address>,
        fail_base: bool,
        fail_assets: bool,
        fail_owner: bool,
        fail_latest: bool,
        fail_history: bool,
    }

    #[derive(Clone)]
    #[contracttype]
    enum Key {
        Base,
        Owner,
        Assets,
        FailBase,
        FailAssets,
        FailOwner,
        FailLatest,
        FailHistory,
        Latest(Asset),
        History(Asset),
    }

    #[contract]
    pub struct MockParent;

    fn fail(env: &Env, key: Key, error: ContractError) {
        if env
            .storage()
            .instance()
            .get::<_, bool>(&key)
            .unwrap_or(false)
        {
            soroban_sdk::panic_with_error!(env, error);
        }
    }

    fn assets(env: &Env) -> Vec<Asset> {
        env.storage().instance().get(&Key::Assets).unwrap()
    }

    #[contractimpl]
    impl MockParent {
        pub fn set_base(env: Env, base: Asset) {
            env.storage().instance().set(&Key::Base, &base);
        }

        pub fn clear_base(env: Env) {
            env.storage().instance().remove(&Key::Base);
        }

        pub fn set_owner(env: Env, owner: Address) {
            env.storage().instance().set(&Key::Owner, &owner);
        }

        pub fn clear_owner(env: Env) {
            env.storage().instance().remove(&Key::Owner);
        }

        pub fn set_assets(env: Env, assets: Vec<Asset>) {
            env.storage().instance().set(&Key::Assets, &assets);
        }

        pub fn set_fail(
            env: Env,
            base: bool,
            assets: bool,
            owner: bool,
            latest: bool,
            history: bool,
        ) {
            let storage = env.storage().instance();
            storage.set(&Key::FailBase, &base);
            storage.set(&Key::FailAssets, &assets);
            storage.set(&Key::FailOwner, &owner);
            storage.set(&Key::FailLatest, &latest);
            storage.set(&Key::FailHistory, &history);
        }

        pub fn set_latest(env: Env, asset: Asset, price: NormalizedPrice) {
            env.storage().persistent().set(&Key::Latest(asset), &price);
        }

        pub fn clear_latest(env: Env, asset: Asset) {
            env.storage().persistent().remove(&Key::Latest(asset));
        }

        pub fn set_history(env: Env, asset: Asset, history: Vec<NormalizedPrice>) {
            env.storage()
                .persistent()
                .set(&Key::History(asset), &history);
        }

        pub fn registered_assets(env: Env) -> Result<Vec<Asset>, ContractError> {
            fail(&env, Key::FailAssets, ContractError::StorageError);
            Ok(assets(&env))
        }

        pub fn get_owner(env: Env) -> Option<Address> {
            fail(&env, Key::FailOwner, ContractError::StorageError);
            env.storage().instance().get(&Key::Owner)
        }
    }

    #[contractimpl]
    impl ProxyOracleTrait for MockParent {
        fn source_base(env: Env) -> Option<Asset> {
            fail(&env, Key::FailBase, ContractError::StorageError);
            env.storage().instance().get(&Key::Base)
        }

        fn aggregated_latest(env: Env, asset: Asset) -> Option<NormalizedPrice> {
            fail(&env, Key::FailLatest, ContractError::SourceUnavailable);
            if !assets(&env).contains(&asset) {
                return None;
            }
            env.storage().persistent().get(&Key::Latest(asset))
        }

        fn aggregated_history(
            env: Env,
            asset: Asset,
            records: u32,
        ) -> Option<Vec<NormalizedPrice>> {
            fail(&env, Key::FailHistory, ContractError::SourceUnavailable);
            if records == 0 || !assets(&env).contains(&asset) {
                return None;
            }
            let history: Vec<NormalizedPrice> =
                env.storage().persistent().get(&Key::History(asset))?;
            Some(history.slice(history.len().saturating_sub(records)..))
        }
    }

    fn apply(parent: &MockParentClient<'static>, state: &State) {
        match &state.base {
            Some(base) => parent.set_base(base),
            None => parent.clear_base(),
        }
        match &state.owner {
            Some(owner) => parent.set_owner(owner),
            None => parent.clear_owner(),
        }
        parent.set_assets(&state.assets);
        parent.set_fail(
            &state.fail_base,
            &state.fail_assets,
            &state.fail_owner,
            &state.fail_latest,
            &state.fail_history,
        );
    }

    fn set_quotes(
        parent: &MockParentClient<'static>,
        asset: &Asset,
        latest: Option<NormalizedPrice>,
        history: Vec<NormalizedPrice>,
    ) {
        match latest {
            Some(price) => parent.set_latest(asset, &price),
            None => parent.clear_latest(asset),
        }
        parent.set_history(asset, &history);
    }

    fn parent(env: &Env) -> (MockParentClient<'static>, State, Asset, Asset) {
        let a = Asset::Stellar(Address::generate(env));
        let b = Asset::Stellar(Address::generate(env));
        let state = State {
            base: Some(Asset::Other(Symbol::new(env, "USD"))),
            assets: Vec::from_array(env, [b.clone(), a.clone()]),
            owner: Some(Address::generate(env)),
            fail_base: false,
            fail_assets: false,
            fail_owner: false,
            fail_latest: false,
            fail_history: false,
        };
        let id = env.register(MockParent, ());
        let client = MockParentClient::new(env, &id);
        apply(&client, &state);
        (client, state, a, b)
    }

    struct Fixture {
        env: Env,
        parent: MockParentClient<'static>,
        adapter: PoolOracleAdapterClient<'static>,
        state: State,
        config: PoolOracleConfig,
        a: Asset,
        b: Asset,
    }

    fn fixture(decimals: u32, resolution: u32) -> Fixture {
        let env = environment();
        // Mock quotes are stamped up to 110; keep them at or before ledger time so
        // `lastprice`'s future-dated guard (L-05) does not reject them.
        env.ledger().with_mut(|ledger| ledger.timestamp = 110);
        let (parent, state, a, b) = parent(&env);
        let config = PoolOracleConfig {
            parent_oracle: parent.address.clone(),
            deployment_owner: state.owner.clone().unwrap(),
            decimals,
            resolution,
            base: state.base.clone().unwrap(),
        };
        let id = env.register(
            PoolOracleAdapter,
            (
                &config.deployment_owner,
                &config.parent_oracle,
                &decimals,
                &resolution,
                &config.base,
            ),
        );
        let adapter = PoolOracleAdapterClient::new(&env, &id);
        Fixture {
            env,
            parent,
            adapter,
            state,
            config,
            a,
            b,
        }
    }

    #[contract]
    struct ConstructorContext;

    #[test]
    fn constructor_rejections_leave_config_unset_and_allow_valid_initialization() {
        for case in 0..11 {
            let env = environment();
            let (parent, valid, _, _) = parent(&env);
            let owner = valid.owner.clone().unwrap();
            let base = valid.base.clone().unwrap();
            let mut invalid = valid.clone();
            let mut decimals = 8;
            let mut resolution = 10;
            let expected = match case {
                0 => {
                    decimals = 19;
                    AdapterError::InvalidConfig
                }
                1 => {
                    resolution = 0;
                    AdapterError::InvalidConfig
                }
                2 => {
                    invalid.base = None;
                    AdapterError::ParentUnavailable
                }
                3 => {
                    invalid.fail_base = true;
                    AdapterError::ParentUnavailable
                }
                4 => {
                    invalid.base = Some(Asset::Other(Symbol::new(&env, "EUR")));
                    AdapterError::ParentMismatch
                }
                5 => {
                    invalid.assets = Vec::new(&env);
                    AdapterError::ParentMismatch
                }
                6 => {
                    invalid.fail_assets = true;
                    AdapterError::ParentUnavailable
                }
                7 => {
                    invalid.owner = None;
                    AdapterError::ParentUnavailable
                }
                8 => {
                    invalid.owner = Some(Address::generate(&env));
                    AdapterError::ParentMismatch
                }
                9 => {
                    invalid.fail_owner = true;
                    AdapterError::ParentUnavailable
                }
                10 => AdapterError::ParentUnavailable,
                _ => unreachable!(),
            };
            apply(&parent, &invalid);
            let parent_id = if case == 10 {
                Address::generate(&env)
            } else {
                parent.address.clone()
            };
            // The host reserves __constructor. Exercise its public Rust entry point
            // in an empty contract context, without inspecting any private storage key.
            let id = env.register(ConstructorContext, ());
            assert_eq!(
                env.as_contract(&id, || PoolOracleAdapter::__constructor(
                    env.clone(),
                    owner.clone(),
                    parent_id,
                    decimals,
                    resolution,
                    base.clone(),
                )),
                Err(expected),
                "constructor case {case}",
            );
            assert!(
                catch_unwind(AssertUnwindSafe(
                    || env.as_contract(&id, || { PoolOracleAdapter::config(env.clone()) })
                ))
                .is_err(),
                "rejected constructor case {case} stored config"
            );
            apply(&parent, &valid);
            assert_eq!(
                env.as_contract(&id, || PoolOracleAdapter::__constructor(
                    env.clone(),
                    owner.clone(),
                    parent.address.clone(),
                    8,
                    10,
                    base.clone(),
                )),
                Ok(())
            );
            assert_eq!(
                env.as_contract(&id, || PoolOracleAdapter::config(env.clone())),
                PoolOracleConfig {
                    parent_oracle: parent.address.clone(),
                    deployment_owner: owner,
                    decimals: 8,
                    resolution: 10,
                    base,
                }
            );
        }
    }

    #[test]
    fn deployment_metadata_is_exact_and_reinitialization_preserves_it() {
        let f = fixture(8, 10);
        assert_eq!(&f.adapter.config(), &f.config);
        assert_eq!(f.adapter.interface_version(), 1);
        assert_eq!(f.adapter.storage_version(), 1);
        assert_eq!(f.adapter.base(), f.config.base.clone());
        assert_eq!(f.adapter.decimals(), 8);
        assert_eq!(f.adapter.resolution(), 10);
        assert_eq!(
            f.env
                .as_contract(&f.adapter.address, || PoolOracleAdapter::__constructor(
                    f.env.clone(),
                    Address::generate(&f.env),
                    f.parent.address.clone(),
                    0,
                    1,
                    f.config.base.clone(),
                )),
            Err(AdapterError::AlreadyInitialized)
        );
        assert_eq!(f.adapter.config(), f.config);
    }

    #[test]
    fn one_adapter_prices_two_stellar_assets_and_tracks_registry_order_and_removal() {
        let mut f = fixture(2, 10);
        for (asset, value) in [(&f.a, 123), (&f.b, 456)] {
            let p = normalized(value, -1, 107);
            set_quotes(
                &f.parent,
                asset,
                Some(p.clone()),
                Vec::from_array(&f.env, [p]),
            );
            assert_eq!(
                f.adapter.lastprice(asset),
                Some(projected(i128::from(value) * 10, 100))
            );
            assert_eq!(f.adapter.price(asset, &100), f.adapter.lastprice(asset));
            assert_eq!(
                f.adapter.prices(asset, &1),
                Some(Vec::from_array(
                    &f.env,
                    [projected(i128::from(value) * 10, 100)]
                ))
            );
        }
        assert_eq!(
            f.adapter.assets(),
            Vec::from_array(&f.env, [f.b.clone(), f.a.clone()])
        );
        let c = Asset::Stellar(Address::generate(&f.env));
        assert_eq!(f.adapter.lastprice(&c), None);
        assert_eq!(f.adapter.price(&c, &100), None);
        assert_eq!(f.adapter.prices(&c, &2), None);
        f.state.assets = Vec::from_array(&f.env, [f.b.clone(), c.clone(), f.a.clone()]);
        apply(&f.parent, &f.state);
        assert_eq!(f.adapter.assets(), f.state.assets);
        f.state.assets = Vec::from_array(&f.env, [c, f.b.clone()]);
        apply(&f.parent, &f.state);
        assert_eq!(f.adapter.assets(), f.state.assets);
        assert_eq!(f.adapter.lastprice(&f.a), None);
        assert_eq!(f.adapter.price(&f.a, &100), None);
        assert_eq!(f.adapter.prices(&f.a, &2), None);
        assert_eq!(f.adapter.lastprice(&f.b), Some(projected(4560, 100)));
    }

    #[test]
    fn unavailable_asset_registry_traps_instead_of_claiming_an_empty_registry() {
        let mut f = fixture(8, 10);
        f.state.fail_assets = true;
        apply(&f.parent, &f.state);
        assert!(f.adapter.try_assets().is_err());
    }

    #[test]
    fn conversion_boundaries_apply_equally_to_all_three_price_paths() {
        for (decimals, mantissa, expo, timestamp, expected) in [
            (8, 123, -2, 107, Some(projected(123_000_000, 100))),
            (2, 12345, -4, 107, Some(projected(123, 100))),
            (0, 1, -1, 107, None),
            (18, i64::MAX, 20, 107, None),
            (0, 0, 0, 0, Some(projected(0, 0))),
            (0, 7, 0, 0, Some(projected(7, 0))),
            (
                0,
                7,
                0,
                u64::MAX,
                Some(projected(7, u64::MAX - u64::MAX % 10)),
            ),
        ] {
            let f = fixture(decimals, 10);
            // Keep the quote at or before ledger time so every path reads it (L-05 guard).
            f.env
                .ledger()
                .with_mut(|ledger| ledger.timestamp = ledger.timestamp.max(timestamp));
            let p = normalized(mantissa, expo, timestamp);
            set_quotes(
                &f.parent,
                &f.a,
                Some(p.clone()),
                Vec::from_array(&f.env, [p]),
            );
            assert_eq!(f.adapter.lastprice(&f.a), expected.clone());
            assert_eq!(
                f.adapter.price(&f.a, &(timestamp - timestamp % 10)),
                expected.clone()
            );
            assert_eq!(
                f.adapter.prices(&f.a, &1),
                expected.map(|p| Vec::from_array(&f.env, [p]))
            );
        }
    }

    #[test]
    fn history_is_oldest_first_and_bucket_collisions_keep_the_newest_entry() {
        let f = fixture(0, 10);
        let history = Vec::from_array(
            &f.env,
            [
                normalized(10, 0, 101),
                normalized(11, 0, 109),
                normalized(20, 0, 115),
                normalized(30, 0, 129),
            ],
        );
        set_quotes(&f.parent, &f.a, None, history);
        assert_eq!(f.adapter.lastprice(&f.a), None);
        assert_eq!(
            f.adapter.prices(&f.a, &32),
            Some(Vec::from_array(
                &f.env,
                [projected(11, 100), projected(20, 110), projected(30, 120),]
            ))
        );
        assert_eq!(
            f.adapter.prices(&f.a, &2),
            Some(Vec::from_array(
                &f.env,
                [projected(20, 110), projected(30, 120)]
            ))
        );
        assert_eq!(f.adapter.price(&f.a, &100), Some(projected(11, 100)));
        assert_eq!(f.adapter.price(&f.a, &101), None);
        assert_eq!(f.adapter.price(&f.a, &130), None);
        assert_eq!(f.adapter.prices(&f.a, &0), None);
        set_quotes(&f.parent, &f.a, None, Vec::new(&f.env));
        assert_eq!(f.adapter.prices(&f.a, &32), None);
        assert_eq!(f.adapter.price(&f.a, &100), None);
    }

    #[test]
    fn price_lookup_is_bounded_to_the_latest_32_parent_records() {
        let f = fixture(0, 1);
        let mut history = Vec::new(&f.env);
        for timestamp in 1..=33 {
            history.push_back(normalized(7, 0, timestamp));
        }
        set_quotes(&f.parent, &f.a, None, history);
        assert_eq!(f.adapter.price(&f.a, &1), None);
        assert_eq!(f.adapter.price(&f.a, &2), Some(projected(7, 2)));
        assert_eq!(f.adapter.price(&f.a, &33), Some(projected(7, 33)));
    }

    #[test]
    fn history_conversion_is_all_or_none_even_when_bad_entry_would_be_deduped() {
        let f = fixture(0, 10);
        for history in [
            Vec::from_array(&f.env, [normalized(10, 0, 100), normalized(1, -1, 110)]),
            Vec::from_array(&f.env, [normalized(1, -1, 101), normalized(10, 0, 109)]),
        ] {
            set_quotes(&f.parent, &f.a, Some(normalized(10, 0, 109)), history);
            assert_eq!(f.adapter.prices(&f.a, &32), None);
            assert_eq!(f.adapter.price(&f.a, &100), Some(projected(10, 100)));
            assert_eq!(f.adapter.lastprice(&f.a), Some(projected(10, 100)));
        }
        // A newer invalid value in a bucket must not fall back to the older value.
        set_quotes(
            &f.parent,
            &f.a,
            None,
            Vec::from_array(&f.env, [normalized(10, 0, 101), normalized(1, -1, 109)]),
        );
        assert_eq!(f.adapter.price(&f.a, &100), None);
    }

    /// L-05: rounding down to the bucket start would hide a parent price published after the
    /// current ledger from the pool's own future-dated check, so `lastprice` refuses it until
    /// ledger time reaches its timestamp.
    #[test]
    fn lastprice_refuses_a_parent_price_ahead_of_ledger_time() {
        let f = fixture(0, 10);
        set_quotes(
            &f.parent,
            &f.a,
            Some(normalized(7, 0, 115)),
            Vec::from_array(&f.env, [normalized(7, 0, 115)]),
        );
        assert_eq!(f.adapter.lastprice(&f.a), None);
        f.env.ledger().with_mut(|ledger| ledger.timestamp = 115);
        assert_eq!(f.adapter.lastprice(&f.a), Some(projected(7, 110)));
    }

    #[test]
    fn base_drift_missing_base_and_parent_failures_fail_price_reads_closed() {
        let mut f = fixture(0, 10);
        let p = normalized(7, 0, 100);
        set_quotes(
            &f.parent,
            &f.a,
            Some(p.clone()),
            Vec::from_array(&f.env, [p]),
        );
        let valid = f.state.clone();
        for case in 0..4 {
            f.state = valid.clone();
            match case {
                0 => f.state.base = Some(Asset::Other(Symbol::new(&f.env, "EUR"))),
                1 => f.state.base = None,
                2 => f.state.fail_base = true,
                3 => {
                    f.state.fail_latest = true;
                    f.state.fail_history = true;
                }
                _ => unreachable!(),
            }
            apply(&f.parent, &f.state);
            assert_eq!(f.adapter.lastprice(&f.a), None);
            assert_eq!(f.adapter.price(&f.a, &100), None);
            assert_eq!(f.adapter.prices(&f.a, &32), None);
            assert_eq!(&f.adapter.config(), &f.config);
        }
        // A broken latest endpoint must never be consulted by historical reads.
        f.state = valid;
        f.state.fail_latest = true;
        apply(&f.parent, &f.state);
        assert_eq!(f.adapter.lastprice(&f.a), None);
        assert_eq!(f.adapter.price(&f.a, &100), Some(projected(7, 100)));
        assert_eq!(
            f.adapter.prices(&f.a, &32),
            Some(Vec::from_array(&f.env, [projected(7, 100)]))
        );
    }

    #[test]
    fn ttl_extension_is_permissionless_and_config_survives_near_expiry() {
        // Deliberately unmocked auth manager: `extend_ttl` and the read paths are
        // ordinary invocations here, so any require_auth in them would be rejected by
        // the host. The constructor itself is not covered by this — `register_at` runs
        // it through the auth-bypassing unsafe path.
        let env = Env::default();
        env.ledger().with_mut(|ledger| {
            ledger.protocol_version = 25;
            ledger.timestamp = 100;
            ledger.sequence_number = 100;
            ledger.max_entry_ttl = DEFAULT_TTL_EXTEND_TO + 1;
        });
        let (parent, state, _, _) = parent(&env);
        let base = state.base.clone().unwrap();
        let owner = state.owner.clone().unwrap();
        let id = env.register_at(
            &Address::generate(&env),
            PoolOracleAdapter,
            (
                owner.clone(),
                parent.address.clone(),
                8_u32,
                10_u32,
                base.clone(),
            ),
        );
        let adapter = PoolOracleAdapterClient::new(&env, &id);
        let config = PoolOracleConfig {
            parent_oracle: parent.address.clone(),
            deployment_owner: owner,
            decimals: 8,
            resolution: 10,
            base,
        };
        assert_eq!(&adapter.config(), &config);

        let ttl = |env: &Env| env.as_contract(&id, || env.storage().instance().get_ttl());

        // Walk the instance just under the extension threshold, then bump it.
        let near = ttl(&env) - (DEFAULT_TTL_THRESHOLD - 1);
        env.ledger()
            .with_mut(|ledger| ledger.sequence_number += near);
        assert_eq!(ttl(&env), DEFAULT_TTL_THRESHOLD - 1);
        adapter.extend_ttl();
        assert_eq!(ttl(&env), DEFAULT_TTL_EXTEND_TO);
        assert_eq!(&adapter.config(), &config);

        // Second near-expiry pass: a plain read must leave stored config intact.
        let advance = ttl(&env) - (DEFAULT_TTL_THRESHOLD - 1);
        env.ledger()
            .with_mut(|ledger| ledger.sequence_number += advance);
        assert_eq!(adapter.decimals(), 8);
        assert_eq!(&adapter.config(), &config);
        assert!(ttl(&env) > 0);
    }
}

mod real_parent {
    use super::*;
    use templar_proxy_oracle_soroban_common::{ProxyConfig, SourceConfig};
    use templar_proxy_oracle_soroban_contract::{
        RefreshStatus, SorobanProxyOracle, SorobanProxyOracleClient,
    };

    #[derive(Clone)]
    #[contracttype]
    enum Key {
        Base,
        Price(Asset),
    }

    #[contract]
    struct Source;

    #[contractimpl]
    impl Source {
        pub fn __constructor(env: Env, base: Asset) {
            env.storage().instance().set(&Key::Base, &base);
        }

        pub fn set_price(env: Env, asset: Asset, price: Option<PriceData>) {
            env.storage().persistent().set(&Key::Price(asset), &price);
        }
    }

    #[contractimpl]
    impl PriceFeedTrait for Source {
        fn base(env: Env) -> Asset {
            env.storage().instance().get(&Key::Base).unwrap()
        }
        fn decimals(_env: Env) -> u32 {
            8
        }
        fn resolution(_env: Env) -> u32 {
            1
        }
        fn assets(env: Env) -> Vec<Asset> {
            Vec::new(&env)
        }
        fn lastprice(env: Env, asset: Asset) -> Option<PriceData> {
            env.storage()
                .persistent()
                .get::<_, Option<PriceData>>(&Key::Price(asset))
                .flatten()
        }
        fn price(env: Env, asset: Asset, timestamp: u64) -> Option<PriceData> {
            Self::lastprice(env, asset).filter(|price| price.timestamp == timestamp)
        }
        fn prices(env: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>> {
            if records == 0 {
                return None;
            }
            Some(Vec::from_array(
                &env,
                [Self::lastprice(env.clone(), asset)?],
            ))
        }
    }

    fn assert_quote(
        env: &Env,
        adapter: &PoolOracleAdapterClient,
        asset: &Asset,
        price: i128,
        timestamp: u64,
    ) {
        let expected = projected(price, timestamp);
        assert_eq!(adapter.lastprice(asset), Some(expected.clone()));
        assert_eq!(adapter.price(asset, &timestamp), Some(expected.clone()));
        assert_eq!(
            adapter.prices(asset, &1),
            Some(Vec::from_array(env, [expected]))
        );
    }

    /// The pinned runtime owns the per-asset breaker/cache state, not the adapter. A
    /// manual trip mutates the breakers through `with_breakers`, which invalidates the
    /// cached quote, so all three adapter paths close at once; untrip + `refresh`
    /// restores every path. A stale quote or a non-blocking failed refresh closes only
    /// `lastprice` while accepted history stays readable, the other asset is never
    /// affected, and parent owner rotation changes neither reads nor `config()`.
    #[test]
    fn real_runtime_breaker_trip_clears_every_read_and_untrip_refresh_restores_it() {
        let env = environment();
        let owner = Address::generate(&env);
        let base = Asset::Other(Symbol::new(&env, "USD"));
        let a = Asset::Stellar(Address::generate(&env));
        let b = Asset::Stellar(Address::generate(&env));
        let source_ids = [
            env.register(Source, (&base,)),
            env.register(Source, (&base,)),
            env.register(Source, (&base,)),
        ];
        let sources = source_ids.map(|id| SourceClient::new(&env, &id));
        let parent_id = env.register(SorobanProxyOracle, (&owner, &base));
        let parent = SorobanProxyOracleClient::new(&env, &parent_id);
        for (asset, value) in [(&a, 1_000_000_000), (&b, 2_000_000_000)] {
            let mut configs = Vec::new(&env);
            for source in &sources {
                source.set_price(asset, &Some(projected(value, 100)));
                configs.push_back(SourceConfig {
                    oracle: source.address.clone(),
                    asset: asset.clone(),
                });
            }
            parent.set_proxy(
                asset,
                &ProxyConfig {
                    sources: configs,
                    min_sources: 3,
                    max_age_secs: Some(30),
                    max_clock_drift_secs: Some(5),
                },
            );
            assert!(matches!(parent.refresh(asset), RefreshStatus::Accepted(_)));
        }
        let adapter_id = env.register(
            PoolOracleAdapter,
            (&owner, &parent_id, &8_u32, &10_u32, &base),
        );
        let adapter = PoolOracleAdapterClient::new(&env, &adapter_id);
        let original_config = adapter.config();
        assert_quote(&env, &adapter, &a, 1_000_000_000, 100);
        assert_quote(&env, &adapter, &b, 2_000_000_000, 100);

        parent.set_manual_trip(&a, &true, &None);
        assert_eq!(adapter.price(&a, &100), None);
        assert_eq!(adapter.prices(&a, &32), None);
        // not reachable: "lastprice still returns until refresh" — the pinned runtime's
        // `with_breakers` invalidates the latest cache on any breaker mutation
        // (proxy-oracle contract/src/lib.rs:123), so a manual trip clears latest with
        // history. The latest-vs-history divergence is proven by the failed-refresh step.
        assert_eq!(adapter.lastprice(&a), None);
        assert_quote(&env, &adapter, &b, 2_000_000_000, 100);
        assert!(matches!(parent.refresh(&a), RefreshStatus::Blocked(_)));
        assert_eq!(adapter.lastprice(&a), None);
        assert_eq!(adapter.price(&a, &100), None);
        assert_eq!(adapter.prices(&a, &32), None);
        assert_quote(&env, &adapter, &b, 2_000_000_000, 100);

        parent.set_manual_trip(&a, &false, &None);
        assert!(matches!(parent.refresh(&a), RefreshStatus::Accepted(_)));
        assert_quote(&env, &adapter, &a, 1_000_000_000, 100);
        assert_quote(&env, &adapter, &b, 2_000_000_000, 100);

        // Only A becomes stale: keep B's independent sources and cache fresh.
        env.ledger().with_mut(|ledger| ledger.timestamp = 140);
        for source in &sources {
            source.set_price(&b, &Some(projected(2_000_000_000, 140)));
        }
        assert!(matches!(parent.refresh(&b), RefreshStatus::Accepted(_)));
        assert_eq!(adapter.lastprice(&a), None);
        assert_eq!(adapter.price(&a, &100), Some(projected(1_000_000_000, 100)));
        assert_eq!(
            adapter.prices(&a, &32),
            Some(Vec::from_array(&env, [projected(1_000_000_000, 100)]))
        );
        assert_quote(&env, &adapter, &b, 2_000_000_000, 140);
        for source in &sources {
            source.set_price(&a, &None);
        }
        assert!(matches!(
            parent.refresh(&a),
            RefreshStatus::SourceUnavailable
        ));
        assert_eq!(adapter.lastprice(&a), None);
        assert_eq!(adapter.price(&a, &100), Some(projected(1_000_000_000, 100)));
        assert_eq!(
            adapter.prices(&a, &32),
            Some(Vec::from_array(&env, [projected(1_000_000_000, 100)]))
        );
        assert_quote(&env, &adapter, &b, 2_000_000_000, 140);

        // The same non-blocking failure invalidates a still-fresh latest cache too.
        for source in &sources {
            source.set_price(&a, &Some(projected(1_100_000_000, 140)));
        }
        assert!(matches!(parent.refresh(&a), RefreshStatus::Accepted(_)));
        assert_quote(&env, &adapter, &a, 1_100_000_000, 140);
        for source in &sources {
            source.set_price(&a, &None);
        }
        assert!(matches!(
            parent.refresh(&a),
            RefreshStatus::SourceUnavailable
        ));
        assert_eq!(adapter.lastprice(&a), None);
        assert_eq!(adapter.price(&a, &140), Some(projected(1_100_000_000, 140)));
        let accepted_history = Vec::from_array(
            &env,
            [projected(1_000_000_000, 100), projected(1_100_000_000, 140)],
        );
        assert_eq!(adapter.prices(&a, &32), Some(accepted_history.clone()));
        assert_quote(&env, &adapter, &b, 2_000_000_000, 140);

        let new_owner = Address::generate(&env);
        parent.transfer_ownership(&new_owner, &env.ledger().max_live_until_ledger());
        parent.accept_ownership();
        assert_eq!(parent.get_owner(), Some(new_owner));
        assert_eq!(adapter.config(), original_config);
        assert_eq!(adapter.lastprice(&a), None);
        assert_eq!(adapter.price(&a, &140), Some(projected(1_100_000_000, 140)));
        assert_eq!(adapter.prices(&a, &32), Some(accepted_history));
        assert_quote(&env, &adapter, &b, 2_000_000_000, 140);
    }

    /// The default build serves only what a keeper refreshed. The eager build's
    /// `lastprice` refreshes the parent first, so a new source price is served with
    /// no keeper at all, while the runtime's breakers and fail-closed refresh still
    /// decide what comes back.
    #[test]
    fn lastprice_refreshes_the_parent_only_in_the_eager_build() {
        let eager = cfg!(feature = "dangerous-eager-refresh");
        let env = environment();
        let owner = Address::generate(&env);
        let base = Asset::Other(Symbol::new(&env, "USD"));
        let a = Asset::Stellar(Address::generate(&env));
        let sources = [(); 3].map(|()| SourceClient::new(&env, &env.register(Source, (&base,))));
        let parent_id = env.register(SorobanProxyOracle, (&owner, &base));
        let parent = SorobanProxyOracleClient::new(&env, &parent_id);
        let set_prices = |price: Option<PriceData>| {
            for source in &sources {
                source.set_price(&a, &price);
            }
        };
        set_prices(Some(projected(1_000_000_000, 100)));
        let mut configs = Vec::new(&env);
        for source in &sources {
            configs.push_back(SourceConfig {
                oracle: source.address.clone(),
                asset: a.clone(),
            });
        }
        parent.set_proxy(
            &a,
            &ProxyConfig {
                sources: configs,
                min_sources: 3,
                max_age_secs: Some(30),
                max_clock_drift_secs: Some(5),
            },
        );
        let adapter_id = env.register(
            PoolOracleAdapter,
            (&owner, &parent_id, &8_u32, &10_u32, &base),
        );
        let adapter = PoolOracleAdapterClient::new(&env, &adapter_id);

        // Nobody has called `refresh`.
        let first = eager.then(|| projected(1_000_000_000, 100));
        assert_eq!(adapter.lastprice(&a), first);
        assert_eq!(parent.get_cached(&a).is_some(), eager);

        env.ledger().with_mut(|ledger| ledger.timestamp = 140);
        set_prices(Some(projected(1_100_000_000, 140)));
        let second = eager.then(|| projected(1_100_000_000, 140));
        assert_eq!(adapter.lastprice(&a), second);

        // A manual trip blocks the refresh, and untripping serves again.
        parent.set_manual_trip(&a, &true, &None);
        assert_eq!(adapter.lastprice(&a), None);
        parent.set_manual_trip(&a, &false, &None);
        assert_eq!(adapter.lastprice(&a), second);

        // A failed refresh closes the read instead of serving the last good price.
        set_prices(None);
        assert_eq!(adapter.lastprice(&a), None);
    }
}
