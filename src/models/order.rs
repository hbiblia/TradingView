use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    Market,
    Limit,
    StopLoss,
    StopLossLimit,
    TakeProfit,
    TakeProfitLimit,
    LimitMaker,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    PendingCancel,
    Rejected,
    Expired,
}

/// Respuesta de Binance al crear una orden
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub symbol: String,
    pub order_id: u64,
    pub client_order_id: String,
    pub transact_time: u64,
    pub price: String,
    pub orig_qty: String,
    pub executed_qty: String,
    pub cummulative_quote_qty: String,
    pub status: OrderStatus,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
}

/// Registro interno de una operación DCA
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DcaTrade {
    pub order_id: u64,
    pub buy_price: f64,
    pub quantity: f64,  // cantidad base (ej: BTC)
    pub cost: f64,      // costo total en quote (ej: USDT)
    pub timestamp: DateTime<Utc>,
}

impl DcaTrade {
    pub fn new(order_id: u64, buy_price: f64, quantity: f64, cost: f64) -> Self {
        Self {
            order_id,
            buy_price,
            quantity,
            cost,
            timestamp: Utc::now(),
        }
    }
}
