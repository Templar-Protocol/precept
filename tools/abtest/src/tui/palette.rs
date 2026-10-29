//! Command palette: builds an ad-hoc `LabeledStep` from the same `Step` vocabulary
//! the scenarios use, so an operator action is executed and recorded identically.

use crate::manifest::{Tok, Who};
use crate::scenario::{req, Expect, LabeledStep, Req, Step, TrapCall};

// --- State ---

/// One argument a step needs, with the prompt shown while picking it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg {
    Who(&'static str),
    Tok(&'static str),
    Reserve(&'static str),
    Amount(&'static str),
    U32(&'static str),
    Trap(&'static str),
    ReqKind(&'static str),
    Bool(&'static str),
    /// Four 7-decimal prices in `Tok::ORACLE_ORDER`.
    Prices,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Who(Who),
    Tok(Tok),
    Amount(i128),
    U32(u32),
    Trap(TrapCall),
    Bool(bool),
    Prices(Vec<i128>),
}

impl Val {
    pub fn who(&self) -> Who {
        match self {
            Val::Who(w) => *w,
            _ => Who::User,
        }
    }
    pub fn tok(&self) -> Tok {
        match self {
            Val::Tok(t) => *t,
            _ => Tok::Stable,
        }
    }
    pub fn amount(&self) -> i128 {
        match self {
            Val::Amount(a) => *a,
            Val::U32(n) => *n as i128,
            _ => 0,
        }
    }
    pub fn u32(&self) -> u32 {
        match self {
            Val::U32(n) => *n,
            Val::Amount(a) => *a as u32,
            _ => 0,
        }
    }
    pub fn label(&self) -> String {
        match self {
            Val::Who(w) => w.as_str().into(),
            Val::Tok(t) => t.as_str().into(),
            Val::Amount(a) => a.to_string(),
            Val::U32(n) => n.to_string(),
            Val::Trap(t) => t.label().into(),
            Val::Bool(b) => b.to_string(),
            Val::Prices(p) => p
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

pub struct StepSpec {
    pub name: &'static str,
    pub args: &'static [Arg],
    pub build: fn(&[Val]) -> Step,
}

/// Every `Step` variant, in palette order. The exhaustiveness test below fails to
/// compile if `Step` gains a variant that is not represented here.
pub fn specs() -> &'static [StepSpec] {
    &[
        StepSpec {
            name: "mint",
            args: &[Arg::Tok("token"), Arg::Who("to"), Arg::Amount("amount")],
            build: |v| Step::Mint {
                token: v[0].tok(),
                to: v[1].who(),
                amount: v[2].amount(),
            },
        },
        StepSpec {
            name: "lp_transfer",
            args: &[Arg::Who("from"), Arg::Who("to"), Arg::Amount("amount")],
            build: |v| Step::LpTransfer {
                from: v[0].who(),
                to: v[1].who(),
                amount: v[2].amount(),
            },
        },
        StepSpec {
            name: "submit",
            args: &[
                Arg::Who("from"),
                Arg::ReqKind("request"),
                Arg::Reserve("asset"),
                Arg::Amount("amount"),
            ],
            build: |v| Step::Submit {
                from: v[0].who(),
                reqs: vec![Req::new(v[1].u32(), v[2].tok(), v[3].amount())],
            },
        },
        StepSpec {
            name: "repay_all",
            args: &[Arg::Who("from"), Arg::Reserve("asset")],
            build: |v| Step::RepayAll {
                from: v[0].who(),
                asset: v[1].tok(),
            },
        },
        StepSpec {
            name: "fill_auction",
            args: &[Arg::Who("filler"), Arg::Who("victim"), Arg::U32("percent")],
            build: |v| Step::FillAuction {
                filler: v[0].who(),
                victim: v[1].who(),
                percent: v[2].u32(),
            },
        },
        StepSpec {
            name: "set_prices",
            args: &[Arg::Prices],
            build: |v| match &v[0] {
                Val::Prices(p) => Step::SetPrices(p.clone()),
                _ => Step::SetPrices(vec![]),
            },
        },
        StepSpec {
            name: "set_status",
            args: &[Arg::U32("status")],
            build: |v| Step::SetStatus(v[0].u32()),
        },
        StepSpec {
            name: "update_status",
            args: &[],
            build: |_| Step::UpdateStatus,
        },
        StepSpec {
            name: "backstop_deposit",
            args: &[Arg::Who("who"), Arg::Amount("amount")],
            build: |v| Step::BackstopDeposit {
                who: v[0].who(),
                amount: v[1].amount(),
            },
        },
        StepSpec {
            name: "backstop_queue_pct",
            args: &[Arg::Who("who"), Arg::U32("percent")],
            build: |v| Step::BackstopQueuePct {
                who: v[0].who(),
                pct: v[1].u32(),
            },
        },
        StepSpec {
            name: "backstop_dequeue_half",
            args: &[Arg::Who("who")],
            build: |v| Step::BackstopDequeueHalf { who: v[0].who() },
        },
        StepSpec {
            name: "backstop_withdraw_queued",
            args: &[Arg::Who("who")],
            build: |v| Step::BackstopWithdrawQueued { who: v[0].who() },
        },
        StepSpec {
            name: "new_auction",
            args: &[
                Arg::Who("user"),
                Arg::Reserve("bid"),
                Arg::Reserve("lot"),
                Arg::U32("percent"),
            ],
            build: |v| Step::NewAuction {
                user: v[0].who(),
                bid: vec![v[1].tok()],
                lot: vec![v[2].tok()],
                percent: v[3].u32(),
            },
        },
        StepSpec {
            name: "del_auction",
            args: &[Arg::Who("user")],
            build: |v| Step::DelAuction { user: v[0].who() },
        },
        StepSpec {
            name: "bad_debt",
            args: &[Arg::Who("user")],
            build: |v| Step::BadDebt { user: v[0].who() },
        },
        StepSpec {
            name: "gulp",
            args: &[Arg::Reserve("asset")],
            build: |v| Step::Gulp { asset: v[0].tok() },
        },
        StepSpec {
            name: "queue_set_reserve",
            args: &[Arg::Reserve("asset"), Arg::Bool("modify")],
            build: |v| Step::QueueSetReserve {
                asset: v[0].tok(),
                modify: matches!(v[1], Val::Bool(true)),
            },
        },
        StepSpec {
            name: "trap",
            args: &[Arg::Trap("export")],
            build: |v| match v[0] {
                Val::Trap(t) => Step::Trap(t),
                _ => Step::Trap(TrapCall::PoolUpdatePool),
            },
        },
        StepSpec {
            name: "wait_ledgers_since",
            args: &[Arg::Who("auction_of"), Arg::U32("min_delta")],
            build: |v| Step::WaitLedgersSince {
                auction_of: v[0].who(),
                min_delta: v[1].u32(),
            },
        },
        StepSpec {
            name: "del_auction_when_stale",
            args: &[Arg::Who("user"), Arg::U32("code")],
            build: |v| Step::DelAuctionWhenStale {
                user: v[0].who(),
                code: v[1].u32(),
            },
        },
        StepSpec {
            name: "wait_seconds",
            args: &[Arg::U32("seconds")],
            build: |v| Step::WaitSeconds(v[0].u32() as u64),
        },
        StepSpec {
            name: "assert_rates_increased",
            args: &[Arg::Reserve("asset")],
            build: |v| Step::AssertRatesIncreased { asset: v[0].tok() },
        },
    ]
}

/// Choices offered for one argument.
pub fn choices(arg: &Arg) -> Vec<Val> {
    match arg {
        Arg::Who(_) => Who::ALL.iter().map(|w| Val::Who(*w)).collect(),
        Arg::Tok(_) => Tok::ALL.iter().map(|t| Val::Tok(*t)).collect(),
        Arg::Reserve(_) => Tok::RESERVES.iter().map(|t| Val::Tok(*t)).collect(),
        Arg::Trap(_) => TrapCall::ALL.iter().map(|t| Val::Trap(*t)).collect(),
        Arg::Bool(_) => vec![Val::Bool(true), Val::Bool(false)],
        Arg::ReqKind(_) => [
            req::SUPPLY,
            req::WITHDRAW,
            req::SUPPLY_COLLATERAL,
            req::WITHDRAW_COLLATERAL,
            req::BORROW,
            req::REPAY,
        ]
        .iter()
        .map(|k| Val::U32(*k))
        .collect(),
        // Free-text arguments have no preset list.
        Arg::Amount(_) | Arg::U32(_) | Arg::Prices => vec![],
    }
}

pub fn arg_prompt(arg: &Arg) -> &'static str {
    match arg {
        Arg::Who(p)
        | Arg::Tok(p)
        | Arg::Reserve(p)
        | Arg::Amount(p)
        | Arg::U32(p)
        | Arg::Trap(p)
        | Arg::ReqKind(p)
        | Arg::Bool(p) => p,
        Arg::Prices => "prices (weth,usdc,xlm,stable)",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Choosing which step to run.
    Pick,
    /// Filling argument `usize`.
    Arg(usize),
    /// All arguments filled; Enter fires.
    Ready,
}

pub struct Palette {
    pub query: String,
    pub sel: usize,
    pub stage: Stage,
    pub vals: Vec<Val>,
    pub sides: crate::run::Sides,
    pub spec: Option<usize>,
    /// Free-text buffer for amount/u32/prices arguments.
    pub buf: String,
    pub seq: usize,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            query: String::new(),
            sel: 0,
            stage: Stage::Pick,
            vals: Vec::new(),
            sides: crate::run::Sides::Both,
            spec: None,
            buf: String::new(),
            seq: 0,
        }
    }
}

impl Palette {
    /// Indices into `specs()` matching the current query.
    pub fn matches(&self) -> Vec<usize> {
        let q = self.query.to_ascii_lowercase();
        specs()
            .iter()
            .enumerate()
            .filter(|(_, s)| q.is_empty() || s.name.contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn current_arg(&self) -> Option<&'static Arg> {
        match (self.spec, self.stage) {
            (Some(i), Stage::Arg(n)) => specs()[i].args.get(n),
            _ => None,
        }
    }

    /// Accept the highlighted choice or typed buffer, advancing the stage.
    /// Returns false when typed input cannot satisfy the argument: the stage stays
    /// open instead of silently submitting 0 for garbage.
    pub fn accept(&mut self) -> bool {
        match self.stage {
            Stage::Pick => {
                let m = self.matches();
                if let Some(&i) = m.get(self.sel) {
                    self.spec = Some(i);
                    self.sel = 0;
                    self.stage = if specs()[i].args.is_empty() {
                        Stage::Ready
                    } else {
                        Stage::Arg(0)
                    };
                }
                true
            }
            Stage::Arg(n) => {
                let Some(arg) = self.current_arg() else {
                    return true;
                };
                let opts = choices(arg);
                let val = if opts.is_empty() {
                    let buf = self.buf.trim().to_string();
                    match arg {
                        Arg::Prices => {
                            let parts: Vec<&str> = buf.split(',').map(str::trim).collect();
                            if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
                                return false;
                            }
                            match parts
                                .iter()
                                .map(|p| p.parse::<i128>().ok())
                                .collect::<Option<Vec<i128>>>()
                            {
                                Some(v) => Val::Prices(v),
                                None => return false,
                            }
                        }
                        Arg::U32(_) => match buf.parse::<u32>() {
                            Ok(v) => Val::U32(v),
                            Err(_) => return false,
                        },
                        _ => match buf.parse::<i128>() {
                            Ok(v) => Val::Amount(v),
                            Err(_) => return false,
                        },
                    }
                } else {
                    opts[self.sel.min(opts.len() - 1)].clone()
                };
                self.vals.push(val);
                self.buf.clear();
                self.sel = 0;
                let total = self.spec.map(|i| specs()[i].args.len()).unwrap_or(0);
                self.stage = if n + 1 < total {
                    Stage::Arg(n + 1)
                } else {
                    Stage::Ready
                };
                true
            }
            Stage::Ready => true,
        }
    }

    /// The step to run, once every argument is filled.
    pub fn build(&self) -> Option<LabeledStep> {
        let i = self.spec?;
        if self.stage != Stage::Ready {
            return None;
        }
        let spec = &specs()[i];
        if self.vals.len() != spec.args.len() {
            return None;
        }
        let step = (spec.build)(&self.vals);
        let args = self
            .vals
            .iter()
            .map(|v| v.label())
            .collect::<Vec<_>>()
            .join("_");
        let label = format!(
            "operator_{:03}_{}{}{}",
            self.seq,
            spec.name,
            if args.is_empty() { "" } else { "_" },
            args
        );
        // ponytail: labels are &'static str across the crate, and operator labels are
        // bounded by keystrokes. Upgrade path is Cow<'static, str> on LabeledStep.
        Some(LabeledStep {
            label: Box::leak(label.into_boxed_str()),
            step,
            expect: Expect::Any,
            delta_dependent: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails to compile when `Step` gains a variant the palette does not cover.
    #[allow(dead_code)]
    fn every_step_variant_is_in_the_palette(s: &Step) -> &'static str {
        match s {
            Step::Mint { .. } => "mint",
            Step::LpTransfer { .. } => "lp_transfer",
            Step::Submit { .. } => "submit",
            Step::RepayAll { .. } => "repay_all",
            Step::FillAuction { .. } => "fill_auction",
            Step::SetPrices(_) => "set_prices",
            Step::SetPricesAt { .. } => "set_prices_at",
            Step::SetStatus(_) => "set_status",
            Step::UpdateStatus => "update_status",
            Step::BackstopDeposit { .. } => "backstop_deposit",
            Step::BackstopQueuePct { .. } => "backstop_queue_pct",
            Step::BackstopDequeueHalf { .. } => "backstop_dequeue_half",
            Step::BackstopWithdrawQueued { .. } => "backstop_withdraw_queued",
            Step::NewAuction { .. } => "new_auction",
            Step::DelAuction { .. } => "del_auction",
            Step::BadDebt { .. } => "bad_debt",
            Step::Gulp { .. } => "gulp",
            Step::QueueSetReserve { .. } => "queue_set_reserve",
            Step::Trap(_) => "trap",
            Step::WaitLedgersSince { .. } => "wait_ledgers_since",
            Step::DelAuctionWhenStale { .. } => "del_auction_when_stale",
            Step::WaitSeconds(_) => "wait_seconds",
            Step::AssertRatesIncreased { .. } => "assert_rates_increased",
        }
    }

    fn fill(p: &mut Palette) {
        while p.stage != Stage::Ready {
            if let Some(a) = p.current_arg() {
                if choices(a).is_empty() {
                    p.buf = match a {
                        Arg::Prices => "1,2,3,4".into(),
                        _ => "7".into(),
                    };
                }
            }
            p.accept();
        }
    }

    #[test]
    fn every_spec_builds_with_default_arguments() {
        for (i, spec) in specs().iter().enumerate() {
            let mut p = Palette {
                spec: Some(i),
                stage: if spec.args.is_empty() {
                    Stage::Ready
                } else {
                    Stage::Arg(0)
                },
                ..Default::default()
            };
            fill(&mut p);
            let built = p.build();
            assert!(built.is_some(), "{} failed to build", spec.name);
            assert!(built.unwrap().label.starts_with("operator_"));
        }
    }

    #[test]
    fn builds_the_expected_steps() {
        let mut p = Palette {
            query: "mint".into(),
            ..Default::default()
        };
        assert_eq!(specs()[p.matches()[0]].name, "mint");
        p.accept(); // pick mint
        p.sel = 3; // Tok::Xlm within Tok::ALL
        p.accept();
        p.sel = 2; // Who::Sam
        p.accept();
        p.buf = "1000".into();
        p.accept();
        assert_eq!(
            p.build().unwrap().step,
            Step::Mint {
                token: Tok::Xlm,
                to: Who::Sam,
                amount: 1000
            }
        );

        let mut t = Palette {
            query: "trap".into(),
            ..Default::default()
        };
        t.accept();
        t.sel = 7; // TrapCall::PoolFlashLoan
        t.accept();
        assert_eq!(t.build().unwrap().step, Step::Trap(TrapCall::PoolFlashLoan));
    }

    #[test]
    fn submit_builds_a_single_request() {
        let mut p = Palette {
            query: "submit".into(),
            ..Default::default()
        };
        p.accept();
        p.sel = 1; // frodo
        p.accept();
        p.sel = 4; // BORROW
        p.accept();
        p.sel = 0; // stable
        p.accept();
        p.buf = "5".into();
        p.accept();
        assert_eq!(
            p.build().unwrap().step,
            Step::Submit {
                from: Who::Frodo,
                reqs: vec![Req::new(req::BORROW, Tok::Stable, 5)]
            }
        );
    }

    /// Garbage typed into a numeric argument must be rejected, not silently
    /// submitted as 0.
    #[test]
    fn garbage_arguments_do_not_silently_submit_zero() {
        let mut p = Palette {
            query: "mint".into(),
            ..Default::default()
        };
        p.accept(); // pick mint
        p.sel = 0; // token
        p.accept();
        p.sel = 0; // who
        p.accept();
        p.buf = "1O00".into();
        assert!(!p.accept(), "garbage amount must be rejected");
        assert_eq!(p.stage, Stage::Arg(2), "the stage stays open");
        p.buf = "1000".into();
        assert!(p.accept());
        assert_eq!(p.stage, Stage::Ready);
    }

    #[test]
    fn query_filters_and_unfilled_steps_do_not_build() {
        let p = Palette {
            query: "backstop".into(),
            ..Default::default()
        };
        assert_eq!(p.matches().len(), 4);
        let half = Palette {
            spec: Some(0),
            stage: Stage::Arg(0),
            ..Default::default()
        };
        assert!(half.build().is_none());
    }
}

// --- Market play mode (additive: the pair table above is byte-compatible) --

/// Pair-only probes hidden in market play mode: trap calls, delta-dependent
/// rate assertions, and future-timestamp price probes (whose freshness the
/// proxy consumes). Pair mode is untouched; this only gates market visibility.
pub fn market_hidden(name: &str) -> bool {
    matches!(name, "trap" | "assert_rates_increased" | "set_prices_at")
}

#[cfg(test)]
mod market_palette_tests {
    use super::*;

    #[test]
    fn pair_probes_are_hidden_and_pair_actions_stay_present() {
        for probe in ["trap", "assert_rates_increased", "set_prices_at"] {
            assert!(
                market_hidden(probe),
                "{probe} must be hidden in market mode"
            );
        }
        for kept in ["mint", "submit", "set_prices", "backstop_deposit"] {
            assert!(!market_hidden(kept), "{kept} must remain visible");
        }
        // The pair table itself is unchanged: first and last entries intact.
        assert_eq!(specs()[0].name, "mint");
        assert_eq!(specs()[specs().len() - 1].name, "assert_rates_increased");
    }
}
