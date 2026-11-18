extern crate std;

use soroban_sdk::{
    contract, contractimpl, contracttype, map, panic_with_error, testutils::Address as _, Address,
    Env, Error, Map, Symbol, Vec,
};
use templar_proxy_oracle_soroban_common::{Asset, PriceData, PriceFeedTrait};

use crate::{rescale, Sep40RescaleAdapter, Sep40RescaleAdapterClient, DECIMALS};

#[contracttype]
enum Key {
    Decimals,
    Broken,
    History(Asset),
}

/// A SEP-40 source whose decimals, per-asset history and failure are set by the test.
#[contract]
struct Source;

#[contractimpl]
impl Source {
    pub fn set_decimals(env: Env, decimals: u32) {
        env.storage().instance().set(&Key::Decimals, &decimals);
    }

    pub fn set_history(env: Env, asset: Asset, history: Vec<PriceData>) {
        env.storage().instance().set(&Key::History(asset), &history);
    }

    pub fn set_broken(env: Env, broken: bool) {
        env.storage().instance().set(&Key::Broken, &broken);
    }
}

fn history(env: &Env, asset: &Asset) -> Option<Vec<PriceData>> {
    if env.storage().instance().get(&Key::Broken).unwrap_or(false) {
        panic_with_error!(env, Error::from_contract_error(1));
    }
    env.storage().instance().get(&Key::History(asset.clone()))
}

#[contractimpl]
impl PriceFeedTrait for Source {
    fn base(env: Env) -> Asset {
        Asset::Other(Symbol::new(&env, "USD"))
    }
    fn assets(env: Env) -> Vec<Asset> {
        Vec::from_array(&env, [Asset::Other(Symbol::new(&env, "earnUSDC_PPS"))])
    }
    fn decimals(env: Env) -> u32 {
        env.storage().instance().get(&Key::Decimals).unwrap()
    }
    fn resolution(_env: Env) -> u32 {
        43_200
    }
    fn price(env: Env, asset: Asset, timestamp: u64) -> Option<PriceData> {
        history(&env, &asset)?
            .iter()
            .find(|price| price.timestamp == timestamp)
    }
    fn prices(env: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>> {
        let history = history(&env, &asset)?;
        let from = history.len().saturating_sub(records);
        Some(history.slice(from..))
    }
    fn lastprice(env: Env, asset: Asset) -> Option<PriceData> {
        history(&env, &asset)?.last()
    }
}

fn quote(price: i128, timestamp: u64) -> PriceData {
    PriceData { price, timestamp }
}

struct Fixture {
    env: Env,
    source: SourceClient<'static>,
    adapter: Sep40RescaleAdapterClient<'static>,
    /// What a pool asks for: the reserve token's Stellar address.
    token: Asset,
    /// What the source serves it under.
    feed: Asset,
}

fn fixture(source_decimals: u32) -> Fixture {
    let env = Env::default();
    let source = SourceClient::new(&env, &env.register(Source, ()));
    source.set_decimals(&source_decimals);
    let token = Asset::Stellar(Address::generate(&env));
    let feed = Asset::Other(Symbol::new(&env, "earnUSDC_PPS"));
    let aliases: Map<Asset, Asset> = map![&env, (token.clone(), feed.clone())];
    let adapter_id = env.register(Sep40RescaleAdapter, (&source.address, aliases));
    let adapter = Sep40RescaleAdapterClient::new(&env, &adapter_id);
    Fixture {
        env,
        source,
        adapter,
        token,
        feed,
    }
}

#[test]
fn rescale_truncates_down_scales_up_and_refuses_what_it_cannot_represent() {
    // Mainnet values read from RedStone's SEP-40 wrapper (8 decimals) on 2026-11-16.
    assert_eq!(
        rescale(quote(102_020_197, 9), 8),
        Some(quote(10_202_019, 9))
    );
    assert_eq!(rescale(quote(99_978_438, 9), 8), Some(quote(9_997_843, 9)));
    assert_eq!(rescale(quote(12_345_678, 9), 7), Some(quote(12_345_678, 9)));
    assert_eq!(rescale(quote(1_234_567, 9), 6), Some(quote(12_345_670, 9)));
    assert_eq!(
        rescale(quote(1_020_201_970_000_000_000, 9), 18),
        Some(quote(10_202_019, 9))
    );
    assert_eq!(rescale(quote(1, 9), 0), Some(quote(10_000_000, 9)));
    // Truncated to zero, overflowed, unrepresentable scale, or not positive.
    assert_eq!(rescale(quote(9, 9), 8), None);
    assert_eq!(rescale(quote(i128::MAX, 9), 0), None);
    assert_eq!(rescale(quote(1, 9), DECIMALS + 39), None);
    assert_eq!(rescale(quote(0, 9), 7), None);
    assert_eq!(rescale(quote(-10_000_000, 9), 7), None);
}

#[test]
fn reads_forward_live_through_aliases_at_seven_decimals() {
    let f = fixture(8);
    let history = Vec::from_array(
        &f.env,
        [
            quote(101_937_494, 100),
            quote(102_020_197, 200),
            quote(102_020_198, 300),
        ],
    );
    f.source.set_history(&f.feed, &history);

    assert_eq!(f.adapter.decimals(), 7);
    assert_eq!(f.adapter.base(), Asset::Other(Symbol::new(&f.env, "USD")));
    assert_eq!(f.adapter.resolution(), 43_200);
    assert_eq!(
        f.adapter.assets(),
        Vec::from_array(&f.env, [f.feed.clone()])
    );

    // The pool asks by token; the source serves the alias. The direct feed key works too.
    assert_eq!(f.adapter.lastprice(&f.token), Some(quote(10_202_019, 300)));
    assert_eq!(f.adapter.lastprice(&f.feed), Some(quote(10_202_019, 300)));
    assert_eq!(
        f.adapter.price(&f.token, &200),
        Some(quote(10_202_019, 200))
    );
    assert_eq!(f.adapter.price(&f.token, &250), None);
    assert_eq!(
        f.adapter.prices(&f.token, &2),
        Some(Vec::from_array(
            &f.env,
            [quote(10_202_019, 200), quote(10_202_019, 300)]
        ))
    );
    assert_eq!(f.adapter.prices(&f.token, &0), None);

    // Decimals are read on every call, never cached.
    f.source.set_decimals(&6);
    assert_eq!(
        f.adapter.lastprice(&f.token),
        Some(quote(1_020_201_980, 300))
    );

    // Unknown asset, source trap and one unrepresentable record all fail closed.
    let other = Asset::Stellar(Address::generate(&f.env));
    assert_eq!(f.adapter.lastprice(&other), None);
    f.source.set_history(
        &f.feed,
        &Vec::from_array(&f.env, [quote(i128::MAX, 100), quote(1, 200)]),
    );
    assert_eq!(f.adapter.prices(&f.token, &2), None);
    assert_eq!(f.adapter.lastprice(&f.token), Some(quote(10, 200)));
    f.source.set_broken(&true);
    assert_eq!(f.adapter.lastprice(&f.token), None);
    assert_eq!(f.adapter.price(&f.token, &200), None);
    assert_eq!(f.adapter.prices(&f.token, &2), None);
}
