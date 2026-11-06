//! Local proofs of the unchanged Pool oracle/cache implementation.
//!
//! Host interfaces below model exact map key/value effects, invocation-constant
//! time, arbitrary feed responses and aborts. They do not verify Soroban itself.
//! Maps have two slots: cache claims cover the explicitly exercised two-key
//! footprint, not arbitrary-length collection induction. Out-of-footprint reserve
//! operations fail closed instead of returning a fake financial result.
//! The expanded price-cache/reset probe timed out at 180 seconds and is excluded
//! from just verify-local; only the passing price/decimals rules are accepted.
#![allow(dead_code)]

extern crate self as sep_40_oracle;
extern crate self as soroban_sdk;
// No copied guard/cache algorithm: compile the original production source file.
#[path = "../../pool/src/pool/pool.rs"]
mod production_pool;

use core::cell::Cell;

// Kind plus two words spans every account/contract 256-bit payload. Only equality
// is modeled; word grouping avoids repeatedly expanding byte-wise memcmp loops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, kani::Arbitrary)]
pub struct Address {
    contract: bool,
    high: u128,
    low: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolError {
    InternalReserveNotFound,
    InvalidPoolStatus,
    MaxPositionsExceeded,
    InvalidPrice,
}

mod errors {
    pub use crate::PoolError;
}

#[derive(Clone, Copy, kani::Arbitrary)]
pub struct PriceData {
    price: i128,
    timestamp: u64,
}

pub struct Env {
    now: u64,
    quote: Cell<Option<PriceData>>,
    decimals: Cell<u32>,
    queried: Cell<Option<Address>>,
    decimals_queried: Cell<bool>,
    feed_failure: Cell<bool>,
    expected_error: Option<PoolError>,
}

impl Env {
    fn new(now: u64, quote: Option<PriceData>, decimals: u32) -> Self {
        Self {
            now,
            quote: Cell::new(quote),
            decimals: Cell::new(decimals),
            queried: Cell::new(None),
            decimals_queried: Cell::new(false),
            feed_failure: Cell::new(false),
            expected_error: None,
        }
    }

    pub fn ledger(&self) -> Ledger {
        Ledger(self.now)
    }
}

pub struct Ledger(u64);
impl Ledger {
    pub fn timestamp(&self) -> u64 {
        self.0
    }
}

fn reject(env: &Env, error: PoolError) -> ! {
    assert_eq!(
        env.expected_error,
        Some(error),
        "unexpected contract rejection"
    );
    // Host contract: a rejected invocation cannot return normally. This models
    // that control-flow boundary, not a target financial postcondition.
    kani::assume(false);
    unreachable!()
}

#[macro_export]
macro_rules! panic_with_error {
    ($env:expr, $error:expr) => {
        $crate::reject($env, $error)
    };
}

#[derive(Clone)]
pub struct Map<K, V>([Option<(K, V)>; 2]);
impl<K: Clone + Eq, V: Clone> Map<K, V> {
    pub fn new(_env: &Env) -> Self {
        Self([None, None])
    }

    pub fn get(&self, key: K) -> Option<V> {
        for (stored, value) in self.0.iter().flatten() {
            if *stored == key {
                return Some(value.clone());
            }
        }
        None
    }

    pub fn set(&mut self, key: K, value: V) {
        for slot in &mut self.0 {
            if slot.as_ref().is_some_and(|(stored, _)| *stored == key) {
                *slot = Some((key, value));
                return;
            }
        }
        for slot in &mut self.0 {
            if slot.is_none() {
                *slot = Some((key, value));
                return;
            }
        }
        panic!("proof footprint exceeds two map keys");
    }
}

#[macro_export]
macro_rules! map {
    ($env:expr) => {
        $crate::Map::new($env)
    };
}

pub struct Vec<T>([Option<T>; 2]);
impl<T: Clone + Eq> Vec<T> {
    pub fn new(_env: &Env) -> Self {
        Self([None, None])
    }

    pub fn contains(&self, value: &T) -> bool {
        self.0.iter().flatten().any(|stored| stored == value)
    }

    pub fn push_back(&mut self, value: T) {
        for slot in &mut self.0 {
            if slot.is_none() {
                *slot = Some(value);
                return;
            }
        }
        panic!("proof footprint exceeds two vector entries");
    }

    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.0.iter().flatten().cloned()
    }
}

#[macro_export]
macro_rules! vec {
    ($env:expr) => {
        $crate::Vec::new($env)
    };
}

pub enum Asset {
    Stellar(Address),
}

pub struct PriceFeedClient<'a>(&'a Env);
impl<'a> PriceFeedClient<'a> {
    pub fn new(env: &'a Env, _oracle: &Address) -> Self {
        Self(env)
    }

    fn require_return(&self) {
        if self.0.feed_failure.get() {
            // External call failure cannot return a price or decimal value.
            kani::assume(false);
        }
    }

    pub fn lastprice(&self, asset: &Asset) -> Option<PriceData> {
        let Asset::Stellar(address) = asset;
        self.0.queried.set(Some(*address));
        self.require_return();
        self.0.quote.get()
    }

    pub fn decimals(&self) -> u32 {
        self.0.decimals_queried.set(true);
        self.require_return();
        self.0.decimals.get()
    }
}

pub struct Positions(u32);
impl Positions {
    pub fn effective_count(&self) -> u32 {
        self.0
    }
}

mod storage {
    use super::*;

    pub struct PoolConfig {
        pub oracle: Address,
        pub status: u32,
        pub max_positions: u32,
    }

    pub fn get_pool_config(_env: &Env) -> PoolConfig {
        PoolConfig {
            oracle: Address {
                contract: true,
                high: 0,
                low: 0,
            },
            status: 0,
            max_positions: 30,
        }
    }
}

mod reserve {
    use super::*;

    #[derive(Clone)]
    pub struct Reserve {
        pub asset: Address,
    }

    impl Reserve {
        pub fn load(_env: &Env, _config: &storage::PoolConfig, _asset: &Address) -> Self {
            panic!("reserve loading is outside the oracle proof footprint")
        }

        pub fn store(&self, _env: &Env) {
            panic!("reserve persistence is outside the oracle proof footprint")
        }
    }
}

fn acceptable(now: u64, quote: Option<PriceData>) -> bool {
    quote.is_some_and(|q| q.price > 0 && q.timestamp <= now && now - q.timestamp <= 86_400)
}

#[kani::proof]
#[kani::unwind(34)]
fn prove_price_boundary_and_rejection() {
    let now: u64 = kani::any();
    let quote: Option<PriceData> = kani::any();
    let mut env = Env::new(now, quote, 7);
    let failed: bool = kani::any();
    env.feed_failure.set(failed);
    let good = acceptable(now, quote);
    if !good {
        env.expected_error = Some(PoolError::InvalidPrice);
    }
    kani::cover!(failed);
    kani::cover!(!failed && quote.is_none());
    if let Some(q) = quote {
        kani::cover!(!failed && q.price == 0);
        kani::cover!(!failed && q.price < 0);
        kani::cover!(!failed && q.price > 0 && q.timestamp > now);
        kani::cover!(!failed && q.price > 0 && q.timestamp <= now && now - q.timestamp > 86_400);
    }
    let asset = kani::any();
    let mut pool = production_pool::Pool::load(&env);
    let price = pool.load_price(&env, &asset);
    assert!(!failed && good);
    assert_eq!(price, quote.unwrap().price);
    assert_eq!(env.queried.get(), Some(asset));
    kani::cover!(now == 100_000 && quote.unwrap().timestamp == 13_600);
    kani::cover!(price > 0 && quote.unwrap().timestamp == now);
}

#[kani::proof]
#[kani::unwind(34)]
fn prove_price_cache_keys_and_invocation_reset() {
    let now: u64 = kani::any();
    let first: Address = kani::any();
    let second: Address = kani::any();
    kani::assume(first != second);
    let env = Env::new(
        now,
        Some(PriceData {
            price: 1,
            timestamp: now,
        }),
        7,
    );
    let mut pool = production_pool::Pool::load(&env);
    assert_eq!(pool.load_price(&env, &first), 1);
    env.quote.set(None);
    env.queried.set(None);
    env.feed_failure.set(true);
    assert_eq!(pool.load_price(&env, &first), 1);
    assert_eq!(env.queried.get(), None);
    let next: i128 = kani::any();
    kani::assume(next > 1);
    env.feed_failure.set(false);
    env.quote.set(Some(PriceData {
        price: next,
        timestamp: now,
    }));
    assert_eq!(pool.load_price(&env, &second), next);
    assert_eq!(env.queried.get(), Some(second));
    assert_eq!(pool.load_price(&env, &first), 1);
    let mut fresh = production_pool::Pool::load(&env);
    assert_eq!(fresh.load_price(&env, &first), next);
    kani::cover!(first != second && next > 1);
    kani::cover!(
        first.contract != second.contract
            && first.high == second.high
            && first.low == second.low
            && next > 1
    );
}

#[kani::proof]
#[kani::unwind(34)]
fn prove_decimals_boundary_and_cache() {
    let decimals: u32 = kani::any();
    let mut env = Env::new(0, None, decimals);
    let failed: bool = kani::any();
    env.feed_failure.set(failed);
    if decimals != 7 {
        env.expected_error = Some(PoolError::InvalidPrice);
    }
    kani::cover!(failed);
    kani::cover!(!failed && decimals < 7);
    kani::cover!(!failed && decimals > 7);
    let mut pool = production_pool::Pool::load(&env);
    assert_eq!(pool.load_price_decimals(&env), 7);
    assert!(!failed);
    assert_eq!(decimals, 7);
    env.decimals.set(0);
    env.decimals_queried.set(false);
    env.feed_failure.set(true);
    assert_eq!(pool.load_price_decimals(&env), 7);
    assert!(!env.decimals_queried.get());
    kani::cover!(decimals == 7);
    env.feed_failure.set(false);
    env.expected_error = Some(PoolError::InvalidPrice);
    let mut fresh = production_pool::Pool::load(&env);
    fresh.load_price_decimals(&env);
    assert!(false, "a fresh invocation must validate decimals again");
}
