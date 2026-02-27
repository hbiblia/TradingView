use std::collections::{HashMap, VecDeque};

use crate::config::Direction;
use crate::strategy::dca::DcaStrategy;

/// Máximo de estrategias simultáneas
pub const MAX_SLOTS: usize = 4;

/// Lista de respaldo cuando la API de Binance no está disponible
pub const DEFAULT_SYMBOLS: &[&str] = &[
    "BTCUSDT", "ETHUSDT", "XRPUSDT", "ADAUSDT",
    "DOGEUSDT", "SOLUSDT", "TRXUSDT", "RONUSDT", "BNBUSDT",
];

/// Datos de mercado para un símbolo
#[derive(Debug, Clone, Default)]
pub struct MarketData {
    pub price: f64,
    pub change_24h_pct: f64,
    pub high_24h: f64,
    pub low_24h: f64,
}

/// Niveles de soporte/resistencia calculados por el motor de alertas
pub struct AlertLevel {
    /// Resistencia: máximo de los highs en el rolling window
    pub resistance: f64,
    /// Soporte: mínimo de los lows en el rolling window
    pub support: f64,
    /// Último precio conocido (para detectar cruce de nivel)
    pub prev_price: f64,
    /// Instante de la última alerta de soporte disparada (para cooldown)
    pub last_support_alert: Option<std::time::Instant>,
    /// Instante de la última alerta de resistencia disparada (para cooldown)
    pub last_resistance_alert: Option<std::time::Instant>,
}

/// Una estrategia DCA activa con su contexto de mercado
pub struct StrategySlot {
    pub id: usize,
    pub strategy: DcaStrategy,
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub base_balance: f64,
    pub quote_balance: f64,
}

/// Resultado de una venta (para mostrar en el overlay post-venta)
#[derive(Debug, Clone, PartialEq)]
pub struct SaleResult {
    pub kind: String,    // "TAKE PROFIT", "TRAILING TP", "STOP LOSS"
    pub received: f64,   // USDT recibidos / pagados
    pub pnl: f64,        // ganancia/pérdida en USDT
    pub pnl_pct: f64,    // ganancia/pérdida en %
}

/// Modo de la interfaz de usuario
#[derive(Debug, Clone, PartialEq)]
pub enum UiMode {
    Normal,
    /// Panel de configuración (solo monto USDT)
    Config,
    /// Overlay al inicio: sesiones anteriores encontradas
    /// Vec<(symbol, direction, trade_count)>
    RestoreSession(Vec<(String, Direction, usize)>),
    /// Modal para lanzar una nueva estrategia (S)
    NewStrategy,
    /// Overlay post-venta: muestra resultado de un slot específico
    PostSale(usize, SaleResult),
    /// Confirmación de cierre manual de posición (V)
    ConfirmClose,
}

/// Mensajes que el UI puede enviar al motor de estrategia
#[derive(Debug)]
pub enum AppCommand {
    Quit,

    // --- Navegación de slots ---
    SlotSelectUp,
    SlotSelectDown,
    StopSelected,

    // --- Modal nueva estrategia (S) ---
    OpenNewStrategy,
    NewStratSymbolUp,
    NewStratSymbolDown,
    NewStratToggleDirection,      // Tab: alterna LONG/SHORT
    NewStratToggleAutoRestart,    // ←/→: alterna manual/auto
    NewStratConfirm,              // Enter: crear y lanzar
    NewStratCancel,               // Esc: cancelar

    // --- Post-venta por slot ---
    PostSaleRestart(usize),       // slot_id: reiniciar ciclo
    PostSaleDismiss(usize),       // slot_id: cerrar overlay

    // --- Restauración de sesión ---
    RestoreSessionContinue,
    RestoreSessionDiscard,

    // --- Panel de configuración (solo monto) ---
    OpenConfig,
    CloseConfig,
    CfgInputChar(char),
    CfgBackspace,
    CfgConfirm,

    // --- Cierre manual de posición (V) ---
    OpenConfirmClose,   // V: pide confirmación
    ConfirmCloseNow,    // Enter: ejecuta el cierre a mercado
}

/// Estado compartido entre el UI y el motor de estrategia
pub struct AppState {
    /// Slots de estrategia activos (hasta MAX_SLOTS)
    pub slots: Vec<StrategySlot>,
    /// Índice del slot seleccionado en el panel izquierdo
    pub selected_slot: usize,
    /// Datos de precio por símbolo
    pub prices: HashMap<String, MarketData>,
    /// Niveles S/R calculados por el motor de alertas (por símbolo)
    pub alert_levels: HashMap<String, AlertLevel>,
    /// Lista de pares disponibles obtenida de Binance al arrancar
    pub symbols: Vec<String>,
    /// Ring buffer para mensajes de log (últimos 100)
    pub log: VecDeque<String>,
    pub should_quit: bool,
    pub ui_mode: UiMode,

    // --- Modal nueva estrategia ---
    pub new_strat_symbol_idx: usize,
    pub new_strat_direction: Direction,
    pub new_strat_auto_restart: bool,

    // --- Panel de configuración ---
    pub cfg_amount_buf: String,

    /// Próximo ID de slot (auto-incremental)
    pub next_slot_id: usize,
}

impl AppState {
    pub fn log(&mut self, msg: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        let entry = format!("[{}] {}", ts, msg);
        tracing::info!("{}", msg);
        if self.log.len() >= 100 {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }

    pub fn log_alert(&mut self, msg: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        let entry = format!("[{}] ALERT {}", ts, msg);
        tracing::warn!("ALERT: {}", msg);
        if self.log.len() >= 100 {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }

    pub fn log_error(&mut self, msg: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        let entry = format!("[{}] ⚠ {}", ts, msg);
        tracing::error!("{}", msg);
        if self.log.len() >= 100 {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }

    /// Precio actual del slot seleccionado
    pub fn selected_price(&self) -> f64 {
        self.slots
            .get(self.selected_slot)
            .and_then(|s| self.prices.get(&s.symbol))
            .map(|m| m.price)
            .unwrap_or(0.0)
    }

    /// Datos de mercado del slot seleccionado
    pub fn selected_market(&self) -> MarketData {
        self.slots
            .get(self.selected_slot)
            .and_then(|s| self.prices.get(&s.symbol))
            .cloned()
            .unwrap_or_default()
    }

    /// Slot seleccionado (si existe)
    pub fn selected(&self) -> Option<&StrategySlot> {
        self.slots.get(self.selected_slot)
    }

    /// Slot seleccionado mutable
    pub fn selected_mut(&mut self) -> Option<&mut StrategySlot> {
        self.slots.get_mut(self.selected_slot)
    }

    /// Busca un slot por ID
    pub fn slot_by_id(&self, id: usize) -> Option<&StrategySlot> {
        self.slots.iter().find(|s| s.id == id)
    }

    /// Busca un slot mutable por ID
    pub fn slot_by_id_mut(&mut self, id: usize) -> Option<&mut StrategySlot> {
        self.slots.iter_mut().find(|s| s.id == id)
    }

    /// Elimina un slot por ID
    pub fn remove_slot(&mut self, id: usize) {
        if let Some(pos) = self.slots.iter().position(|s| s.id == id) {
            self.slots.remove(pos);
            if self.selected_slot >= self.slots.len() && !self.slots.is_empty() {
                self.selected_slot = self.slots.len() - 1;
            }
        }
    }

    /// Produce el siguiente ID único para un slot
    pub fn alloc_slot_id(&mut self) -> usize {
        let id = self.next_slot_id;
        self.next_slot_id += 1;
        id
    }
}
