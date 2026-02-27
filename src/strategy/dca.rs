use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

use crate::config::DcaConfig;
use crate::models::order::DcaTrade;

/// Estado de la estrategia DCA
#[derive(Debug, Clone, PartialEq)]
pub enum DcaState {
    /// Detenida / esperando inicio manual
    Idle,
    /// Ejecutándose, esperando condición de compra
    Running,
    /// Take profit alcanzado (vendido)
    TakeProfitReached,
    /// Stop loss activado (vendido)
    StopLossReached,
    /// Número máximo de órdenes alcanzado
    MaxOrdersReached,
    /// Error durante la ejecución
    Error(String),
}

impl DcaState {
    pub fn label(&self) -> &str {
        match self {
            DcaState::Idle => "DETENIDO",
            DcaState::Running => "ACTIVO",
            DcaState::TakeProfitReached => "TAKE PROFIT",
            DcaState::StopLossReached => "STOP LOSS",
            DcaState::MaxOrdersReached => "MAX ÓRDENES",
            DcaState::Error(_) => "ERROR",
        }
    }

    pub fn is_active(&self) -> bool {
        *self == DcaState::Running
    }
}

/// Motor de la estrategia DCA
pub struct DcaStrategy {
    pub config: DcaConfig,
    pub state: DcaState,
    pub trades: Vec<DcaTrade>,
    pub last_buy_time: Option<DateTime<Utc>>,
    pub last_buy_price: Option<f64>,
    /// Gasto total en el día actual
    pub daily_spent: f64,
    /// Día del mes del último reset
    last_reset_day: u32,
    /// Tiempo hasta la próxima compra (segundos)
    pub next_buy_in_secs: i64,
    /// Precio máximo visto mientras hay compras abiertas (para trailing TP)
    pub price_peak: f64,
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
        }
    }

    // -----------------------------------------------------------
    // Métricas del portfolio DCA
    // -----------------------------------------------------------

    /// Costo promedio de todas las compras
    pub fn average_cost(&self) -> f64 {
        let total_qty = self.total_quantity();
        if total_qty == 0.0 {
            return 0.0;
        }
        let total_cost: f64 = self.trades.iter().map(|t| t.cost).sum();
        total_cost / total_qty
    }

    /// Total invertido en USDT
    pub fn total_invested(&self) -> f64 {
        self.trades.iter().map(|t| t.cost).sum()
    }

    /// Cantidad total del asset base (ej: BTC)
    pub fn total_quantity(&self) -> f64 {
        self.trades.iter().map(|t| t.quantity).sum()
    }

    /// P&L absoluto en USDT al precio actual
    pub fn pnl(&self, current_price: f64) -> f64 {
        let current_value = self.total_quantity() * current_price;
        current_value - self.total_invested()
    }

    /// P&L en porcentaje
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
        // Reset diario
        let today = now.day();
        if today != self.last_reset_day {
            self.daily_spent = 0.0;
            self.last_reset_day = today;
        }

        // Calcular tiempo hasta próxima compra
        if let Some(last_time) = self.last_buy_time {
            let interval_secs = (self.config.interval_minutes * 60) as i64;
            let elapsed = now.signed_duration_since(last_time).num_seconds();
            self.next_buy_in_secs = (interval_secs - elapsed).max(0);
        } else {
            self.next_buy_in_secs = 0; // primera compra, inmediata
        }
    }

    /// Decide si se debe ejecutar una compra DCA ahora
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

        // Primera compra: inmediata
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

        // Trigger por caída de precio
        if self.config.price_drop_trigger > 0.0 {
            if let Some(last_price) = self.last_buy_price {
                if last_price > 0.0 {
                    let drop_pct = ((last_price - current_price) / last_price) * 100.0;
                    if drop_pct >= self.config.price_drop_trigger {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Actualiza el precio máximo visto mientras hay compras abiertas
    pub fn update_price_peak(&mut self, price: f64) {
        if !self.trades.is_empty() && price > self.price_peak {
            self.price_peak = price;
        }
    }

    /// Trailing Take Profit: vende si el precio cayó X% desde el máximo Y sigue en ganancia
    pub fn should_trailing_tp(&self, current_price: f64) -> bool {
        if self.trades.is_empty() || self.config.trailing_tp_pct <= 0.0 {
            return false;
        }
        let avg = self.average_cost();
        if avg == 0.0 || self.price_peak <= avg {
            return false; // nunca estuvo en ganancia
        }
        let drop_from_peak = ((self.price_peak - current_price) / self.price_peak) * 100.0;
        drop_from_peak >= self.config.trailing_tp_pct && current_price > avg
    }

    /// Retorna el precio que dispararía el trailing TP (para mostrar en TUI)
    pub fn trailing_tp_trigger_price(&self) -> f64 {
        if self.price_peak <= 0.0 || self.config.trailing_tp_pct <= 0.0 {
            return 0.0;
        }
        self.price_peak * (1.0 - self.config.trailing_tp_pct / 100.0)
    }

    /// Decide si se debe tomar ganancias (vender todo)
    pub fn should_take_profit(&self, current_price: f64) -> bool {
        if self.trades.is_empty() || self.config.take_profit_pct <= 0.0 {
            return false;
        }
        let avg = self.average_cost();
        if avg == 0.0 {
            return false;
        }
        let gain_pct = ((current_price - avg) / avg) * 100.0;
        gain_pct >= self.config.take_profit_pct
    }

    /// Decide si se debe activar el stop loss (vender todo)
    pub fn should_stop_loss(&self, current_price: f64) -> bool {
        if self.trades.is_empty() || self.config.stop_loss_pct <= 0.0 {
            return false;
        }
        let avg = self.average_cost();
        if avg == 0.0 {
            return false;
        }
        let loss_pct = ((avg - current_price) / avg) * 100.0;
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

    /// Registra una compra exitosa
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

    /// Limpia las operaciones tras una venta (take profit / stop loss)
    pub fn clear_trades(&mut self) {
        self.trades.clear();
        self.last_buy_time = None;
        self.last_buy_price = None;
        self.price_peak = 0.0;
    }

    /// Formatea el tiempo hasta próxima compra como "MM:SS"
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

    /// Crea un snapshot del estado actual para persistencia
    pub fn to_snapshot(&self, symbol: &str) -> StrategySnapshot {
        StrategySnapshot {
            symbol: symbol.to_string(),
            trades: self.trades.clone(),
            last_buy_time: self.last_buy_time,
            last_buy_price: self.last_buy_price,
            daily_spent: self.daily_spent,
            last_reset_day: self.last_reset_day,
            price_peak: self.price_peak,
        }
    }

    /// Restaura el estado desde un snapshot (el estado queda en Idle por seguridad)
    pub fn restore_from_snapshot(&mut self, snapshot: StrategySnapshot) {
        self.trades = snapshot.trades;
        self.last_buy_time = snapshot.last_buy_time;
        self.last_buy_price = snapshot.last_buy_price;
        self.daily_spent = snapshot.daily_spent;
        self.last_reset_day = snapshot.last_reset_day;
        self.price_peak = snapshot.price_peak;
        // state se mantiene Idle — el usuario debe reactivar manualmente
    }
}

// ---------------------------------------------------------------------------
// Persistencia del estado de la estrategia
// ---------------------------------------------------------------------------

/// Snapshot serializable del estado DCA
#[derive(Debug, Serialize, Deserialize)]
pub struct StrategySnapshot {
    pub symbol: String,
    pub trades: Vec<DcaTrade>,
    pub last_buy_time: Option<DateTime<Utc>>,
    pub last_buy_price: Option<f64>,
    pub daily_spent: f64,
    pub last_reset_day: u32,
    #[serde(default)]
    pub price_peak: f64,
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
