use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::models::ticker::MiniTickerEvent;

// El testnet de Binance no soporta streams de mercado (@miniTicker).
// Los precios son datos públicos: siempre se usa mainnet para el WebSocket.
const MAINNET_WS: &str = "wss://stream.binance.com:9443/ws";

/// Inicia el stream de precios vía WebSocket (@miniTicker).
/// Se reconecta automáticamente en caso de error o cambio de símbolo.
pub async fn run_price_stream(
    mut symbol_rx: watch::Receiver<String>,
    price_tx: mpsc::Sender<MiniTickerEvent>,
) {
    loop {
        let symbol = symbol_rx.borrow_and_update().clone();
        let ws_url = format!("{}/{}@miniTicker", MAINNET_WS, symbol.to_lowercase());

        tracing::info!("Conectando WebSocket: {}", ws_url);

        tokio::select! {
            result = connect_and_stream(&ws_url, price_tx.clone()) => {
                match result {
                    Ok(_) => tracing::warn!("WebSocket cerrado limpiamente, reconectando..."),
                    Err(e) => tracing::error!("WebSocket error: {}, reconectando en 5s...", e),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
            _ = symbol_rx.changed() => {
                tracing::info!("Símbolo cambiado, reconectando WebSocket...");
                // reconecta inmediatamente en la siguiente iteración del loop
            }
        }
    }
}

async fn connect_and_stream(
    ws_url: &str,
    price_tx: mpsc::Sender<MiniTickerEvent>,
) -> Result<()> {
    let (ws_stream, _response) = connect_async(ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    tracing::info!("WebSocket conectado");

    // Binance cierra la conexión después de 24h; necesitamos responder pings
    // y Binance también envía pings automáticos.
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<MiniTickerEvent>(&text) {
                    Ok(event) => {
                        // Si el canal está lleno, descartamos (no bloqueamos)
                        let _ = price_tx.try_send(event);
                    }
                    Err(e) => {
                        tracing::warn!("JSON parse error: {} | data: {}", e, &text[..text.len().min(100)]);
                    }
                }
            }
            Ok(Message::Ping(data)) => {
                write.send(Message::Pong(data)).await?;
            }
            Ok(Message::Close(_)) => {
                tracing::warn!("WebSocket: servidor cerró la conexión");
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
            _ => {}
        }
    }

    Ok(())
}
