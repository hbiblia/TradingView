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
    ticker::{Kline, TickerPrice},
};

type HmacSha256 = Hmac<Sha256>;

/// Binance base URLs
const MAINNET_URL: &str = "https://api.binance.com";
const TESTNET_URL: &str = "https://testnet.binance.vision";

pub struct BinanceClient {
    http: Client,
    secret: String,
    base_url: String,
    /// Offset in ms between local clock and Binance server
    time_offset_ms: AtomicI64,
}

impl BinanceClient {
    pub fn new(config: BinanceConfig) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "X-MBX-APIKEY",
            header::HeaderValue::from_str(&config.api_key)
                .map_err(|_| anyhow!("Invalid API key"))?,
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
            "Binance client initialized ({})",
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
    // Internal helpers
    // -------------------------------------------------------

    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts keys of any size");
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
        // Try to parse Binance error message
        if let Ok(val) = serde_json::from_str::<Value>(&text) {
            let code = val["code"].as_i64().unwrap_or(0);
            let msg = val["msg"].as_str().unwrap_or(&text);
            Err(anyhow!("Binance error {}: {} (HTTP {})", code, msg, status))
        } else {
            Err(anyhow!("HTTP {}: {}", status, text))
        }
    }

    // -------------------------------------------------------
    // Public endpoints (no signature)
    // -------------------------------------------------------

    /// Connectivity test
    pub async fn ping(&self) -> Result<()> {
        let url = format!("{}/api/v3/ping", self.base_url);
        self.http.get(&url).send().await?;
        Ok(())
    }

    /// Local clock synchronization with Binance server to avoid error -1021.
    /// Calculates the offset and stores it to apply it on each signed timestamp.
    pub async fn sync_time(&self) -> Result<()> {
        let local_before = Utc::now().timestamp_millis();
        let url = format!("{}/api/v3/time", self.base_url);
        let resp: Value = self.http.get(&url).send().await?.json().await?;
        let local_after = Utc::now().timestamp_millis();

        let server_time = resp["serverTime"]
            .as_i64()
            .ok_or_else(|| anyhow!("Invalid Binance time response"))?;

        // Approximate local time when the server processed it
        let local_mid = (local_before + local_after) / 2;
        let offset = server_time - local_mid;

        self.time_offset_ms.store(offset, Ordering::Relaxed);
        tracing::info!("Time sync: offset {}ms (local clock is {}ms off Binance)", offset, offset.abs());
        Ok(())
    }

    /// Gets all active USDT pairs in Spot — public endpoint, no signature.
    /// Returns the list sorted alphabetically.
    pub async fn get_usdt_symbols(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/v3/exchangeInfo", self.base_url);
        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

        let mut symbols: Vec<String> = resp["symbols"]
            .as_array()
            .ok_or_else(|| anyhow!("exchangeInfo: 'symbols' field not found"))?
            .iter()
            .filter_map(|s| {
                let symbol = s["symbol"].as_str()?;
                let status = s["status"].as_str()?;
                let quote  = s["quoteAsset"].as_str()?;
                let is_spot = s["isSpotTradingAllowed"].as_bool().unwrap_or(false);
                let permissions = s["permissions"].as_array()?;
                let is_margin = permissions.iter().any(|p| p == "MARGIN" || p == "ISOLATED_MARGIN");

                if status == "TRADING" && quote == "USDT" && is_spot && is_margin {
                    Some(symbol.to_string())
                } else {
                    None
                }
            })
            .collect();

        symbols.sort();
        Ok(symbols)
    }

    /// Gets historical OHLC candles (klines) — public endpoint, no signature
    /// Returns up to `limit` candles of the indicated `interval` (e.g.: "1h", "4h", "1d")
    pub async fn get_klines(&self, symbol: &str, interval: &str, limit: u32) -> Result<Vec<Kline>> {
        let url = format!(
            "{}/api/v3/klines?symbol={}&interval={}&limit={}",
            self.base_url, symbol, interval, limit
        );
        // API returns Vec<Vec<Value>>; each candle is an array of 12+ elements:
        // [open_time, open, high, low, close, volume, close_time, ...]
        let resp: Vec<serde_json::Value> = self.http.get(&url).send().await?.json().await?;
        let klines = resp
            .into_iter()
            .filter_map(|k| {
                let high: f64 = k.get(2)?.as_str()?.parse().ok()?;
                let low:  f64 = k.get(3)?.as_str()?.parse().ok()?;
                Some(Kline { high, low })
            })
            .collect();
        Ok(klines)
    }

    /// Current price of a symbol
    pub async fn get_price(&self, symbol: &str) -> Result<f64> {
        let url = format!("{}/api/v3/ticker/price?symbol={}", self.base_url, symbol);
        let resp: TickerPrice = self.http.get(&url).send().await?.json().await?;
        resp.price
            .parse::<f64>()
            .map_err(|_| anyhow!("Invalid price: {}", resp.price))
    }

    // -------------------------------------------------------
    // Private endpoints (require HMAC-SHA256 signature)
    // -------------------------------------------------------

    /// Account info (balances, permissions)
    pub async fn get_account(&self) -> Result<AccountInfo> {
        let ts = self.timestamp_ms();
        let query = format!("timestamp={}", ts);
        let sig = self.sign(&query);
        let url = format!("{}/api/v3/account?{}&signature={}", self.base_url, query, sig);

        let resp = self.http.get(&url).send().await?;
        let resp = self.check_response(resp).await?;
        Ok(resp.json::<AccountInfo>().await?)
    }

    /// Market buy order using quoteOrderQty (monto en USDT)
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

    /// Market buy order using quantity (exact base quantity, e.g.: BTC)
    /// Used to close SHORT positions: rebuy the exact quantity sold
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

    /// Market sell order using quantity (base quantity, e.g.: BTC)
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

    /// Cancels an order by ID
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
