use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::models::ticker::{CombinedStreamWrapper, MiniTickerEvent};

// Los precios son datos públicos: siempre se usa mainnet para el WebSocket.
const MAINNET_WS: &str = "wss://stream.binance.com:9443";

/// Inicia el stream de precios vía WebSocket (@miniTicker).
/// Soporta múltiples símbolos usando el combined stream de Binance.
/// Se reconecta automáticamente en caso de error o cambio en la lista de símbolos.
pub async fn run_price_stream(
    mut symbol_rx: watch::Receiver<Vec<String>>,
    price_tx: mpsc::Sender<MiniTickerEvent>,
) {
    loop {
        let symbols = symbol_rx.borrow_and_update().clone();

        if symbols.is_empty() {
            let _ = symbol_rx.changed().await;
            continue;
        }

        // Combined stream URL:
        // wss://stream.binance.com:9443/stream?streams=btcusdt@miniTicker/ethusdt@miniTicker
        let streams: String = symbols
            .iter()
            .map(|s| format!("{}@miniTicker", s.to_lowercase()))
            .collect::<Vec<_>>()
            .join("/");
        let ws_url = format!("{}/stream?streams={}", MAINNET_WS, streams);

        tracing::info!("Conectando WebSocket ({} símbolo(s))", symbols.len());

        tokio::select! {
            result = connect_and_stream(&ws_url, price_tx.clone()) => {
                match result {
                    Ok(_) => tracing::warn!("WebSocket cerrado, reconectando..."),
                    Err(e) => tracing::error!("WebSocket error: {}, reconectando en 5s...", e),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
            _ = symbol_rx.changed() => {
                tracing::info!("Símbolos cambiados, reconectando WebSocket...");
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

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // Intentar parsear como combined stream wrapper primero
                let event = if let Ok(wrapper) = serde_json::from_str::<CombinedStreamWrapper>(&text) {
                    Some(wrapper.data)
                } else if let Ok(event) = serde_json::from_str::<MiniTickerEvent>(&text) {
                    Some(event)
                } else {
                    tracing::warn!("JSON no reconocido: {}", &text[..text.len().min(120)]);
                    None
                };

                if let Some(event) = event {
                    let _ = price_tx.try_send(event);
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
