use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{DcaConfig, Direction};
use crate::models::order::DcaTrade;

/// DCA strategy state
#[derive(Debug, Clone, PartialEq)]
pub enum DcaState {
    /// Stopped / waiting for manual start
    Idle,
    /// Running, waiting for entry condition
    Running,
    /// Take profit reached (position closed)
    TakeProfitReached,
    /// Stop loss activated (position closed)
    StopLossReached,
    /// Maximum number of orders reached
    MaxOrdersReached,
    /// Error during execution
    Error(String),
}

impl DcaState {
    pub fn label(&self) -> &str {
        match self {
            DcaState::Idle => "STOPPED",
            DcaState::Running => "ACTIVE",
            DcaState::TakeProfitReached => "TAKE PROFIT",
            DcaState::StopLossReached => "STOP LOSS",
            DcaState::MaxOrdersReached => "MAX ORDERS",
            DcaState::Error(_) => "ERROR",
        }
    }

    pub fn is_active(&self) -> bool {
        *self == DcaState::Running
    }
}

/// DCA strategy engine
pub struct DcaStrategy {
    pub config: DcaConfig,
    pub state: DcaState,
    pub trades: Vec<DcaTrade>,
    pub last_buy_time: Option<DateTime<Utc>>,
    pub last_buy_price: Option<f64>,
    /// Total spent on the current day (LONG: USDT bought; SHORT: base asset USDT sold)
    pub daily_spent: f64,
    /// Day of the month of the last reset
    last_reset_day: u32,
    /// Time until next entry (seconds)
    pub next_buy_in_secs: i64,
    /// LONG: maximum price seen while position is open (for trailing TP)
    pub price_peak: f64,
    /// SHORT: minimum price seen while position is open (for inverse trailing TP)
    pub price_trough: f64,
}

impl DcaStrategy {
    pub fn new(config: DcaConfig) -> Self {
        Self {
            config,
            state: DcaState::Idle,
            trades: Vec::new(),
            last_buy_time: None,
            last_buy_price: None,
            daily_spent: 0.0,
            last_reset_day: 0,
            next_buy_in_secs: 0,
            price_peak: 0.0,
            price_trough: f64::MAX,
        }
    }

    // -----------------------------------------------------------
    // DCA portfolio metrics
    // -----------------------------------------------------------

    /// Average entry price (buy in LONG, sell in SHORT)
    pub fn average_cost(&self) -> f64 {
        let total_qty = self.total_quantity();
        if total_qty == 0.0 {
            return 0.0;
        }
        let total_cost: f64 = self.trades.iter().map(|t| t.cost).sum();
        total_cost / total_qty
    }

    /// Total USDT involved in entries
    /// LONG: total spent on buys; SHORT: total received on selling
    pub fn total_invested(&self) -> f64 {
        self.trades.iter().map(|t| t.cost).sum()
    }

    /// Total base asset quantity (e.g.: BTC) in position
    pub fn total_quantity(&self) -> f64 {
        self.trades.iter().map(|t| t.quantity).sum()
    }

    /// Absolute P&L in USDT at current price
    /// LONG:  profit when price rises  (current_value - cost)
    /// SHORT: profit when price falls  (cost - current_value)
    pub fn pnl(&self, current_price: f64) -> f64 {
        let current_value = self.total_quantity() * current_price;
        let invested = self.total_invested();
        match self.config.direction {
            Direction::Long  => current_value - invested,
            Direction::Short => invested - current_value,
        }
    }

    /// P&L in percentage
    pub fn pnl_pct(&self, current_price: f64) -> f64 {
        let invested = self.total_invested();
        if invested == 0.0 {
            return 0.0;
        }
        (self.pnl(current_price) / invested) * 100.0
    }

    // -----------------------------------------------------------
    // Lógica de decisión
    // -----------------------------------------------------------

    /// Actualiza el contador regresivo y verifica el reset diario
    pub fn tick(&mut self, now: DateTime<Utc>) {
        // Daily reset
        let today = now.day();
        if today != self.last_reset_day {
            self.daily_spent = 0.0;
            self.last_reset_day = today;
        }

        // Calcular tiempo hasta próxima entrada
        if let Some(last_time) = self.last_buy_time {
            let interval_secs = (self.config.interval_minutes * 60) as i64;
            let elapsed = now.signed_duration_since(last_time).num_seconds();
            self.next_buy_in_secs = (interval_secs - elapsed).max(0);
        } else {
            self.next_buy_in_secs = 0; // first entry: immediate
        }
    }

    /// Decides if a DCA entry should be executed now
    /// LONG: buy; SHORT: sell base asset
    pub fn should_buy(&self, current_price: f64, now: DateTime<Utc>, max_daily: f64) -> bool {
        if !self.state.is_active() {
            return false;
        }

        // Límite de órdenes
        if self.trades.len() >= self.config.max_orders as usize {
            return false;
        }

        // Límite diario
        if self.daily_spent + self.config.quote_amount > max_daily {
            return false;
        }

        // First entry: immediate
        if self.last_buy_time.is_none() {
            return true;
        }

        // Trigger por tiempo
        let elapsed = now
            .signed_duration_since(self.last_buy_time.unwrap())
            .num_minutes();
        if elapsed >= self.config.interval_minutes as i64 {
            return true;
        }

        // Trigger por movimiento de precio
        if self.config.price_drop_trigger > 0.0 {
            if let Some(last_price) = self.last_buy_price {
                if last_price > 0.0 {
                    let move_pct = match self.config.direction {
                        // LONG: comprar más si cayó X%
                        Direction::Long => ((last_price - current_price) / last_price) * 100.0,
                        // SHORT: vender más si subió X%
                        Direction::Short => ((current_price - last_price) / last_price) * 100.0,
                    };
                    if move_pct >= self.config.price_drop_trigger {
                        return true;
                    }
                }
            }
        }

        false
    }

    // -----------------------------------------------------------
    // Trailing extreme logic (peak for LONG, trough for SHORT)
    // -----------------------------------------------------------

    /// LONG: updates maximum price seen while position is open
    pub fn update_price_peak(&mut self, price: f64) {
        if !self.trades.is_empty() {
            match self.config.direction {
                Direction::Long => {
                    if price > self.price_peak {
                        self.price_peak = price;
                    }
                }
                Direction::Short => {
                    if price < self.price_trough {
                        self.price_trough = price;
                    }
                }
            }
        }
    }

    /// LONG: Trailing Take Profit: closes if price fell X% from the maximum AND is still in profit
    /// SHORT: Trailing Take Profit: closes if price rose X% from the minimum AND is still in profit
    pub fn should_trailing_tp(&self, current_price: f64) -> bool {
        if self.trades.is_empty() || self.config.trailing_tp_pct <= 0.0 {
            return false;
        }
        let avg = self.average_cost();
        if avg == 0.0 {
            return false;
        }

        match self.config.direction {
            Direction::Long => {
                if self.price_peak <= avg {
                    return false; // nunca estuvo en ganancia
                }
                let drop_from_peak =
                    ((self.price_peak - current_price) / self.price_peak) * 100.0;
                drop_from_peak >= self.config.trailing_tp_pct && current_price > avg
            }
            Direction::Short => {
                if self.price_trough >= avg || self.price_trough == f64::MAX {
                    return false; // never was in profit
                }
                let rise_from_trough =
                    ((current_price - self.price_trough) / self.price_trough) * 100.0;
                rise_from_trough >= self.config.trailing_tp_pct && current_price < avg
            }
        }
    }

    /// Price that would trigger trailing TP (for TUI display)
    pub fn trailing_tp_trigger_price(&self) -> f64 {
        if self.config.trailing_tp_pct <= 0.0 {
            return 0.0;
        }
        match self.config.direction {
            Direction::Long => {
                if self.price_peak <= 0.0 {
                    return 0.0;
                }
                self.price_peak * (1.0 - self.config.trailing_tp_pct / 100.0)
            }
            Direction::Short => {
                if self.price_trough == f64::MAX || self.price_trough <= 0.0 {
                    return 0.0;
                }
                self.price_trough * (1.0 + self.config.trailing_tp_pct / 100.0)
            }
        }
    }

    /// Decides if profit should be taken (close position)
    /// LONG: profit when price rises above average cost
    /// SHORT: profit when price falls below average sell price
    pub fn should_take_profit(&self, current_price: f64) -> bool {
        if self.trades.is_empty() || self.config.take_profit_pct <= 0.0 {
            return false;
        }
        let avg = self.average_cost();
        if avg == 0.0 {
            return false;
        }
        let gain_pct = match self.config.direction {
            Direction::Long  => ((current_price - avg) / avg) * 100.0,
            Direction::Short => ((avg - current_price) / avg) * 100.0,
        };
        gain_pct >= self.config.take_profit_pct
    }

    /// Decides if stop loss should be activated (close position)
    /// LONG: loss when price falls below average cost
    /// SHORT: loss when price rises above average sell price
    pub fn should_stop_loss(&self, current_price: f64) -> bool {
        if self.trades.is_empty() || self.config.stop_loss_pct <= 0.0 {
            return false;
        }
        let avg = self.average_cost();
        if avg == 0.0 {
            return false;
        }
        let loss_pct = match self.config.direction {
            Direction::Long  => ((avg - current_price) / avg) * 100.0,
            Direction::Short => ((current_price - avg) / avg) * 100.0,
        };
        loss_pct >= self.config.stop_loss_pct
    }

    // -----------------------------------------------------------
    // Mutaciones de estado
    // -----------------------------------------------------------

    pub fn start(&mut self) {
        self.state = DcaState::Running;
    }

    pub fn stop(&mut self) {
        if self.state == DcaState::Running {
            self.state = DcaState::Idle;
        }
    }

    /// Records a successful entry (buy in LONG, sell in SHORT)
    pub fn record_buy(&mut self, order_id: u64, price: f64, quantity: f64, cost: f64) {
        let now = Utc::now();
        self.trades.push(DcaTrade::new(order_id, price, quantity, cost));
        self.last_buy_time = Some(now);
        self.last_buy_price = Some(price);
        self.daily_spent += cost;
        self.next_buy_in_secs = (self.config.interval_minutes * 60) as i64;

        if self.trades.len() >= self.config.max_orders as usize {
            self.state = DcaState::MaxOrdersReached;
        }
    }

    /// Clears trades after closing position (TP / SL)
    pub fn clear_trades(&mut self) {
        self.trades.clear();
        self.last_buy_time = None;
        self.last_buy_price = None;
        self.price_peak = 0.0;
        self.price_trough = f64::MAX;
    }

    /// Formats time until next entry as "MM:SS"
    pub fn next_buy_countdown(&self) -> String {
        if !self.state.is_active() {
            return "--:--".to_string();
        }
        if self.last_buy_time.is_none() {
            return "00:00".to_string();
        }
        let secs = self.next_buy_in_secs;
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }

    /// Creates a snapshot of current state for persistence
    pub fn to_snapshot(&self, symbol: &str) -> StrategySnapshot {
        StrategySnapshot {
            symbol: symbol.to_string(),
            direction: self.config.direction.clone(),
            trades: self.trades.clone(),
            last_buy_time: self.last_buy_time,
            last_buy_price: self.last_buy_price,
            daily_spent: self.daily_spent,
            last_reset_day: self.last_reset_day,
            price_peak: self.price_peak,
            price_trough: self.price_trough,
        }
    }

    /// Restores state from a snapshot (state remains Idle for safety)
    pub fn restore_from_snapshot(&mut self, snapshot: StrategySnapshot) {
        self.config.direction = snapshot.direction;
        self.trades = snapshot.trades;
        self.last_buy_time = snapshot.last_buy_time;
        self.last_buy_price = snapshot.last_buy_price;
        self.daily_spent = snapshot.daily_spent;
        self.last_reset_day = snapshot.last_reset_day;
        self.price_peak = snapshot.price_peak;
        self.price_trough = snapshot.price_trough;
        // state remains Idle — user must reactivate manually
    }
}

// ---------------------------------------------------------------------------
// Persistencia del estado de la estrategia
// ---------------------------------------------------------------------------

/// Serializable snapshot of DCA state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySnapshot {
    pub symbol: String,
    /// Dirección de la estrategia (long/short). Default = long para compatibilidad.
    #[serde(default)]
    pub direction: Direction,
    pub trades: Vec<DcaTrade>,
    pub last_buy_time: Option<DateTime<Utc>>,
    pub last_buy_price: Option<f64>,
    pub daily_spent: f64,
    pub last_reset_day: u32,
    #[serde(default)]
    pub price_peak: f64,
    #[serde(default = "default_trough")]
    pub price_trough: f64,
}

fn default_trough() -> f64 {
    f64::MAX
}

impl StrategySnapshot {
    /// Guarda el snapshot en disco como JSON
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Carga el snapshot desde disco; devuelve None si no existe o está corrupto
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }
}
