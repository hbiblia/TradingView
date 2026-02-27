use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{anyhow, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::{header, Client};
use serde_json::Value;
use sha2::Sha256;

use crate::config::BinanceConfig;
use crate::models::{
    account::AccountInfo,
    order::Order,
    ticker::TickerPrice,
};

type HmacSha256 = Hmac<Sha256>;

/// URLs base de Binance
const MAINNET_URL: &str = "https://api.binance.com";
const TESTNET_URL: &str = "https://testnet.binance.vision";

pub struct BinanceClient {
    http: Client,
    secret: String,
    base_url: String,
    /// Diferencia en ms entre el reloj local y el servidor de Binance
    time_offset_ms: AtomicI64,
}

impl BinanceClient {
    pub fn new(config: BinanceConfig) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "X-MBX-APIKEY",
            header::HeaderValue::from_str(&config.api_key)
                .map_err(|_| anyhow!("API key inválida"))?,
        );

        let http = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

        let base_url = if config.testnet {
            TESTNET_URL.to_string()
        } else {
            MAINNET_URL.to_string()
        };

        tracing::info!(
            "Cliente Binance inicializado ({})",
            if config.testnet { "TESTNET" } else { "MAINNET" }
        );

        Ok(Self {
            http,
            secret: config.api_secret,
            base_url,
            time_offset_ms: AtomicI64::new(0),
        })
    }

    // -------------------------------------------------------
    // Helpers internos
    // -------------------------------------------------------

    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .expect("HMAC acepta claves de cualquier tamaño");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn timestamp_ms(&self) -> u64 {
        let offset = self.time_offset_ms.load(Ordering::Relaxed);
        (Utc::now().timestamp_millis() + offset) as u64
    }

    async fn check_response(&self, resp: reqwest::Response) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // Intentar parsear mensaje de error de Binance
        if let Ok(val) = serde_json::from_str::<Value>(&text) {
            let code = val["code"].as_i64().unwrap_or(0);
            let msg = val["msg"].as_str().unwrap_or(&text);
            Err(anyhow!("Binance error {}: {} (HTTP {})", code, msg, status))
        } else {
            Err(anyhow!("HTTP {}: {}", status, text))
        }
    }

    // -------------------------------------------------------
    // Endpoints públicos (sin firma)
    // -------------------------------------------------------

    /// Test de conectividad
    pub async fn ping(&self) -> Result<()> {
        let url = format!("{}/api/v3/ping", self.base_url);
        self.http.get(&url).send().await?;
        Ok(())
    }

    /// Sincroniza el reloj local con el servidor de Binance para evitar error -1021.
    /// Calcula el offset y lo almacena para aplicarlo en cada timestamp firmado.
    pub async fn sync_time(&self) -> Result<()> {
        let local_before = Utc::now().timestamp_millis();
        let url = format!("{}/api/v3/time", self.base_url);
        let resp: Value = self.http.get(&url).send().await?.json().await?;
        let local_after = Utc::now().timestamp_millis();

        let server_time = resp["serverTime"]
            .as_i64()
            .ok_or_else(|| anyhow!("Respuesta de tiempo de Binance inválida"))?;

        // Aproximamos el tiempo local en el momento en que el servidor lo procesó
        let local_mid = (local_before + local_after) / 2;
        let offset = server_time - local_mid;

        self.time_offset_ms.store(offset, Ordering::Relaxed);
        tracing::info!("Sincronización de tiempo: offset {}ms (reloj local {}ms respecto a Binance)", offset, offset.abs());
        Ok(())
    }

    /// Precio actual de un símbolo
    pub async fn get_price(&self, symbol: &str) -> Result<f64> {
        let url = format!("{}/api/v3/ticker/price?symbol={}", self.base_url, symbol);
        let resp: TickerPrice = self.http.get(&url).send().await?.json().await?;
        resp.price
            .parse::<f64>()
            .map_err(|_| anyhow!("Precio inválido: {}", resp.price))
    }

    // -------------------------------------------------------
    // Endpoints privados (requieren firma HMAC-SHA256)
    // -------------------------------------------------------

    /// Información de la cuenta (balances, permisos)
    pub async fn get_account(&self) -> Result<AccountInfo> {
        let ts = self.timestamp_ms();
        let query = format!("timestamp={}", ts);
        let sig = self.sign(&query);
        let url = format!("{}/api/v3/account?{}&signature={}", self.base_url, query, sig);

        let resp = self.http.get(&url).send().await?;
        let resp = self.check_response(resp).await?;
        Ok(resp.json::<AccountInfo>().await?)
    }

    /// Orden de compra a mercado usando quoteOrderQty (monto en USDT)
    pub async fn market_buy_quote(&self, symbol: &str, quote_qty: f64) -> Result<Order> {
        let ts = self.timestamp_ms();
        let body = format!(
            "symbol={}&side=BUY&type=MARKET&quoteOrderQty={:.8}&timestamp={}",
            symbol, quote_qty, ts
        );
        let sig = self.sign(&body);
        let full_body = format!("{}&signature={}", body, sig);

        let url = format!("{}/api/v3/order", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(full_body)
            .send()
            .await?;

        let resp = self.check_response(resp).await?;
        Ok(resp.json::<Order>().await?)
    }

    /// Orden de compra a mercado usando quantity (cantidad base exacta, ej: BTC)
    /// Usada para cerrar posiciones SHORT: recomprar la cantidad exacta vendida
    pub async fn market_buy_qty(&self, symbol: &str, quantity: f64) -> Result<Order> {
        let ts = self.timestamp_ms();
        let body = format!(
            "symbol={}&side=BUY&type=MARKET&quantity={:.8}&timestamp={}",
            symbol, quantity, ts
        );
        let sig = self.sign(&body);
        let full_body = format!("{}&signature={}", body, sig);

        let url = format!("{}/api/v3/order", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(full_body)
            .send()
            .await?;

        let resp = self.check_response(resp).await?;
        Ok(resp.json::<Order>().await?)
    }

    /// Orden de venta a mercado usando quantity (cantidad base, ej: BTC)
    pub async fn market_sell_qty(&self, symbol: &str, quantity: f64) -> Result<Order> {
        let ts = self.timestamp_ms();
        let body = format!(
            "symbol={}&side=SELL&type=MARKET&quantity={:.8}&timestamp={}",
            symbol, quantity, ts
        );
        let sig = self.sign(&body);
        let full_body = format!("{}&signature={}", body, sig);

        let url = format!("{}/api/v3/order", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(full_body)
            .send()
            .await?;

        let resp = self.check_response(resp).await?;
        Ok(resp.json::<Order>().await?)
    }

    /// Cancela una orden por su ID
    pub async fn cancel_order(&self, symbol: &str, order_id: u64) -> Result<Value> {
        let ts = self.timestamp_ms();
        let body = format!(
            "symbol={}&orderId={}&timestamp={}",
            symbol, order_id, ts
        );
        let sig = self.sign(&body);
        let full_body = format!("{}&signature={}", body, sig);

        let url = format!("{}/api/v3/order", self.base_url);
        let resp = self
            .http
            .delete(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(full_body)
            .send()
            .await?;

        let resp = self.check_response(resp).await?;
        Ok(resp.json().await?)
    }
}
