#![no_std]
//! # Pool pause policy
//!
//! A stateless OpenZeppelin smart-account [`Policy`] that lets a context rule
//! call `set_status(2)` (On-Ice) on the pool the rule is scoped to, and
//! nothing else. Attach it to a `CallContract(pool)` rule whose signer is the
//! pause key. One matched signer is enough for this policy. The rule also
//! carries a `simple_threshold` policy with threshold 1, which authorization
//! does not need: the multisig tooling reads the rule's threshold from it, and
//! the keeper extends that policy's storage TTLs along with the rule's.
//!
//! By the time the smart account calls [`Policy::enforce`], it has
//! authenticated the rule's signers and matched the auth context to the rule.
//! This policy then refuses the call unless all of these hold:
//!
//! - at least one of the rule's signers authenticated;
//! - the rule is `CallContract(pool)` (`install` already refuses to install
//!   on a `Default`, `CreateContract` or `CreateContractExternalRef` rule;
//!   `enforce` keeps the same check as the fail-closed backstop);
//! - the context is a call to that `pool`'s `set_status` with exactly one
//!   argument, the `u32` `2`.
//!
//! `set_status` is 10 characters, one more than `symbol_short!` accepts, so
//! the name is built with `Symbol::new`.
//!
//! The policy keeps no state and takes no install parameters.
//!
//! A refusal traps as `Error(Contract, #1)`. The pool's
//! `Error::InternalError = 1` (`pool/src/errors.rs:10`) has the same code, so
//! in the diagnostics of a failed pause only the contract id on the event
//! tells them apart.
use soroban_sdk::{
    auth::{Context, ContractContext},
    contract, contracterror, contractimpl, panic_with_error, Address, Env, Symbol, TryFromVal, Vec,
};
use stellar_accounts::{
    policies::Policy,
    smart_account::{ContextRule, ContextRuleType, Signer},
};

/// The only pool status this policy authorizes setting.
const ON_ICE: u32 = 2;

#[contract]
pub struct PoolPausePolicy;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PauseError {
    /// The authorized call is not `set_status(2)` on the rule's pool.
    NotAllowed = 1,
}

#[contractimpl]
impl Policy for PoolPausePolicy {
    type AccountParams = ();

    fn enforce(
        e: &Env,
        context: Context,
        authenticated_signers: Vec<Signer>,
        context_rule: ContextRule,
        smart_account: Address,
    ) {
        smart_account.require_auth();
        if authenticated_signers.is_empty() {
            panic_with_error!(e, PauseError::NotAllowed);
        }
        let pool = match context_rule.context_type {
            ContextRuleType::CallContract(pool) => pool,
            // A Default, CreateContract or CreateContractExternalRef rule has
            // no pool to compare against.
            _ => panic_with_error!(e, PauseError::NotAllowed),
        };
        match context {
            Context::Contract(ContractContext {
                contract,
                fn_name,
                args,
            }) if contract == pool
                && fn_name == Symbol::new(e, "set_status")
                && args.len() == 1
                && matches!(u32::try_from_val(e, &args.get_unchecked(0)), Ok(ON_ICE)) => {}
            _ => panic_with_error!(e, PauseError::NotAllowed),
        }
    }

    fn install(
        e: &Env,
        _install_params: Self::AccountParams,
        context_rule: ContextRule,
        _smart_account: Address,
    ) {
        // A rule that is not CallContract(pool) can never authorize anything
        // through this policy, so reject it at add_context_rule time instead
        // of at the pause that needs it. enforce keeps the same check as the
        // fail-closed backstop.
        if !matches!(context_rule.context_type, ContextRuleType::CallContract(_)) {
            panic_with_error!(e, PauseError::NotAllowed);
        }
    }

    fn uninstall(_e: &Env, _context_rule: ContextRule, _smart_account: Address) {}
}

#[cfg(test)]
mod test;
