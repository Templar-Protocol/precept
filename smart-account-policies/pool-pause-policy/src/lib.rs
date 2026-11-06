#![no_std]
//! # Pool pause policy
//!
//! A stateless OpenZeppelin smart-account [`Policy`] that lets a context rule
//! do exactly one thing: call `set_status(2)` (On-Ice) on the pool the rule is
//! scoped to. Attach it to a `CallContract(pool)` rule whose signer is the
//! pause key; the rule's other policy (`simple_threshold`, threshold 1) makes
//! that single signer sufficient.
//!
//! The smart account has already authenticated the rule's signers and matched
//! the auth context to the rule when it calls [`Policy::enforce`]. This policy
//! then pins the call itself, and refuses anything else:
//!
//! - at least one of the rule's signers authenticated;
//! - the rule is `CallContract(pool)` (a `Default`, `CreateContract` or
//!   `CreateContractExternalRef` rule is refused outright);
//! - the context is a contract call to that `pool`, to `set_status`, with
//!   exactly one argument that is the `u32` `2`.
//!
//! The function name is built with `Symbol::new`, not `symbol_short!`:
//! `set_status` is 10 characters and `symbol_short!` accepts at most 9.
//!
//! There is no state, no installation parameter and nothing to uninstall.
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
        _e: &Env,
        _install_params: Self::AccountParams,
        _context_rule: ContextRule,
        _smart_account: Address,
    ) {
    }

    fn uninstall(_e: &Env, _context_rule: ContextRule, _smart_account: Address) {}
}

#[cfg(test)]
mod test;
