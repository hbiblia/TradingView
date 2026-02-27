use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// DCA strategy direction
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// LONG: buy and sell when it goes up (original behavior)
    Long,
    /// SHORT: sell base asset and rebuy when it goes down
    Short,
}

impl Direction {
    pub fn flip(&self) -> Self {
        match self {
            Direction::Long  => Direction::Short,
            Direction::Short => Direction::Long,
        }
    }
}

impl Default for Direction {
    fn default() -> Self {
        Direction::Long
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub binance: BinanceConfig,
    pub dca: DcaConfig,
    pub risk: RiskConfig,
    #[serde(default)]
    pub alerts: AlertsConfig,
}

/// Support/Resistance alert engine configuration
#[derive(Debug, Deserialize, Clone)]
pub struct AlertsConfig {
    /// Number of closed candles to calculate S/R (excludes current candle)
    #[serde(default = "default_rolling_window")]
    pub rolling_window: usize,
    /// Candle interval: "1m", "5m", "15m", "1h", "4h", "1d"
    #[serde(default = "default_candle_interval")]
    pub candle_interval: String,
    /// Minimum minutes between two alerts of the same type for the same symbol
    #[serde(default = "default_cooldown_minutes")]
    pub cooldown_minutes: u64,
}

fn default_rolling_window() -> usize { 20 }
fn default_candle_interval() -> String { "1h".to_string() }
fn default_cooldown_minutes() -> u64 { 30 }

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            rolling_window: default_rolling_window(),
            candle_interval: default_candle_interval(),
            cooldown_minutes: default_cooldown_minutes(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct BinanceConfig {
    pub api_key: String,
    pub api_secret: String,
    pub testnet: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DcaConfig {
    /// Binance symbol (e.g.: BTCUSDT)
    pub symbol: String,
    /// Direction: "long" (buy and sell when it goes up) or "short" (sell and rebuy when it goes down)
    #[serde(default)]
    pub direction: Direction,
    /// Amount in quote currency per trade (e.g.: 10 USDT)
    pub quote_amount: f64,
    /// Interval between entries in minutes
    pub interval_minutes: u64,
    /// LONG: additional entry if price drops X% from last buy (0 = off)
    /// SHORT: additional entry if price rises X% from last sell (0 = off)
    pub price_drop_trigger: f64,
    /// Maximum number of DCA orders
    pub max_orders: u32,
    /// Take profit in % from average entry price (0 = off)
    pub take_profit_pct: f64,
    /// Stop loss in % from average entry price (0 = off)
    pub stop_loss_pct: f64,
    /// Trailing take profit: closes if price retreats X% from the peak/trough (0 = off)
    pub trailing_tp_pct: f64,
    /// Restart DCA cycle automatically after a TP/Trailing TP (true/false)
    /// If false, the bot shows an overlay and waits for user decision
    pub auto_restart: bool,
    /// If auto_restart is true, automatically flip direction (Long <-> Short) after a TP
    #[serde(default)]
    pub auto_flip: bool,
    /// Use BNB for commissions (applies 25% discount logic if true)
    #[serde(default)]
    pub has_bnb_balance: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RiskConfig {
    /// Maximum USDT spend per day
    pub max_daily_spend: f64,
}

/// Returns the directory where the executable lives (or current directory as fallback)
pub fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

impl Config {
    /// Loads the config and also returns the path where it was found
    pub fn load() -> Result<(Self, std::path::PathBuf)> {
        let path = if std::path::Path::new("config.toml").exists() {
            std::path::PathBuf::from("config.toml")
        } else {
            exe_dir().join("config.toml")
        };
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("config.toml not found (searched in {:?})", path))?;
        let config: Config =
            toml::from_str(&content).context("Error parsing config.toml")?;

        if config.binance.api_key == "YOUR_API_KEY_HERE" {
            anyhow::bail!("Configure your API keys in config.toml before running the bot");
        }
        if config.dca.quote_amount <= 0.0 {
            anyhow::bail!("dca.quote_amount must be greater than 0");
        }
        if config.dca.interval_minutes == 0 {
            anyhow::bail!("dca.interval_minutes must be greater than 0");
        }

        Ok((config, path))
    }

    /// Saves symbol and amount in config.toml preserving comments
    pub fn save_dca(path: &std::path::Path, symbol: &str, amount: f64) -> Result<()> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Could not read {:?}", path))?;
        let mut doc = content
            .parse::<toml_edit::DocumentMut>()
            .context("Error parsing config.toml to save")?;

        doc["dca"]["symbol"] = toml_edit::value(symbol);
        doc["dca"]["quote_amount"] = toml_edit::value(amount);

        std::fs::write(path, doc.to_string())
            .with_context(|| format!("Could not write {:?}", path))?;
        Ok(())
    }
}
