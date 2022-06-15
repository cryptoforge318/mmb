use crate::exchanges::common::{Amount, ExchangeId, MarketId, Price};
use crate::misc::time::time_manager;
use crate::orders::order::{ExchangeOrderId, OrderSide};
use mmb_database::postgres_db::events::{Event, TableName};
use mmb_utils::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransactionTradeDirection {
    Target,
    Hedge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionTrade {
    pub exchange_order_id: ExchangeOrderId,
    pub exchange_id: ExchangeId,
    pub price: Option<Price>,
    pub amount: Amount,
}

pub type TransactionId = Uuid;

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum TransactionStatus {
    /// when got postponed fill
    New,

    /// when starting hedging
    Hedging,

    /// when waiting trailing stop
    Trailing,

    /// if hedging stopped by timeout
    Timeout,

    /// when stop loss completed
    StopLoss,

    /// if order successfully hedged or catch exception
    Finished,
}

impl TransactionStatus {
    pub fn is_finished(&self) -> bool {
        use TransactionStatus::*;
        matches!(self, StopLoss | Finished)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionSnapshot {
    revision: u64,
    transaction_id: TransactionId,
    transaction_creation_time: DateTime,
    pub market_id: MarketId,
    pub side: OrderSide,
    pub price: Option<Price>,
    pub amount: Amount,
    pub status: TransactionStatus,
    // name of strategy for showing on website
    pub strategy_name: String,
    pub hedged: Option<Amount>,
    pub profit_loss_pct: Option<Amount>,
    pub trades: Vec<TransactionTrade>,
}

impl TransactionSnapshot {
    pub fn new(
        market_id: MarketId,
        side: OrderSide,
        price: Option<Price>,
        amount: Amount,
        status: TransactionStatus,
        strategy_name: String,
    ) -> Self {
        TransactionSnapshot {
            revision: 1,
            transaction_id: Uuid::new_v4(),
            transaction_creation_time: time_manager::now(),
            market_id,
            side,
            price,
            amount,
            status,
            strategy_name,
            hedged: None,
            profit_loss_pct: None,
            trades: vec![],
        }
    }

    pub fn revisions(&self) -> u64 {
        self.revision
    }

    pub fn increment_revision(&mut self) {
        self.revision += 1;
    }

    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    pub fn creation_time(&self) -> DateTime {
        self.transaction_creation_time
    }
}

impl Event for &mut TransactionSnapshot {
    fn get_table_name(&self) -> TableName {
        "transactions"
    }

    fn get_json(&self) -> serde_json::Result<Value> {
        serde_json::to_value(self)
    }
}

pub mod transaction_service {
    use crate::database::events::transaction::{TransactionSnapshot, TransactionStatus};
    use crate::database::events::EventRecorder;
    use anyhow::Context;

    pub fn save(
        transaction: &mut TransactionSnapshot,
        status: TransactionStatus,
        event_recorder: &EventRecorder,
    ) -> anyhow::Result<()> {
        transaction.status = status;
        transaction.increment_revision();

        event_recorder
            .save(transaction)
            .context("in transaction_service::save()")
    }
}
