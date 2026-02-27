use serde::Deserialize;

/// Respuesta de GET /api/v3/ticker/price
#[derive(Debug, Deserialize, Clone)]
pub struct TickerPrice {
    pub symbol: String,
    pub price: String,
}

/// Evento del stream WebSocket @miniTicker
#[derive(Debug, Deserialize, Clone)]
pub struct MiniTickerEvent {
    #[serde(rename = "e")]
    pub event_type: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    /// Precio de cierre (último precio)
    #[serde(rename = "c")]
    pub close_price: String,
    /// Precio de apertura (24h)
    #[serde(rename = "o")]
    pub open_price: String,
    /// Precio máximo (24h)
    #[serde(rename = "h")]
    pub high_price: String,
    /// Precio mínimo (24h)
    #[serde(rename = "l")]
    pub low_price: String,
    /// Volumen base (24h)
    #[serde(rename = "v")]
    pub base_volume: String,
    /// Volumen cotización (24h)
    #[serde(rename = "q")]
    pub quote_volume: String,
}

/// Wrapper del combined stream de Binance (multi-símbolo)
/// Formato: {"stream":"btcusdt@miniTicker","data":{...MiniTickerEvent...}}
#[derive(Debug, Deserialize, Clone)]
pub struct CombinedStreamWrapper {
    pub stream: String,
    pub data: MiniTickerEvent,
}

impl MiniTickerEvent {
    pub fn close_f64(&self) -> f64 {
        self.close_price.parse().unwrap_or(0.0)
    }

    pub fn open_f64(&self) -> f64 {
        self.open_price.parse().unwrap_or(0.0)
    }

    pub fn change_pct(&self) -> f64 {
        let open = self.open_f64();
        if open == 0.0 {
            return 0.0;
        }
        ((self.close_f64() - open) / open) * 100.0
    }
}
