//! Curated operator guides for one deployed market (plan §4).
//!
//! A guide is a recipe and nothing more: it names a category and action,
//! resolves the manifest role and the live exported function behind it, and
//! fills in the argument values this run's frozen manifest already fixes
//! (asset addresses, the admin caller, this market's pool). It never signs,
//! journals, simulates, or submits. `market_call::prepare` and
//! `market_call::execute` stay the single guarded sending path, and they
//! re-resolve the target and revalidate the live interface immediately before
//! their own pre-send intent, so nothing here is a trust anchor.
//!
//! Every method named below is one these contracts actually export: the pool
//! and backstop entrypoints of the pinned Blend v2 contracts
//! (`vendor/blend-contracts-v2/pool/src/contract.rs`,
//! `vendor/blend-contracts-v2/backstop/src/contract.rs`), and the proxy
//! runtime, governance and source entrypoints this tool already drives
//! (`src/market.rs`, `src/deploy.rs`). Nothing is guessed, and a guide whose
//! function is absent from the deployed interface is refused through
//! `market_explorer::inspect` rather than emulated.
//!
//! Rejections are input-free where the input came from an operator or the
//! network: an unknown category or action is answered by pointing at the
//! catalog, and argument names come only from an interface the CLI seam
//! already validated as control-free.

use std::path::Path;

use crate::manifest::{MarketManifest, Tok};
use crate::market_explorer;
use serde_json::{json, Value};

use crate::stellar::{Executor, Safe, Stellar};
fn safe_refusal(message: &'static str) -> anyhow::Error {
    anyhow::Error::msg(Safe::literal(message))
}

fn safe_refusal_owned(message: String) -> anyhow::Error {
    let safe = Safe::render_market_json(&json!(message), 4_096)
        .unwrap_or_else(|_| Safe::literal("guided validation refused this call"));
    anyhow::Error::msg(safe)
}

/// A live recipe split into read-only manifest bindings and caller inputs.
pub struct GuideForm {
    pub role: String,
    pub method: String,
    pub spec: Safe,
    pub bound: Vec<(String, String)>,
    pub inputs: Vec<(String, Value)>,
    /// Manifest-bound asset choices eligible for this recipe.
    pub assets: Vec<Tok>,
    /// The selected manifest token after applying the eligible default.
    pub selected_asset: Option<Tok>,
}

/// Role placeholder: the target is the manifest token for the selected asset.
const ROLE_TOKEN: &str = "token";
/// Role placeholder: the target is one deployed price source.
const ROLE_SOURCE: &str = "source";

/// One curated guide. `role` is a frozen manifest role or one of the two
/// selector-resolved placeholders above; `method` is the exported function it
/// needs; `help` says what it does and what the caller still has to supply.
struct Guide {
    category: &'static str,
    action: &'static str,
    role: &'static str,
    method: &'static str,
    help: &'static str,
}

/// The whole catalog. This table is the only source of guide names, targets
/// and methods, so the help surface and the selector cannot drift.
const GUIDES: &[Guide] = &[
    // Oracle and source publication.
    Guide {
        category: "oracle",
        action: "publish-price",
        role: ROLE_SOURCE,
        method: "set_data",
        help: "publish one price for one asset on ONE deployed source (select source 0, 1 or 2). You supply price and timestamp; nothing publishes automatically and every source is confirmed on its own. The freshness window checked here is the host clock, which is not ledger time: the source replay guard and the proxy's age and clock-drift filters stay the enforcement.",
    },
    Guide {
        category: "oracle",
        action: "refresh",
        role: "proxy-runtime",
        method: "refresh",
        help: "recompute and cache the proxy price for one asset. The asset is bound from this manifest.",
    },
    // Governance: breaker movement is proposal-only, never an owner call.
    Guide {
        category: "governance",
        action: "propose-manual-trip",
        role: "proxy-governance",
        method: "create_proposal",
        help: "propose a manual breaker trip for one asset. Caller, tagged SetManualTrip operation, zero requested TTL and the exact live next_proposal_id are bound read-only.",
    },
    Guide {
        category: "governance",
        action: "propose-manual-untrip",
        role: "proxy-governance",
        method: "create_proposal",
        help: "propose clearing a manual breaker trip for one asset. Same bindings as the trip guide, including the exact live next_proposal_id.",
    },
    Guide {
        category: "governance",
        action: "execute-proposal",
        role: "proxy-governance",
        method: "execute_proposal",
        help: "execute one governance proposal by id. The caller is bound to this manifest's admin; you supply id.",
    },
    Guide {
        category: "governance",
        action: "proposal-count",
        role: "proxy-governance",
        method: "next_proposal_id",
        help: "read the next proposal id. It takes no arguments and is the only safe source for the id the proposal guides need.",
    },
    // Pool.
    Guide {
        category: "pool",
        action: "submit",
        role: "pool",
        method: "submit",
        help: "submit a set of pool requests. You supply from, spender, to and requests.",
    },
    Guide {
        category: "pool",
        action: "submit-with-allowance",
        role: "pool",
        method: "submit_with_allowance",
        help: "submit requests that draw on an existing token allowance. You supply from, spender, to and requests.",
    },
    Guide {
        category: "pool",
        action: "flash-loan",
        role: "pool",
        method: "flash_loan",
        help: "submit requests bundled with a flash loan. You supply from, flash_loan and requests.",
    },
    Guide {
        category: "pool",
        action: "set-status",
        role: "pool",
        method: "set_status",
        help: "set the pool status by number; you supply pool_status. Status 4 (frozen) is absorbing: the pool never leaves it.",
    },
    Guide {
        category: "pool",
        action: "update-status",
        role: "pool",
        method: "update_status",
        help: "let the pool advance its own status from on-chain state. It takes no arguments.",
    },
    Guide {
        category: "pool",
        action: "gulp",
        role: "pool",
        method: "gulp",
        help: "settle pool-owned orphan b-tokens for one reserve once it has no debt (d_supply = 0): burns the pool's orphan claim and returns 0. It never recognizes donated or unaccounted-for tokens as surplus. Refuses with BadRequest (1200) while debt remains or b-token emissions are configured for the reserve; with no orphan claim it is a no-op. The asset is bound from this manifest.",
    },
    Guide {
        category: "pool",
        action: "claim",
        role: "pool",
        method: "claim",
        help: "claim outstanding emissions. You supply from, reserve_token_ids and to.",
    },
    Guide {
        category: "pool",
        action: "set-emissions-config",
        role: "pool",
        method: "set_emissions_config",
        help: "set pool emissions configuration using res_emission_metadata rows (res_index, res_type, share); [] is a valid ABI input. The Precept fork rejects this operation outright with BadRequest (1200); `r` checks the live contract guard.",
    },
    Guide {
        category: "pool",
        action: "bad-debt",
        role: "pool",
        method: "bad_debt",
        help: "handle one user's bad debt. Eligible only when the user has liabilities, zero aggregate raw collateral value (priced from the oracle, so position assets need current prices), and no live liquidation auction: each debt is first set off against the user's own supply in the same reserve, then any residual is defaulted directly (no backstop takeover), and leftover collateral moves to pool custody as orphan b-tokens only if debt was defaulted. Otherwise BadRequest (1200), or AuctionInProgress (1212) while a liquidation auction is live; the backstop address is refused. You supply user.",
    },
    Guide {
        category: "pool",
        action: "propose-admin",
        role: "pool",
        method: "propose_admin",
        help: "propose a new pool admin; you supply new_admin. That address has to accept before the role moves.",
    },
    Guide {
        category: "pool",
        action: "accept-admin",
        role: "pool",
        method: "accept_admin",
        help: "accept a proposed pool admin role. It takes no arguments, and only the proposed address can do it.",
    },
    // Market tokens.
    Guide {
        category: "token",
        action: "transfer",
        role: ROLE_TOKEN,
        method: "transfer",
        help: "move tokens of one market asset. The token contract is bound from this manifest; you supply from, to and amount.",
    },
    Guide {
        category: "token",
        action: "balance",
        role: ROLE_TOKEN,
        method: "balance",
        help: "read a holder's balance of one market asset. The token contract is bound from this manifest; you supply id.",
    },
    Guide {
        category: "token",
        action: "mint",
        role: ROLE_TOKEN,
        method: "mint",
        help: "mint tokens of one market asset to an address; only the token's admin can. This fails once the admin is the faucet; use token/faucet-mint then. You supply to and amount.",
    },
    Guide {
        category: "token",
        action: "faucet-mint",
        role: "faucet",
        method: "mint",
        help: "mint tokens of one market asset to any address through this run's attached open faucet; no admin signature needed. The token is bound from this manifest; you supply to and amount. Attach the faucet role with market attach first.",
    },
    // Backstop.
    Guide {
        category: "backstop",
        action: "deposit",
        role: "backstop",
        method: "deposit",
        help: "deposit backstop tokens for this market's pool, which is bound from this manifest; you supply from and amount.",
    },
    Guide {
        category: "backstop",
        action: "queue-withdrawal",
        role: "backstop",
        method: "queue_withdrawal",
        help: "queue backstop shares of this market's pool for withdrawal. The lifecycle cannot be skipped: a queue is required before any withdrawal. You supply from and amount.",
    },
    Guide {
        category: "backstop",
        action: "dequeue-withdrawal",
        role: "backstop",
        method: "dequeue_withdrawal",
        help: "remove shares from a queued backstop withdrawal. You supply from and amount.",
    },
    Guide {
        category: "backstop",
        action: "withdraw",
        role: "backstop",
        method: "withdraw",
        help: "withdraw shares from a queued backstop withdrawal once its delay has passed. You supply from and amount.",
    },
    Guide {
        category: "backstop",
        action: "claim",
        role: "backstop",
        method: "claim",
        help: "claim backstop deposit emissions. The pool list is bound to this market's pool alone; you supply from and min_lp_tokens_out.",
    },
    Guide {
        category: "backstop",
        action: "donate",
        role: "backstop",
        method: "donate",
        help: "send backstop tokens into this market's backstop without taking credit. You supply from and amount, and the pool has to authorize the call.",
    },
    Guide {
        category: "backstop",
        action: "draw",
        role: "backstop",
        method: "draw",
        help: "move backstop tokens out of this market's backstop to an address. You supply amount and to; only the pool can authorize it.",
    },
    Guide {
        category: "backstop",
        action: "distribute",
        role: "backstop",
        method: "distribute",
        help: "distribute emissions across the reward zone. It takes no arguments.",
    },
    Guide {
        category: "backstop",
        action: "drop",
        role: "backstop",
        method: "drop",
        help: "ask the backstop to drop its market setup. Expected disabled on the Precept fork without insurance; `r` checks the current live simulation. It takes no arguments.",
    },
    Guide {
        category: "backstop",
        action: "gulp-emissions",
        role: "backstop",
        method: "gulp_emissions",
        help: "ask the backstop to credit emissions for this market's pool. The pool address is bound from the manifest. Expected disabled on the Precept fork without insurance; `r` checks current live behavior.",
    },
    Guide {
        category: "backstop",
        action: "add-reward",
        role: "backstop",
        method: "add_reward",
        help: "ask the backstop to add this market's pool as a reward target with no target to remove. Both arguments are manifest-bound; expected disabled on the Precept fork without insurance. `r` checks the current live simulation.",
    },
    Guide {
        category: "backstop",
        action: "remove-reward",
        role: "backstop",
        method: "remove_reward",
        help: "ask the backstop to remove this market's pool as a reward target. The address is manifest-bound; expected disabled on the Precept fork without insurance. `r` checks the current live simulation.",
    },
    Guide {
        category: "backstop",
        action: "user-balance",
        role: "backstop",
        method: "user_balance",
        help: "read one holder's backstop balance for this market's pool. The pool is bound from this manifest; you supply user.",
    },
    Guide {
        category: "backstop",
        action: "pool-data",
        role: "backstop",
        method: "pool_data",
        help: "read this market's backstop data. The pool is bound from this manifest and nothing else is needed.",
    },
    Guide {
        category: "backstop",
        action: "reward-zone",
        role: "backstop",
        method: "reward_zone",
        help: "read the reward zone. It takes no arguments.",
    },
    // Auctions and liquidations, all against the pool.
    Guide {
        category: "auction",
        action: "create-liquidation",
        role: "pool",
        method: "new_auction",
        help: "create a liquidation auction, which is how an undercollateralized position is put up for liquidation. auction_type is bound to 0; you supply user, bid, lot and percent. Read get-liquidation first.",
    },
    Guide {
        category: "auction",
        action: "create-bad-debt",
        role: "pool",
        method: "new_auction",
        help: "attempt a bad-debt auction. auction_type is bound to 1 and user to this market's backstop; you supply bid, lot and percent (expected 100). Expected disabled on the Precept fork without insurance; `r` checks the current live simulation.",
    },
    Guide {
        category: "auction",
        action: "create-interest",
        role: "pool",
        method: "new_auction",
        help: "attempt an interest auction. auction_type is bound to 2 and user to this market's backstop; you supply bid, lot and percent (expected 100). Expected disabled on the Precept fork without insurance; `r` checks the current live simulation.",
    },
    Guide {
        category: "auction",
        action: "get-liquidation",
        role: "pool",
        method: "get_auction",
        help: "read a liquidation auction. auction_type is bound to 0; you supply user. The answer scales with the time since creation.",
    },
    Guide {
        category: "auction",
        action: "fill-liquidation",
        role: "pool",
        method: "submit",
        help: "fill a liquidation auction by submitting a pool request of type 6 (fill user liquidation auction). You supply from, spender, to and the requests; in each request, address is the liquidated user (the auction owner) and amount is the percent of the auction to fill (for example 100).",
    },
    Guide {
        category: "auction",
        action: "fill-bad-debt",
        role: "pool",
        method: "submit",
        help: "fill a bad-debt auction by submitting a pool request of type 7 (fill bad debt auction). You supply from, spender, to and the requests.",
    },
    Guide {
        category: "auction",
        action: "fill-interest",
        role: "pool",
        method: "submit",
        help: "fill an interest auction by submitting a pool request of type 8 (fill interest auction). You supply from, spender, to and the requests.",
    },
    Guide {
        category: "auction",
        action: "delete-liquidation",
        role: "pool",
        method: "submit",
        help: "cancel an unfilled liquidation auction by submitting a pool request of type 9 (delete liquidation auction). You supply from, spender, to and the requests.",
    },
    Guide {
        category: "auction",
        action: "delete-stale-auction",
        role: "pool",
        method: "del_auction",
        help: "delete an auction that has gone 500 ledgers unfilled, which means its creation went wrong and it has to be rebuilt. You supply auction_type and user.",
    },
];

/// Lists every curated guide as category, action, role, live method, and
/// plain-language help including what the caller still has to supply.
/// Read-only: nothing here touches the network.
pub fn catalog() -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)> {
    GUIDES
        .iter()
        .map(|g| (g.category, g.action, g.role, g.method, g.help))
        .collect()
}

/// The guide named by `category` and `action`, or a refusal that points at the
/// catalog without echoing either input.
fn find(category: &str, action: &str) -> anyhow::Result<&'static Guide> {
    GUIDES
        .iter()
        .find(|g| g.category == category && g.action == action)
        .ok_or_else(|| {
            safe_refusal(
                "no guide matches that category and action; list them with the guide catalog",
            )
        })
}

/// The market asset named by an operator selector, checked against the
/// reserved ticker vocabulary. A bad ticker is never echoed.
fn token_of(asset: Option<&str>) -> anyhow::Result<Tok> {
    let ticker = asset.ok_or_else(|| safe_refusal(
        "this guide needs the market asset it acts on; pick one of blnd, usdc, weth, xlm, stable, lp"
    ))?;
    Tok::ALL
        .iter()
        .copied()
        .find(|tok| tok.as_str() == ticker)
        .ok_or_else(|| safe_refusal("that asset is not part of this market's token vocabulary"))
}

/// Bind one selected asset to the token contract frozen in this manifest.
fn bound_token<'a>(manifest: &'a MarketManifest, asset: Option<&str>) -> anyhow::Result<&'a str> {
    let tok = token_of(asset)?;
    Ok(manifest
        .tokens
        .get(&tok)
        .ok_or_else(|| safe_refusal("this market's manifest binds no token for that asset"))?
        .as_str())
}

/// The tagged Stellar asset accepted by the price source, proxy and governance.
fn bound_asset(manifest: &MarketManifest, asset: Option<&str>) -> anyhow::Result<Value> {
    Ok(json!({ "Stellar": bound_token(manifest, asset)? }))
}
/// Manifest-bound asset choices for a recipe. The proxy runtime registry is
/// authoritative for oracle and breaker recipes; other selectors use only
/// token IDs or reserves frozen in this manifest.
pub fn asset_choices<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    category: &str,
    action: &str,
) -> anyhow::Result<Vec<Tok>> {
    match (category, action) {
        ("oracle", "publish-price" | "refresh")
        | ("governance", "propose-manual-trip" | "propose-manual-untrip") => {
            let assets = crate::market_query::query(
                st,
                out,
                manifest,
                "proxy-runtime",
                "registered_assets",
                &[],
            )?;
            let entries = assets
                .as_array()
                .ok_or_else(|| safe_refusal("live registered asset list is malformed"))?;
            let mut choices = Vec::new();
            for entry in entries {
                let Some(object) = entry.as_object().filter(|object| object.len() == 1) else {
                    return Err(safe_refusal("live registered asset list is malformed"));
                };
                let (kind, value) = object.iter().next().expect("one-key object");
                match kind.as_str() {
                    "Stellar" => {
                        let Some(contract) = value.as_str() else {
                            return Err(safe_refusal("live registered asset list is malformed"));
                        };
                        let contract = contract
                            .parse::<crate::stellar::ContractId>()
                            .map_err(|_| safe_refusal("live registered asset list is malformed"))?;
                        if let Some((tok, _)) =
                            manifest.tokens.iter().find(|(_, token_contract)| {
                                token_contract.as_str() == contract.as_str()
                            })
                        {
                            if !choices.contains(tok) {
                                choices.push(*tok);
                            }
                        }
                    }
                    "Other" => {
                        let valid_symbol = value.as_str().is_some_and(|symbol| {
                            !symbol.is_empty()
                                && symbol.len() <= 32
                                && symbol
                                    .bytes()
                                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                        });
                        if !valid_symbol {
                            return Err(safe_refusal("live registered asset list is malformed"));
                        }
                    }
                    _ => return Err(safe_refusal("live registered asset list is malformed")),
                }
            }
            if choices.is_empty() {
                return Err(safe_refusal(
                    "live registry has no manifest-bound assets for this recipe",
                ));
            }
            Ok(choices)
        }
        ("token", _) => Ok(Tok::ALL
            .into_iter()
            .filter(|token| manifest.tokens.contains_key(token))
            .collect()),
        ("pool", "gulp") => Ok(manifest
            .reserves
            .keys()
            .filter(|token| manifest.tokens.contains_key(*token))
            .copied()
            .collect()),
        _ => Ok(Vec::new()),
    }
}

/// Build the tagged governance proposal for one asset's manual breaker
/// trip or untrip from the frozen manifest. Never call the runtime owner
/// directly or apply a blanket change to all reserves.
fn manual_trip(
    manifest: &MarketManifest,
    asset: Option<&str>,
    tripped: bool,
) -> anyhow::Result<String> {
    Ok(
        json!({ "SetManualTrip": [bound_asset(manifest, asset)?, tripped, Value::Null] })
            .to_string(),
    )
}

/// The deployed source selected as `source.<n>`. An index outside the three
/// sources this manifest froze is refused, never rounded or renumbered.
fn source_role(manifest: &MarketManifest, source: Option<usize>) -> anyhow::Result<String> {
    let index = source.ok_or_else(|| {
        safe_refusal("source publication needs the deployed source it acts on: 0, 1 or 2")
    })?;
    if index >= manifest.oracle.sources.len() {
        return Err(safe_refusal(
            "that source index is not one of the sources frozen in this market's manifest",
        ));
    }
    Ok(format!("source.{index}"))
}

/// The argument values a guide binds for itself out of the frozen manifest.
/// Every name returned here is fixed: an operator who tries to supply a
/// different value for one of them is refused, not overridden.
fn bound_args(
    category: &str,
    action: &str,
    manifest: &MarketManifest,
    asset: Option<&str>,
) -> anyhow::Result<Vec<(String, String)>> {
    Ok(match (category, action) {
        ("token", "faucet-mint") => vec![(
            "token".to_string(),
            bound_token(manifest, asset)?.to_string(),
        )],
        ("oracle", "publish-price") | ("oracle", "refresh") => vec![(
            "asset".to_string(),
            bound_asset(manifest, asset)?.to_string(),
        )],
        ("pool", "gulp") => {
            vec![(
                "asset".to_string(),
                bound_token(manifest, asset)?.to_string(),
            )]
        }
        ("governance", "propose-manual-trip") => vec![
            ("caller".to_string(), manifest.admin.as_str().to_string()),
            ("operation".to_string(), manual_trip(manifest, asset, true)?),
            ("requested_ttl".to_string(), "0".to_string()),
        ],
        ("governance", "propose-manual-untrip") => vec![
            ("caller".to_string(), manifest.admin.as_str().to_string()),
            (
                "operation".to_string(),
                manual_trip(manifest, asset, false)?,
            ),
            ("requested_ttl".to_string(), "0".to_string()),
        ],
        ("governance", "execute-proposal") => {
            vec![("caller".to_string(), manifest.admin.as_str().to_string())]
        }
        // Backstop fund movement always concerns this market's own pool.
        ("backstop", "deposit")
        | ("backstop", "queue-withdrawal")
        | ("backstop", "dequeue-withdrawal")
        | ("backstop", "withdraw")
        | ("backstop", "donate")
        | ("backstop", "draw") => vec![(
            "pool_address".to_string(),
            manifest.pool.as_str().to_string(),
        )],
        ("backstop", "claim") => vec![(
            "pool_addresses".to_string(),
            json!([manifest.pool.as_str()]).to_string(),
        )],
        ("backstop", "user-balance" | "pool-data" | "gulp-emissions") => {
            vec![("pool".to_string(), manifest.pool.as_str().to_string())]
        }
        ("backstop", "add-reward") => vec![
            ("to_add".to_string(), manifest.pool.as_str().to_string()),
            ("to_remove".to_string(), Value::Null.to_string()),
        ],
        ("backstop", "remove-reward") => {
            vec![("to_remove".to_string(), manifest.pool.as_str().to_string())]
        }
        // Auction kind and the backstop party are not operator choices.
        ("auction", "create-liquidation") | ("auction", "get-liquidation") => {
            vec![("auction_type".to_string(), "0".to_string())]
        }
        ("auction", "create-bad-debt") => vec![
            ("auction_type".to_string(), "1".to_string()),
            ("user".to_string(), manifest.backstop.as_str().to_string()),
        ],
        ("auction", "create-interest") => vec![
            ("auction_type".to_string(), "2".to_string()),
            ("user".to_string(), manifest.backstop.as_str().to_string()),
        ],
        _ => Vec::new(),
    })
}

/// Host wall-clock seconds, used only as a conservative preflight bound. The
/// ledger is the time authority and the pinned CLI exposes no ledger timestamp,
/// so the disclosure stays attached to the guide.
fn host_seconds() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// Refuses a publication timestamp that cannot survive the source's own replay
/// guard or the proxy's freshness and clock-drift bounds. This is a host-clock
/// preflight and not ledger truth: the on-chain guards are the enforcement and
/// have the last word either way. It fails closed on purpose, because a host
/// clock too far behind to bound the window would otherwise wave through
/// exactly the stale or future value this check exists to stop.
fn check_publication_timestamp(value: &str, manifest: &MarketManifest) -> anyhow::Result<()> {
    let stamp: u64 = value
        .trim()
        .parse()
        .map_err(|_| safe_refusal("a publication timestamp must be a whole number of seconds"))?;
    if stamp == 0 {
        return Err(safe_refusal(
            "a publication timestamp of zero is not a publication time at all",
        ));
    }
    let max_age = u64::from(manifest.oracle.max_age_secs);
    let drift = u64::from(manifest.oracle.max_clock_drift_secs);
    let now = host_seconds()
        .filter(|clock| *clock > max_age)
        .ok_or_else(|| safe_refusal(
            "this host clock is too far behind to bound a publication timestamp; correct it before publishing"
        ))?;
    if stamp > now.saturating_add(drift) {
        return Err(safe_refusal(
            "that publication timestamp is ahead of this host's clock by more than the proxy's clock-drift bound, which the source and the proxy will reject"
        ));
    }
    if stamp < now - max_age {
        return Err(safe_refusal(
            "that publication timestamp is older than the proxy's maximum data age, so the proxy will filter it as stale"
        ));
    }
    Ok(())
}

fn resolve_form<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    category: &str,
    action: &str,
    asset: Option<&str>,
    source: Option<usize>,
) -> anyhow::Result<(&'static Guide, GuideForm, Vec<String>)> {
    let recipe = find(category, action)?;
    let assets = asset_choices(st, out, manifest, category, action)?;
    let requires_asset = recipe.role == ROLE_TOKEN
        || matches!(
            (category, action),
            ("oracle", "publish-price" | "refresh")
                | ("pool", "gulp")
                | ("token", "faucet-mint")
                | (
                    "governance",
                    "propose-manual-trip" | "propose-manual-untrip"
                )
        );
    if requires_asset && assets.is_empty() {
        return Err(safe_refusal(
            "this recipe has no eligible manifest-bound asset choices",
        ));
    }
    let selected_asset = if assets.is_empty() {
        asset.map(str::to_owned)
    } else {
        match asset {
            Some(selected) => {
                let tok = token_of(Some(selected))?;
                if !assets.contains(&tok) {
                    return Err(safe_refusal(
                        "selected asset is not in this recipe's live manifest-bound choices",
                    ));
                }
                Some(selected.to_owned())
            }
            None => Some(
                assets
                    .first()
                    .expect("nonempty recipe choices")
                    .as_str()
                    .to_owned(),
            ),
        }
    };
    let asset = selected_asset.as_deref();
    let role = match recipe.role {
        ROLE_SOURCE => source_role(manifest, source)?,
        ROLE_TOKEN => {
            let tok = token_of(asset)?;
            if !manifest.tokens.contains_key(&tok) {
                return Err(safe_refusal(
                    "this market's manifest binds no token for that asset",
                ));
            }
            format!("token.{}", tok.as_str())
        }
        fixed => fixed.to_string(),
    };
    let spec = market_explorer::inspect(st, out, manifest, &role, None)?;
    let live_spec: Value = serde_json::from_str(&spec.to_string())?;
    let function = live_spec
        .as_array()
        .and_then(|entries| {
            entries.iter().find_map(|entry| {
                let payload = entry.get("function_v0")?;
                (payload.get("name").and_then(Value::as_str) == Some(recipe.method))
                    .then_some(payload)
            })
        })
        .ok_or_else(|| {
            safe_refusal("requested function is not present in the live contract interface")
        })?;
    let live_inputs = function
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| safe_refusal("live contract function has malformed input descriptors"))?;
    let mut bound = bound_args(category, action, manifest, asset)?;
    if category == "governance"
        && (action == "propose-manual-trip" || action == "propose-manual-untrip")
    {
        let next = crate::market_query::query(
            st,
            out,
            manifest,
            "proxy-governance",
            "next_proposal_id",
            &[],
        )?;
        let id = next
            .as_u64()
            .ok_or_else(|| safe_refusal("governance next proposal id is unavailable"))?;
        bound.push(("id".to_string(), id.to_string()));
    }
    let mut inputs = Vec::new();
    for input in live_inputs {
        let name = input.get("name").and_then(Value::as_str).ok_or_else(|| {
            safe_refusal("live contract function has malformed input descriptors")
        })?;
        let ty = input.get("type_").cloned().ok_or_else(|| {
            safe_refusal("live contract function has malformed input descriptors")
        })?;
        if !bound.iter().any(|(fixed, _)| fixed == name) {
            inputs.push((name.to_string(), ty));
        }
    }
    let names = live_inputs
        .iter()
        .filter_map(|input| input.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect();
    Ok((
        recipe,
        GuideForm {
            role,
            method: recipe.method.to_string(),
            spec,
            bound,
            inputs,
            assets,
            selected_asset: asset
                .and_then(|asset| Tok::ALL.into_iter().find(|token| token.as_str() == asset)),
        },
        names,
    ))
}

/// Builds a form from the live recipe, manifest bindings, and current
/// governance proposal id. This is read-only; `guide` remains final authority.
#[allow(clippy::too_many_arguments)]
pub fn form<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    category: &str,
    action: &str,
    asset: Option<&str>,
    source: Option<usize>,
) -> anyhow::Result<GuideForm> {
    resolve_form(st, out, manifest, category, action, asset, source).map(|(_, form, _)| form)
}

/// A complete guided-call target: the manifest role, the exported function
/// it declares, and every named argument in the order the live interface
/// declares them. `guide` hands this shape to the guarded send path, and
/// `market_call` rechecks it immediately before any send.
pub type GuidedCall = (String, String, Vec<(String, String)>);

/// Resolves one curated guide into its manifest role, its exported function,
/// and a complete set of named arguments in the order the live interface
/// declares them.
///
/// The guide validates rather than executes: it re-resolves the target, refuses
/// a function the deployed contract does not export, binds every manifest-owned
/// value, rejects any operator argument that is not a declared input of that
/// exact function or that tries to replace a bound value, and refuses to hand
/// back a partial argument set. Sending belongs to `market_call` alone, which
/// rechecks the target, the live interface, the signer and the network after
/// confirmation and immediately before the pre-send intent is recorded.
// The eight parameters are validated as one atomic guide selection: the
// network seam, run directory, and frozen manifest custody the selection runs
// under, the category/action/asset/source selectors it resolves, and the
// operator-supplied arguments it vets against the live interface. Bundling
// them would scatter this refusal logic across every callsite and hand a
// partially validated selection to `market_call`.
#[allow(clippy::too_many_arguments)]
pub fn guide<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    category: &str,
    action: &str,
    asset: Option<&str>,
    source: Option<usize>,
    explicit: &[(String, String)],
) -> anyhow::Result<GuidedCall> {
    let (recipe, form, live_inputs) =
        resolve_form(st, out, manifest, category, action, asset, source)?;
    let mut supplied: Vec<(String, String)> = Vec::with_capacity(explicit.len());
    for (name, value) in explicit {
        if !live_inputs.iter().any(|input| input == name) {
            return Err(safe_refusal(
                "an argument you supplied is not an input of the guided contract function",
            ));
        }
        if supplied.iter().any(|(seen, _)| seen == name) {
            return Err(safe_refusal("you supplied the same argument twice"));
        }
        if recipe.method == "create_proposal" && name == "id" {
            let current = form
                .bound
                .iter()
                .find(|(fixed, _)| fixed == "id")
                .map(|(_, value)| value.as_str());
            if Some(value.as_str()) != current {
                return Err(safe_refusal(
                    "guided proposal id differs from the live next proposal id",
                ));
            }
            // The matching live id is already bound; keep it for duplicate
            // detection, but never replace the freshly resolved binding.
            supplied.push((name.clone(), value.clone()));
            continue;
        }
        if form.bound.iter().any(|(fixed, _)| fixed == name) {
            return Err(safe_refusal(
                "you supplied a value for an argument this guide binds from the market manifest; drop the override"
            ));
        }
        supplied.push((name.clone(), value.clone()));
    }

    let mut args = Vec::with_capacity(live_inputs.len());
    let mut missing: Vec<&str> = Vec::new();
    for input in &live_inputs {
        match form
            .bound
            .iter()
            .chain(supplied.iter())
            .find(|(name, _)| name == input)
        {
            Some((_, value)) => args.push((input.clone(), value.clone())),
            None => missing.push(input.as_str()),
        }
    }
    if !missing.is_empty() {
        return Err(safe_refusal_owned(format!(
            "the guided call is still missing its {}",
            missing.join(", ")
        )));
    }

    if recipe.method == "set_data" {
        if let Some((_, value)) = args.iter().find(|(name, _)| name == "timestamp") {
            check_publication_timestamp(value, manifest)?;
        }
    }
    Ok((form.role, form.method, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MARKET_TESTNET_PASSPHRASE;
    use crate::market_explorer::attach;
    use crate::stellar::fake::FakeExecutor;
    use crate::stellar::{Net, Policy};
    use std::path::PathBuf;

    const V26: &str = "stellar 26.1.0 (1228cff)";
    const ID_A: &str = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT";
    const ID_B: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
    const ID_C: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
    const ID_D: &str = "CBBTYOXBO7SO3JJ5EM5OBV4WTV5TML4G2K327TYSX422WL47B6RVMJRI";
    const ID_E: &str = "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY";
    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";

    /// Live interface for the positive tests. Names and shapes follow the
    /// CLI26 tagged form the seam requires. `set_data` is present here, so any
    /// guide that needs it can resolve; the absent-method test uses a stripped
    /// interface instead.
    const SPEC: &str = r#"[
      {"function_v0":{"doc":"","name":"set_data","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"price","type_":"i128"},{"doc":"","name":"timestamp","type_":"u64"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"refresh","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"create_proposal","inputs":[{"doc":"","name":"caller","type_":"address"},{"doc":"","name":"id","type_":"u64"},{"doc":"","name":"operation","type_":{"udt":{"name":"Operation"}}},{"doc":"","name":"requested_ttl","type_":"u64"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"registered_assets","inputs":[],"outputs":[{"vec":{"element_type":{"udt":{"name":"Asset"}}}}]}},
      {"function_v0":{"doc":"","name":"next_proposal_id","inputs":[],"outputs":[]}},
      {"function_v0":{"doc":"","name":"transfer","inputs":[{"doc":"","name":"from","type_":"address"},{"doc":"","name":"to","type_":"address"},{"doc":"","name":"amount","type_":"i128"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"balance","inputs":[{"doc":"","name":"id","type_":"address"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"deposit","inputs":[{"doc":"","name":"from","type_":"address"},{"doc":"","name":"pool_address","type_":"address"},{"doc":"","name":"amount","type_":"i128"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"user_balance","inputs":[{"doc":"","name":"pool","type_":"address"},{"doc":"","name":"user","type_":"address"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"pool_data","inputs":[{"doc":"","name":"pool","type_":"address"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"gulp","inputs":[{"doc":"","name":"asset","type_":"address"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"set_emissions_config","inputs":[{"doc":"","name":"res_emission_metadata","type_":{"vec":{"element_type":{"udt":{"name":"ReserveEmissionMetadata"}}}}}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"get_auction","inputs":[{"doc":"","name":"auction_type","type_":"u32"},{"doc":"","name":"user","type_":"address"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"new_auction","inputs":[{"doc":"","name":"auction_type","type_":"u32"},{"doc":"","name":"user","type_":"address"},{"doc":"","name":"bid","type_":{"vec":{"element_type":"address"}}},{"doc":"","name":"lot","type_":{"vec":{"element_type":"address"}}},{"doc":"","name":"percent","type_":"u32"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"drop","inputs":[],"outputs":[]}},
      {"function_v0":{"doc":"","name":"gulp_emissions","inputs":[{"doc":"","name":"pool","type_":"address"}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"add_reward","inputs":[{"doc":"","name":"to_add","type_":"address"},{"doc":"","name":"to_remove","type_":{"option":{"value_type":"address"}}}],"outputs":[]}},
      {"function_v0":{"doc":"","name":"remove_reward","inputs":[{"doc":"","name":"to_remove","type_":"address"}],"outputs":[]}}
    ]"#;

    /// Interface with none of the guided write entrypoints, for the tests that
    /// require a guide to refuse rather than pretend.
    const NO_WRITES_SPEC: &str = r#"[{"function_v0":{"doc":"","name":"balance","inputs":[{"doc":"","name":"id","type_":"address"}],"outputs":[]}}]"#;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abtest-market-guides-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn manifest() -> MarketManifest {
        serde_json::from_value(serde_json::json!({
            "network": {"label": "testnet", "passphrase": MARKET_TESTNET_PASSPHRASE},
            "admin": G_ADMIN,
            "rpc_host": "rpc.testnet.stellar.example",
            "oracle": {
                "runtime": ID_B, "governance": ID_C, "adapter": ID_A,
                "sources": [ID_A, ID_B, ID_C],
                "min_sources": 3, "max_age_secs": 3600, "max_clock_drift_secs": 60,
                "decimals": 7, "resolution": 300, "base": "USD"
            },
            "tokens": {"blnd": ID_E, "usdc": ID_B, "weth": ID_A, "xlm": ID_C, "stable": ID_D},
            "pool": ID_D, "backstop": ID_E, "factory": ID_A,
            "comet": ID_B, "emitter": ID_C,
            "reserves": {}, "identities": {}, "deploy_ledger": 1
        }))
        .expect("fixture market manifest")
    }

    fn net(out: &Path) -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: MARKET_TESTNET_PASSPHRASE.into(),
            config_dir: out.join("cfg"),
            headers: vec![],
        }
    }

    /// The exact-testnet `network info` response every guide selection
    /// re-proves fresh through the CLI seam before any interface read.
    fn network_info_testnet() -> String {
        serde_json::json!({"passphrase": MARKET_TESTNET_PASSPHRASE}).to_string()
    }
    fn choices_for(registry: &str) -> anyhow::Result<Vec<Tok>> {
        let out = scratch("choices");
        let network = net(&out);
        let market = manifest();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(registry, ""),
        ]);
        let st = Stellar::new(&network, &fx, Policy::Market);
        asset_choices(&st, &out, &market, "oracle", "refresh")
    }

    fn form_with_registry(registry: &str, asset: Option<&str>) -> anyhow::Result<GuideForm> {
        let out = scratch("form");
        let network = net(&out);
        let market = manifest();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(registry, ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let st = Stellar::new(&network, &fx, Policy::Market);
        form(&st, &out, &market, "oracle", "refresh", asset, None)
    }

    /// Each selection gets a fresh scripted executor, so an assertion that
    /// something was refused cannot be satisfied by an exhausted script.
    /// Calls are version and testnet probes, live-interface reads, plus an
    /// optional read-only query for manual proposal IDs.
    fn select(
        spec: &str,
        category: &str,
        action: &str,
        asset: Option<&str>,
        source: Option<usize>,
        extra: &[(&str, &str)],
    ) -> (anyhow::Result<GuidedCall>, Vec<Vec<String>>) {
        let out = scratch("select");
        let network = net(&out);
        let mut market = manifest();
        market.reserves.insert(
            Tok::Stable,
            crate::manifest::ReserveRef {
                index: 0,
                decimals: 6,
            },
        );
        let explicit: Vec<(String, String)> = extra
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let needs_registry = category == "oracle"
            || (category == "governance"
                && matches!(action, "propose-manual-trip" | "propose-manual-untrip"));
        let registry = serde_json::json!([
            {"Stellar": ID_A},
            {"Stellar": ID_B},
            {"Stellar": ID_C},
            {"Stellar": ID_D}
        ])
        .to_string();
        let mut replies = Vec::new();
        if needs_registry {
            replies.extend([
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(&network_info_testnet(), ""),
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(SPEC, ""),
                FakeExecutor::ok(&registry, ""),
            ]);
        }
        replies.extend([
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(spec, ""),
        ]);
        if category == "governance"
            && (action == "propose-manual-trip" || action == "propose-manual-untrip")
        {
            replies.extend([
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(&network_info_testnet(), ""),
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(SPEC, ""),
                FakeExecutor::ok("0", ""),
            ]);
        }
        let fx = FakeExecutor::new(replies);
        let result = {
            let st = Stellar::new(&network, &fx, Policy::Market);
            let result = guide(
                &st, &out, &market, category, action, asset, source, &explicit,
            );
            drop(st);
            result
        };
        let calls = fx.calls.borrow().clone();
        (result, calls)
    }

    /// Host wall-clock seconds plus an offset inside the declared clock drift.
    fn stamp(offset: u64) -> String {
        (host_seconds().expect("host clock") + offset).to_string()
    }

    fn publish(
        asset: Option<&str>,
        source: Option<usize>,
        extra: &[(&str, &str)],
    ) -> (anyhow::Result<GuidedCall>, Vec<Vec<String>>) {
        let mut args = vec![("price", "20000000000"), ("timestamp", "0")];
        let dynamic = stamp(5);
        args[1] = ("timestamp", dynamic.as_str());
        args.extend_from_slice(extra);
        select(SPEC, "oracle", "publish-price", asset, source, &args)
    }

    #[test]
    fn the_catalog_covers_every_curated_category_with_unique_actions() {
        let listed = catalog();
        assert!(listed.len() > 20, "the catalog must offer real options");
        for (category, action, _role, method, help) in &listed {
            assert!(
                !method.is_empty() && help.len() > 20,
                "guide {action} needs help"
            );
            assert!(
                [
                    "oracle",
                    "governance",
                    "pool",
                    "token",
                    "backstop",
                    "auction"
                ]
                .contains(category),
                "guide {category}/{action} is outside the curated categories"
            );
        }
        for want in [
            "oracle",
            "governance",
            "pool",
            "token",
            "backstop",
            "auction",
        ] {
            assert!(
                listed.iter().any(|(c, ..)| c == &want),
                "category {want} must offer guides"
            );
        }
        let mut pairs: Vec<(&str, &str)> = listed.iter().map(|(c, a, ..)| (*c, *a)).collect();
        let before = pairs.len();
        pairs.sort_unstable();
        pairs.dedup();
        assert_eq!(before, pairs.len(), "guide names must be unique");
    }

    #[test]
    fn every_catalogued_method_is_one_the_pinned_contracts_declare() {
        // Drift guard on the catalog itself: an offered method has to be an
        // entrypoint the pinned contract sources declare, or an operator is
        // being handed a call the chain cannot honor.
        let declared = [
            "propose_admin",
            "accept_admin",
            "update_pool",
            "queue_set_reserve",
            "cancel_set_reserve",
            "set_reserve",
            "set_status",
            "update_status",
            "submit",
            "submit_with_allowance",
            "flash_loan",
            "gulp",
            "set_emissions_config",
            "claim",
            "bad_debt",
            "new_auction",
            "get_auction",
            "del_auction",
            "deposit",
            "queue_withdrawal",
            "dequeue_withdrawal",
            "withdraw",
            "user_balance",
            "pool_data",
            "backstop_token",
            "reward_zone",
            "distribute",
            "drop",
            "gulp_emissions",
            "add_reward",
            "remove_reward",
            "donate",
            "draw",
            "set_data",
            "refresh",
            "create_proposal",
            "execute_proposal",
            "next_proposal_id",
            "transfer",
            "balance",
            "mint",
        ];
        for (_category, _action, _role, method, _help) in catalog() {
            assert!(
                declared.iter().any(|known| *known == method),
                "the catalog offers {method}, which the pinned contract sources do not declare"
            );
        }
    }

    #[test]
    fn source_publication_binds_the_manifest_asset_and_refuses_a_substitute() {
        let (ok, _calls) = publish(Some("weth"), Some(0), &[]);
        let (role, method, args) = ok.expect("guided publication");
        assert_eq!(role, "source.0");
        assert_eq!(method, "set_data");
        assert_eq!(
            args.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["asset", "price", "timestamp"],
            "arguments must come back in the live interface's own order"
        );
        let bound: serde_json::Value =
            serde_json::from_str(&args[0].1).expect("the bound asset is tagged JSON");
        assert_eq!(bound["Stellar"].as_str(), Some(ID_A));

        // Any other value for a bound argument is refused instead of replacing
        // the manifest binding: a bare contract id, a quoted id, another
        // asset's ticker, and that asset's real contract.
        let quoted = format!("\"{ID_C}\"");
        for impostor in [ID_C, quoted.as_str(), "xlm"] {
            let (err, _) = publish(Some("weth"), Some(0), &[("asset", impostor)]);
            let message = err
                .expect_err("a bound asset must not be overridable")
                .to_string();
            assert!(message.contains("manifest"), "{message}");
        }
        // Pointing the weth guide at the xlm contract is the mismatch itself.
        let (err, _) = publish(Some("xlm"), Some(0), &[("asset", ID_A)]);
        assert!(err.is_err(), "an asset override may not cross tokens");
    }

    #[test]
    fn an_unsupported_asset_or_source_selector_is_refused() {
        for bad in ["teapot", "USDC", "", "btc"] {
            let (err, _) = publish(Some(bad), Some(0), &[]);
            assert!(
                err.is_err(),
                "unsupported asset {bad:?} must be refused, not defaulted"
            );
        }
        for bad in [3usize, 4, 99] {
            let (err, _) = publish(Some("weth"), Some(bad), &[]);
            assert!(err.is_err(), "source {bad} is outside the frozen three");
        }
        // No source at all, and no asset at all, are both refusals.
        assert!(publish(Some("weth"), None, &[]).0.is_err());
        assert!(
            select(SPEC, "token", "transfer", None, None, &[("from", G_ADMIN)])
                .0
                .is_err()
        );
        // `lp` is a valid ticker but this market binds no lp token.
        assert!(select(
            SPEC,
            "token",
            "balance",
            Some("lp"),
            None,
            &[("id", G_ADMIN)]
        )
        .0
        .is_err());
    }

    #[test]
    fn a_method_the_deployed_contract_does_not_export_refuses_the_guide() {
        // `set_data` is absent from this interface, so publication has to
        // refuse rather than pretend to support a method the chain lacks.
        let (err, _) = select(
            NO_WRITES_SPEC,
            "oracle",
            "publish-price",
            Some("weth"),
            Some(0),
            &[("price", "1"), ("timestamp", &stamp(5))],
        );
        let missing_method =
            err.expect_err("publication without a live `set_data` must be refused");
        assert_eq!(
            Safe::market_error(&missing_method).to_string(),
            "requested function is not present in the live contract interface"
        );
        for (category, action, asset, source, extra) in [
            ("oracle", "refresh", Some("weth"), None, vec![]),
            ("backstop", "deposit", None, None, vec![("from", G_ADMIN)]),
            (
                "governance",
                "propose-manual-trip",
                Some("weth"),
                None,
                vec![("id", "0")],
            ),
            (
                "auction",
                "get-liquidation",
                None,
                None,
                vec![("user", G_ADMIN)],
            ),
            (
                "pool",
                "set-emissions-config",
                None,
                None,
                vec![("res_emission_metadata", "[]")],
            ),
        ] {
            let extra: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, *v)).collect();
            let (err, _) = select(NO_WRITES_SPEC, category, action, asset, source, &extra);
            assert!(
                err.is_err(),
                "{category}/{action} must refuse when its method is not exported"
            );
        }
    }

    #[test]
    fn an_unknown_category_or_action_is_refused_without_echoing_input() {
        for (category, action) in [
            ("oracle", "reprice"),
            ("oracles", "refresh"),
            ("governance", "trip"),
            ("nonsense", "anything"),
        ] {
            let (err, _) = select(SPEC, category, action, Some("weth"), Some(0), &[]);
            assert_eq!(
                err.expect_err("an unknown guide must be refused")
                    .to_string(),
                "no guide matches that category and action; list them with the guide catalog"
            );
        }
    }

    #[test]
    fn governance_binds_the_admin_caller_and_the_tagged_trip_operation() {
        let (ok, _) = select(
            SPEC,
            "governance",
            "propose-manual-trip",
            Some("weth"),
            None,
            &[("id", "0")],
        );
        let (role, method, args) = ok.expect("guided proposal");
        assert_eq!(
            (role.as_str(), method.as_str()),
            ("proxy-governance", "create_proposal")
        );
        let map: std::collections::BTreeMap<&str, &str> =
            args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            map["caller"], G_ADMIN,
            "the caller is this manifest's admin"
        );
        assert_eq!(map["requested_ttl"], "0");
        let operation: serde_json::Value =
            serde_json::from_str(map["operation"]).expect("the operation is tagged JSON");
        let trip = operation["SetManualTrip"]
            .as_array()
            .expect("SetManualTrip");
        assert_eq!(trip[0]["Stellar"].as_str(), Some(ID_A));
        assert_eq!(trip[1], serde_json::json!(true));
        assert!(trip[2].is_null());

        let automatic = select(
            SPEC,
            "governance",
            "propose-manual-trip",
            Some("weth"),
            None,
            &[],
        )
        .0
        .expect("current proposal id is filled from the live view");
        assert_eq!(
            automatic.2.iter().find(|(name, _)| name == "id").unwrap().1,
            "0"
        );
        // The untrip guide carries the same shape with the flag cleared.
        let (ok, _) = select(
            SPEC,
            "governance",
            "propose-manual-untrip",
            Some("weth"),
            None,
            &[("id", "0")],
        );
        let untrip = ok.expect("guided untrip");
        let cleared: serde_json::Value = serde_json::from_str(
            &untrip
                .2
                .iter()
                .find(|(k, _)| k == "operation")
                .expect("operation is bound")
                .1,
        )
        .expect("the operation is tagged JSON");
        assert_eq!(cleared["SetManualTrip"][1], serde_json::json!(false));

        // A guessed id, and a redirected caller, are both refused.
        let mismatch = select(
            SPEC,
            "governance",
            "propose-manual-trip",
            Some("weth"),
            None,
            &[("id", "1")],
        )
        .0
        .expect_err("a stale proposal id must be refused");
        assert_eq!(
            Safe::market_error(&mismatch).to_string(),
            "guided proposal id differs from the live next proposal id"
        );
        let duplicate = select(
            SPEC,
            "governance",
            "propose-manual-trip",
            Some("weth"),
            None,
            &[("id", "0"), ("id", "0")],
        )
        .0
        .expect_err("duplicate proposal IDs must be refused");
        assert_eq!(
            Safe::market_error(&duplicate).to_string(),
            "you supplied the same argument twice"
        );
        assert!(select(
            SPEC,
            "governance",
            "propose-manual-trip",
            Some("weth"),
            None,
            &[("id", "0"), ("caller", ID_B)]
        )
        .0
        .is_err());
    }

    #[test]
    fn governance_guides_refuse_when_the_manifest_binds_no_governance_contract() {
        let out = scratch("gov-absent");
        let network = net(&out);
        let mut market = manifest();
        market.oracle.governance = None;
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
        ]);
        let st = Stellar::new(&network, &fx, Policy::Market);
        let err = guide(
            &st,
            &out,
            &market,
            "governance",
            "proposal-count",
            None,
            None,
            &[],
        )
        .expect_err("an unbound governance guide must refuse");
        assert!(
            err.to_string().contains("unknown market role"),
            "the refusal must name the missing role binding: {err}"
        );
    }

    #[test]
    fn form_separates_manifest_bindings_from_live_typed_inputs() {
        let out = scratch("form");
        let network = net(&out);
        let market = manifest();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let recipe_form = {
            let st = Stellar::new(&network, &fx, Policy::Market);
            form(&st, &out, &market, "auction", "create-interest", None, None)
                .expect("live recipe form")
        };
        assert_eq!(
            (recipe_form.role.as_str(), recipe_form.method.as_str()),
            ("pool", "new_auction")
        );
        assert_eq!(
            recipe_form.bound,
            vec![
                ("auction_type".into(), "2".into()),
                ("user".into(), ID_E.into())
            ]
        );
        assert_eq!(
            recipe_form.inputs,
            vec![
                (
                    "bid".into(),
                    serde_json::json!({"vec": {"element_type": "address"}})
                ),
                (
                    "lot".into(),
                    serde_json::json!({"vec": {"element_type": "address"}})
                ),
                ("percent".into(), serde_json::json!("u32"))
            ]
        );
        let out = scratch("proposal-form");
        let network = net(&out);
        let market = manifest();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(&serde_json::json!([{"Stellar": ID_A}]).to_string(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok("7", ""),
        ]);
        let proposal = {
            let st = Stellar::new(&network, &fx, Policy::Market);
            form(
                &st,
                &out,
                &market,
                "governance",
                "propose-manual-trip",
                Some("weth"),
                None,
            )
            .expect("live proposal form")
        };
        assert!(proposal
            .bound
            .iter()
            .any(|(name, value)| name == "id" && value == "7"));
        assert!(!proposal.inputs.iter().any(|(name, _)| name == "id"));
    }

    #[test]
    fn pool_gulp_binds_an_address_without_reusing_oracle_asset_encoding() {
        let (pool, _) = select(SPEC, "pool", "gulp", Some("stable"), None, &[]);
        let (role, method, args) = pool.expect("pinned pool exports gulp(asset: Address)");
        assert_eq!((role.as_str(), method.as_str()), ("pool", "gulp"));
        assert_eq!(
            args,
            vec![("asset".to_string(), ID_D.to_string())],
            "the pool requires its token contract address, not a tagged oracle Asset"
        );
        let (oracle, _) = select(SPEC, "oracle", "refresh", Some("stable"), None, &[]);
        assert_eq!(
            oracle.expect("oracle refresh").2,
            vec![(
                "asset".to_string(),
                serde_json::json!({"Stellar": ID_D}).to_string()
            )],
            "oracle refresh still requires the tagged Asset variant"
        );
    }

    #[test]
    fn backstop_admin_recipes_bind_only_this_markets_pool_and_null_option() {
        let pool = manifest().pool.as_str().to_owned();
        let add = select(SPEC, "backstop", "add-reward", None, None, &[])
            .0
            .expect("add-reward resolves through the live method");
        assert_eq!(
            add.2,
            vec![
                ("to_add".into(), pool.clone()),
                ("to_remove".into(), "null".into())
            ]
        );

        let remove = select(SPEC, "backstop", "remove-reward", None, None, &[])
            .0
            .expect("remove-reward resolves through the live method");
        assert_eq!(remove.2, vec![("to_remove".into(), pool.clone())]);

        let emissions = select(SPEC, "backstop", "gulp-emissions", None, None, &[])
            .0
            .expect("gulp-emissions resolves through the live method");
        assert_eq!(emissions.2, vec![("pool".into(), pool)]);

        let drop = select(SPEC, "backstop", "drop", None, None, &[])
            .0
            .expect("drop resolves through the live method");
        assert!(drop.2.is_empty());
    }

    #[test]
    fn gulp_emissions_form_binds_manifest_pool_and_guide_refuses_redirection() {
        let out = scratch("gulp-emissions-form");
        let network = net(&out);
        let market = manifest();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let (_, resolved, names) = {
            let st = Stellar::new(&network, &fx, Policy::Market);
            resolve_form(&st, &out, &market, "backstop", "gulp-emissions", None, None)
                .expect("live gulp_emissions form")
        };
        assert_eq!(
            resolved.bound,
            vec![("pool".into(), market.pool.as_str().into())]
        );
        assert!(
            resolved.inputs.is_empty(),
            "the manifest-bound pool is not editable"
        );
        assert_eq!(names, vec!["pool".to_string()]);

        let (omitted, _) = select(SPEC, "backstop", "gulp-emissions", None, None, &[]);
        assert_eq!(
            omitted
                .expect("pool is supplied by this market's manifest")
                .2,
            vec![("pool".into(), market.pool.as_str().into())]
        );
        let (redirected, calls) = select(
            SPEC,
            "backstop",
            "gulp-emissions",
            None,
            None,
            &[("pool", ID_A)],
        );
        assert!(
            redirected.is_err(),
            "a caller cannot redirect emissions to another pool"
        );
        assert!(!calls
            .iter()
            .any(|args| args.get(1).is_some_and(|arg| arg == "invoke")));
    }

    #[test]
    fn faucet_mint_binds_the_selected_manifest_token_through_the_attached_faucet() {
        const FAUCET_SPEC: &str = r#"[
          {"function_v0":{"doc":"","name":"mint","inputs":[{"doc":"","name":"token","type_":"address"},{"doc":"","name":"to","type_":"address"},{"doc":"","name":"amount","type_":"i128"}],"outputs":[]}}
        ]"#;
        let out = scratch("faucet-mint");
        let network = net(&out);
        let market = manifest();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(FAUCET_SPEC, ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(FAUCET_SPEC, ""),
        ]);
        let call = {
            let st = Stellar::new(&network, &fx, Policy::Market);
            attach(
                &st,
                &out,
                &market,
                "faucet",
                ID_E.parse().expect("faucet id"),
            )
            .expect("attach faucet");
            guide(
                &st,
                &out,
                &market,
                "token",
                "faucet-mint",
                Some("weth"),
                None,
                &[
                    ("to".to_string(), ID_B.to_string()),
                    ("amount".to_string(), "1000".to_string()),
                ],
            )
        };
        let (role, method, args) = call.expect("guided faucet mint");
        assert_eq!(
            (role.as_str(), method.as_str()),
            ("faucet", "mint"),
            "the mint runs through the attached faucet, not the token"
        );
        assert_eq!(
            args,
            vec![
                ("token".to_string(), ID_A.to_string()),
                ("to".to_string(), ID_B.to_string()),
                ("amount".to_string(), "1000".to_string()),
            ],
            "the manifest token is bound and only to/amount are caller-supplied"
        );
    }

    #[test]
    fn faucet_mint_refuses_when_no_faucet_role_is_attached() {
        let (result, calls) = select(
            SPEC,
            "token",
            "faucet-mint",
            Some("weth"),
            None,
            &[("to", ID_B), ("amount", "1000")],
        );
        assert_eq!(
            result
                .expect_err("an unattached faucet role must refuse the guide")
                .to_string(),
            "unknown market role",
            "the refusal must come from the missing attachment, not a later check"
        );
        assert!(!calls
            .iter()
            .any(|args| args.get(1).is_some_and(|arg| arg == "invoke")));
    }

    #[test]
    fn backstop_views_refuse_to_read_a_different_pool() {
        for (action, user_args) in [
            ("pool-data", vec![]),
            ("user-balance", vec![("user", G_ADMIN)]),
        ] {
            let (call, _) = select(SPEC, "backstop", action, None, None, &user_args);
            assert_eq!(
                call.unwrap()
                    .2
                    .iter()
                    .find(|(name, _)| name == "pool")
                    .unwrap()
                    .1,
                manifest().pool.as_str()
            );
            let mut foreign = user_args;
            foreign.push(("pool", ID_A));
            let (refused, executor) = select(SPEC, "backstop", action, None, None, &foreign);
            assert!(refused.is_err(), "foreign pool overrides must be rejected");
            assert!(!executor
                .iter()
                .any(|args| args.get(1).is_some_and(|arg| arg == "invoke")));
        }
    }

    #[test]
    fn a_publication_timestamp_outside_the_declared_window_is_refused() {
        let now = host_seconds().expect("host clock");
        for bad in [
            (now + 10_000).to_string(),
            (now - 100_000).to_string(),
            "not-a-time".to_string(),
            "".to_string(),
            "-1".to_string(),
            "0".to_string(),
            u64::MAX.to_string(),
        ] {
            let (err, _) = select(
                SPEC,
                "oracle",
                "publish-price",
                Some("weth"),
                Some(0),
                &[("price", "20000000000"), ("timestamp", bad.as_str())],
            );
            let message = err
                .expect_err("an impossible publication timestamp must be refused")
                .to_string();
            assert!(
                message.contains("timestamp"),
                "the refusal must name it: {message}"
            );
        }
        // Inside the window the operator's value is handed through untouched.
        let keep = stamp(1);
        let (ok, _) = select(
            SPEC,
            "oracle",
            "publish-price",
            Some("weth"),
            Some(0),
            &[("price", "20000000000"), ("timestamp", keep.as_str())],
        );
        let args = ok.expect("a current publication timestamp is accepted").2;
        assert_eq!(
            args.iter()
                .find(|(n, _)| n == "timestamp")
                .expect("timestamp argument")
                .1,
            keep,
            "a guided call must not rewrite an operator-supplied timestamp"
        );
    }

    #[test]
    fn stale_and_future_timestamp_refusals_survive_safe_diagnostic_conversion() {
        for (timestamp, expected) in [
            (
                stamp(10_000),
                "that publication timestamp is ahead of this host's clock by more than the proxy's clock-drift bound, which the source and the proxy will reject"
            ),
            (
                (host_seconds().unwrap() - 100_000).to_string(),
                "that publication timestamp is older than the proxy's maximum data age, so the proxy will filter it as stale"
            ),
        ] {
            let (error, _) = select(
                SPEC, "oracle", "publish-price", Some("weth"), Some(0),
                &[("price", "20000000000"), ("timestamp", timestamp.as_str())],
            );
            let error = error.expect_err("out-of-window quote timestamp must be refused");
            assert_eq!(Safe::market_error(&error).to_string(), expected);
        }
    }

    #[test]
    fn emissions_configuration_requires_its_live_vector_input() {
        // The vector is a required input, so no call is ever assembled from a
        // guess, and no invented parameter stands in for it.
        assert!(
            select(SPEC, "pool", "set-emissions-config", None, None, &[])
                .0
                .is_err()
        );
        assert!(select(
            SPEC,
            "pool",
            "set-emissions-config",
            None,
            None,
            &[("pool_status", "0")]
        )
        .0
        .is_err());
    }

    #[test]
    fn an_incomplete_or_invented_argument_set_is_refused() {
        let (err, _) = select(
            SPEC,
            "token",
            "transfer",
            Some("weth"),
            None,
            &[("to", G_ADMIN)],
        );
        let error = err.expect_err("a partial call must not reach the sending path");
        let message = Safe::market_error(&error).to_string();
        assert!(
            message.contains("missing") && message.contains("from"),
            "{message}"
        );

        // A value supplied twice, and a name the function does not declare.
        assert!(select(
            SPEC,
            "token",
            "transfer",
            Some("weth"),
            None,
            &[
                ("from", G_ADMIN),
                ("to", G_ADMIN),
                ("amount", "1"),
                ("amount", "2")
            ]
        )
        .0
        .is_err());
        assert!(select(
            SPEC,
            "token",
            "balance",
            Some("weth"),
            None,
            &[("holder", G_ADMIN)]
        )
        .0
        .is_err());

        // A guide that needs nothing still returns a complete, ordered set.
        let (ok, _) = select(SPEC, "governance", "proposal-count", None, None, &[]);
        let (role, method, args) = ok.expect("the no-argument guide resolves");
        assert_eq!(
            (role.as_str(), method.as_str()),
            ("proxy-governance", "next_proposal_id")
        );
        assert!(args.is_empty());
    }

    #[test]
    fn selecting_a_guide_only_reaches_read_only_cli_seams() {
        for (category, action, asset, source, extra) in [
            ("oracle", "refresh", Some("weth"), None, vec![]),
            (
                "oracle",
                "publish-price",
                Some("weth"),
                Some(1),
                vec![("price", "1".into()), ("timestamp", stamp(5))],
            ),
            (
                "governance",
                "propose-manual-trip",
                Some("weth"),
                None,
                vec![("id", "0".into())],
            ),
            ("governance", "proposal-count", None, None, vec![]),
            (
                "backstop",
                "deposit",
                None,
                None,
                vec![("from", G_ADMIN.into()), ("amount", "1".into())],
            ),
            ("backstop", "pool-data", None, None, vec![]),
            (
                "auction",
                "get-liquidation",
                None,
                None,
                vec![("user", G_ADMIN.into())],
            ),
            (
                "token",
                "balance",
                Some("weth"),
                None,
                vec![("id", G_ADMIN.into())],
            ),
            (
                "pool",
                "set-emissions-config",
                None,
                None,
                vec![("res_emission_metadata", "[]".to_string())],
            ),
        ] {
            let extra: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let (result, calls) = select(SPEC, category, action, asset, source, &extra);
            let _ = result;
            assert!(
                !calls.is_empty(),
                "{category}/{action} must revalidate against the live interface"
            );
            for call in &calls {
                let version_probe = call.len() == 1 && call[0] == "--version";
                let identity_proof = call.len() > 2 && call[0] == "network" && call[1] == "info";
                let interface_read = call.len() > 3
                    && call[0] == "contract"
                    && call[1] == "info"
                    && call[2] == "interface";
                let readonly_query = call.len() > 5
                    && call[0] == "contract"
                    && call[1] == "invoke"
                    && call.iter().any(|arg| arg == "--send")
                    && call.iter().any(|arg| arg == "no");
                assert!(
                    version_probe || identity_proof || interface_read || readonly_query,
                    "a guide may only probe the version, prove exact testnet identity, read an interface, or perform a read-only governance query, saw {call:?}"
                );
                let send = call.iter().position(|arg| arg == "--send");
                assert!(
                    send.is_none()
                        || send
                            .and_then(|index| call.get(index + 1))
                            .is_some_and(|value| value == "no"),
                    "a guide may only invoke with --send no, saw {call:?}"
                );
            }
        }
    }

    #[test]
    fn oracle_choices_follow_live_registry_and_remain_manifest_bound() {
        let registry = serde_json::json!([
            {"Stellar": ID_A},
            {"Stellar": ID_B},
            {"Stellar": ID_C},
            {"Stellar": ID_D}
        ])
        .to_string();
        let choices = choices_for(&registry).expect("valid runtime registry");
        assert_eq!(choices, [Tok::Weth, Tok::Usdc, Tok::Xlm, Tok::Stable]);
        let form = form_with_registry(&registry, None).expect("default live choice");
        assert_eq!(form.assets, choices);
        assert_eq!(form.selected_asset, Some(Tok::Weth));
        let (published, _) = publish(None, Some(0), &[]);
        let (_, _, args) = published.expect("publication defaults from live registry");
        assert!(args.iter().any(|(name, value)| {
            name == "asset" && value == &serde_json::json!({"Stellar": ID_A}).to_string()
        }));
        assert!(form_with_registry(&registry, Some("blnd")).is_err());

        let registered_blnd = serde_json::json!([{"Stellar": ID_E}]).to_string();
        let form =
            form_with_registry(&registered_blnd, None).expect("registered BLND remains eligible");
        assert_eq!(form.assets, [Tok::Blnd]);
        assert_eq!(form.selected_asset, Some(Tok::Blnd));

        for malformed in [
            "[]",
            r#"[{"Stellar":42}]"#,
            r#"[{"Unexpected":"asset"}]"#,
            r#"[{"Stellar":"TOPSECRET"}]"#,
        ] {
            let error = choices_for(malformed).expect_err("malformed registry must fail closed");
            let diagnostic = Safe::market_error(&error).to_string();
            assert!(
                !diagnostic.contains("TOPSECRET"),
                "untrusted registry content must not reach a diagnostic"
            );
            assert!(
                form_with_registry(malformed, None).is_err(),
                "no form may be built from an empty or malformed registry"
            );
        }

        let mut market = manifest();
        market.reserves.insert(
            Tok::Weth,
            crate::manifest::ReserveRef {
                index: 0,
                decimals: 9,
            },
        );
        let out = scratch("local-choices");
        let network = net(&out);
        let fx = FakeExecutor::new(vec![]);
        let st = Stellar::new(&network, &fx, Policy::Market);
        assert_eq!(
            asset_choices(&st, &out, &market, "token", "transfer").unwrap(),
            Tok::ALL
                .into_iter()
                .filter(|token| market.tokens.contains_key(token))
                .collect::<Vec<_>>()
        );
        assert!(asset_choices(&st, &out, &market, "token", "transfer")
            .unwrap()
            .contains(&Tok::Blnd));
        assert_eq!(
            asset_choices(&st, &out, &market, "pool", "gulp").unwrap(),
            [Tok::Weth]
        );
        let empty_reserves = manifest();
        assert!(super::form(
            &st,
            &out,
            &empty_reserves,
            "pool",
            "gulp",
            Some("weth"),
            None
        )
        .is_err());
        let mut empty_tokens = manifest();
        empty_tokens.tokens.clear();
        assert!(super::form(
            &st,
            &out,
            &empty_tokens,
            "token",
            "balance",
            Some("blnd"),
            None
        )
        .is_err());
        assert!(fx.calls.borrow().is_empty());
        let (token, _) = select(
            SPEC,
            "token",
            "balance",
            Some("blnd"),
            None,
            &[("id", G_ADMIN)],
        );
        assert_eq!(token.expect("manifest BLND token action").0, "token.blnd");
    }
}
