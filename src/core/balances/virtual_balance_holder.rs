use std::collections::HashMap;

use crate::core::balance_manager::balance_request::BalanceRequest;
use crate::core::exchanges::common::{CurrencyCode, ExchangeAccountId};
use crate::core::misc::service_value_tree::ServiceValueTree;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

type BalanceByExchangeId = HashMap<ExchangeAccountId, HashMap<CurrencyCode, Decimal>>;

pub(crate) struct VirtualBalanceHolder {
    balance_by_exchange_id: BalanceByExchangeId,
    balance_diff: ServiceValueTree,
}

impl VirtualBalanceHolder {
    pub fn update_balances(
        &mut self,
        exchange_account_id: &ExchangeAccountId,
        balances_by_currency_code: &HashMap<CurrencyCode, Decimal>,
    ) {
        self.balance_by_exchange_id.insert(
            exchange_account_id.clone(),
            balances_by_currency_code.clone(),
        );

        log::info!(
            "VirtualBalanceHolder UpdateBalances {} {:?}",
            exchange_account_id,
            balances_by_currency_code
        );

        let all_diffs = self.balance_diff.get_as_balances();

        for currency_code in balances_by_currency_code.keys() {
            let balance_requests_to_clear = all_diffs.keys().map(|x| {
                if &x.exchange_account_id == exchange_account_id
                    && x.currency_code == currency_code.clone()
                {
                    return Some(x);
                }
                None
            });

            for balance_request in balance_requests_to_clear {
                match balance_request {
                    Some(balance_request) => {
                        self.balance_diff
                            .set_by_balance_request(balance_request, dec!(0));
                        log::info!(
                            "VirtualBalanceHolder update_balances Reset {} {}",
                            balance_request.exchange_account_id,
                            balance_request.currency_code
                        );
                    }
                    None => (),
                }
            }
        }
    }

    pub fn add_balance(
        &mut self,
        balance_request: &BalanceRequest,
        balance_to_add: Decimal,
        member_name: Option<String>,
    ) {
        let current_diff_value = self
            .balance_diff
            .get_by_balance_request(balance_request)
            .unwrap_or(dec!(0));
        let new_value = current_diff_value + balance_to_add;
        self.balance_diff
            .set_by_balance_request(balance_request, new_value);

        log::info!(
            "VirtualBalanceHolder add_balance {} {} {} {} {} {} {}",
            member_name.unwrap_or(String::from("")),
            balance_request.exchange_account_id,
            balance_request.currency_pair,
            balance_request.currency_code,
            current_diff_value,
            balance_to_add,
            new_value
        );
    }
}
