use std::collections::VecDeque;

use crate::strategy::dca::DcaStrategy;

/// Símbolos disponibles para seleccionar desde la TUI
pub const SYMBOLS: &[&str] = &[
    "BTCUSDT", "ETHUSDT", "XRPUSDT", "ADAUSDT",
    "DOGEUSDT", "SOLUSDT", "TRXUSDT", "RONUSDT", "BNBUSDT",
];

/// Resultado de una venta (para mostrar en el overlay post-venta)
#[derive(Debug, Clone, PartialEq)]
pub struct SaleResult {
    pub kind: String,    // "TAKE PROFIT", "TRAILING TP", "STOP LOSS"
    pub received: f64,   // USDT recibidos
    pub pnl: f64,        // ganancia/pérdida en USDT
    pub pnl_pct: f64,    // ganancia/pérdida en %
}

/// Modo de la interfaz de usuario
#[derive(Debug, Clone, PartialEq)]
pub enum UiMode {
    Normal,
    Config,
    /// Overlay al inicio: sesión anterior encontrada, pregunta si continuar
    RestoreSession(usize), // número de trades restaurados
    /// Overlay de confirmación antes de iniciar el ciclo DCA
    ConfirmStart,
    /// Overlay post-venta: muestra resultado y pregunta qué hacer
    PostSale(SaleResult),
}

/// Mensajes que el UI puede enviar al motor de estrategia
#[derive(Debug)]
pub enum AppCommand {
    Start,
    Stop,
    Quit,
    // Restauración de sesión al inicio
    RestoreSessionContinue,  // mantener trades anteriores
    RestoreSessionDiscard,   // descartar y empezar de cero
    // Modal de confirmación de inicio
    OpenConfirmStart,
    ConfirmToggleAutoRestart,
    // Panel de configuración
    OpenConfig,
    CloseConfig,
    CfgTabNext,
    CfgNavUp,
    CfgNavDown,
    CfgInputChar(char),
    CfgBackspace,
    CfgConfirm,
    // Post-venta
    PostSaleRestart,   // reiniciar ciclo DCA inmediatamente
    PostSaleDismiss,   // cerrar overlay y quedar detenido
}

/// Estado compartido entre el UI y el motor de estrategia
pub struct AppState {
    pub current_price: f64,
    pub change_24h_pct: f64,
    pub high_24h: f64,
    pub low_24h: f64,
    pub strategy: DcaStrategy,
    pub base_asset: String,
    pub quote_asset: String,
    pub base_balance: f64,
    pub quote_balance: f64,
    /// Ring buffer para mensajes de log (últimos 100)
    pub log: VecDeque<String>,
    pub should_quit: bool,
    // --- Config panel ---
    pub ui_mode: UiMode,
    pub cfg_tab: usize,              // 0 = Moneda, 1 = Monto
    pub cfg_symbol_idx: usize,       // índice seleccionado en lista SYMBOLS
    pub cfg_amount_buf: String,      // buffer de texto para monto
    // --- Modal de inicio ---
    pub confirm_auto_restart: bool,  // selección de reinicio en el modal
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

    pub fn log_error(&mut self, msg: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        let entry = format!("[{}] ⚠ {}", ts, msg);
        tracing::error!("{}", msg);
        if self.log.len() >= 100 {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }
}
