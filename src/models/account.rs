use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Balance {
    pub asset: String,
    pub free: String,
    pub locked: String,
}

impl Balance {
    pub fn free_f64(&self) -> f64 {
        self.free.parse().unwrap_or(0.0)
    }

    pub fn locked_f64(&self) -> f64 {
        self.locked.parse().unwrap_or(0.0)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub maker_commission: i32,
    pub taker_commission: i32,
    pub buyer_commission: i32,
    pub seller_commission: i32,
    pub can_trade: bool,
    pub can_deposit: bool,
    pub can_withdraw: bool,
    pub balances: Vec<Balance>,
}

impl AccountInfo {
    /// Retorna el balance libre de un asset (ej: "BTC", "USDT")
    pub fn get_free(&self, asset: &str) -> f64 {
        self.balances
            .iter()
            .find(|b| b.asset == asset)
            .map(|b| b.free_f64())
            .unwrap_or(0.0)
    }

    /// Retorna balances no-cero para mostrar en el UI
    pub fn non_zero_balances(&self) -> Vec<&Balance> {
        self.balances
            .iter()
            .filter(|b| b.free_f64() > 0.0 || b.locked_f64() > 0.0)
            .collect()
    }
}
