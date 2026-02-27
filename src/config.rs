use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Dirección de la estrategia DCA
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// LONG: compra y vende cuando sube (comportamiento original)
    Long,
    /// SHORT: vende activo base y recompra cuando baja
    Short,
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
}

#[derive(Debug, Deserialize, Clone)]
pub struct BinanceConfig {
    pub api_key: String,
    pub api_secret: String,
    pub testnet: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DcaConfig {
    /// Símbolo de Binance (ej: BTCUSDT)
    pub symbol: String,
    /// Dirección: "long" (comprar y vender cuando sube) o "short" (vender y recomprar cuando baja)
    #[serde(default)]
    pub direction: Direction,
    /// Monto en moneda de cotización por operación (ej: 10 USDT)
    pub quote_amount: f64,
    /// Intervalo entre entradas en minutos
    pub interval_minutes: u64,
    /// LONG: entrada adicional si precio cae X% desde última compra (0 = off)
    /// SHORT: entrada adicional si precio sube X% desde última venta (0 = off)
    pub price_drop_trigger: f64,
    /// Máximo número de órdenes DCA
    pub max_orders: u32,
    /// Take profit en % desde precio promedio de entrada (0 = off)
    pub take_profit_pct: f64,
    /// Stop loss en % desde precio promedio de entrada (0 = off)
    pub stop_loss_pct: f64,
    /// Trailing take profit: cierra si precio retrocede X% desde el extremo con ganancia (0 = off)
    pub trailing_tp_pct: f64,
    /// Reiniciar el ciclo DCA automáticamente después de un TP/Trailing TP (true/false)
    /// Si es false, el bot muestra un overlay y espera que el usuario decida
    pub auto_restart: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RiskConfig {
    /// Gasto máximo en USDT por día
    pub max_daily_spend: f64,
}

/// Devuelve el directorio donde vive el ejecutable (o el directorio actual como fallback)
pub fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

impl Config {
    /// Carga la config y devuelve también el path donde fue encontrada
    pub fn load() -> Result<(Self, std::path::PathBuf)> {
        let path = if std::path::Path::new("config.toml").exists() {
            std::path::PathBuf::from("config.toml")
        } else {
            exe_dir().join("config.toml")
        };
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("No se encontró config.toml (buscado en {:?})", path))?;
        let config: Config =
            toml::from_str(&content).context("Error al parsear config.toml")?;

        if config.binance.api_key == "TU_API_KEY_AQUI" {
            anyhow::bail!("Configura tus claves de API en config.toml antes de ejecutar el bot");
        }
        if config.dca.quote_amount <= 0.0 {
            anyhow::bail!("dca.quote_amount debe ser mayor a 0");
        }
        if config.dca.interval_minutes == 0 {
            anyhow::bail!("dca.interval_minutes debe ser mayor a 0");
        }

        Ok((config, path))
    }

    /// Guarda símbolo y monto en el config.toml preservando comentarios
    pub fn save_dca(path: &std::path::Path, symbol: &str, amount: f64) -> Result<()> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("No se pudo leer {:?}", path))?;
        let mut doc = content
            .parse::<toml_edit::DocumentMut>()
            .context("Error al parsear config.toml para guardar")?;

        doc["dca"]["symbol"] = toml_edit::value(symbol);
        doc["dca"]["quote_amount"] = toml_edit::value(amount);

        std::fs::write(path, doc.to_string())
            .with_context(|| format!("No se pudo escribir {:?}", path))?;
        Ok(())
    }
}
