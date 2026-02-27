mod api;
mod app;
mod config;
mod models;
mod strategy;
mod ui;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch, Mutex};

use api::client::BinanceClient;
use api::websocket;
use app::{AppCommand, AppState, SaleResult, UiMode, SYMBOLS};
use config::Config;
use models::ticker::MiniTickerEvent;
use strategy::dca::{DcaState, DcaStrategy, StrategySnapshot};
use ui::tui::Tui;

#[tokio::main]
async fn main() -> Result<()> {
    // Redirigir logs a archivo junto al ejecutable, para no interferir con el TUI
    let log_path = config::exe_dir().join("tradingbot.log");
    let log_file = std::fs::File::create(&log_path)?;
    tracing_subscriber::fmt()
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    tracing::info!("Iniciando Binance DCA Bot...");

    // Cargar configuración
    let (config, config_path) = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\n❌ Error de configuración:\n   {}\n", e);
            eprintln!("📝 Edita config.toml con tus claves de API de Binance");
            std::process::exit(1);
        }
    };

    // Ruta del archivo de estado persistente (junto al ejecutable)
    let state_path = config::exe_dir().join("strategy_state.json");

    // Parsear par de trading (ej: BTCUSDT → base=BTC, quote=USDT)
    let (base_asset, quote_asset) = parse_symbol(&config.dca.symbol);
    tracing::info!("Par: {} ({}/{})", config.dca.symbol, base_asset, quote_asset);

    // Crear cliente REST de Binance
    let client = Arc::new(BinanceClient::new(config.binance.clone())?);

    // Test de conectividad
    client.ping().await.map_err(|e| {
        anyhow::anyhow!("No se puede conectar con Binance: {}", e)
    })?;
    tracing::info!("Conectividad OK");

    // Sincronizar reloj con Binance para evitar error -1021
    client.sync_time().await.map_err(|e| {
        anyhow::anyhow!("No se pudo sincronizar tiempo con Binance: {}", e)
    })?;

    // Índice inicial del símbolo en la lista de SYMBOLS
    let initial_symbol_idx = SYMBOLS
        .iter()
        .position(|&s| s == config.dca.symbol)
        .unwrap_or(0);

    // Crear estrategia y restaurar historial si existe un estado guardado
    let mut strategy = DcaStrategy::new(config.dca.clone());
    let restored_trades = if let Some(snapshot) = StrategySnapshot::load(&state_path) {
        if snapshot.symbol == config.dca.symbol {
            let count = snapshot.trades.len();
            strategy.restore_from_snapshot(snapshot);
            tracing::info!("Estado restaurado: {} compras anteriores", count);
            count
        } else {
            tracing::info!("Símbolo cambiado, ignorando estado anterior");
            0
        }
    } else {
        0
    };

    // Estado compartido
    let state = Arc::new(Mutex::new(AppState {
        current_price: 0.0,
        change_24h_pct: 0.0,
        high_24h: 0.0,
        low_24h: 0.0,
        strategy,
        base_asset: base_asset.clone(),
        quote_asset: quote_asset.clone(),
        base_balance: 0.0,
        quote_balance: 0.0,
        log: VecDeque::new(),
        should_quit: false,
        ui_mode: UiMode::Normal,
        cfg_tab: 0,
        cfg_symbol_idx: initial_symbol_idx,
        cfg_amount_buf: String::new(),
        confirm_auto_restart: config.dca.auto_restart,
    }));

    // Si hay trades restaurados, mostrar modal de continuación
    if restored_trades > 0 {
        state.lock().await.ui_mode = UiMode::RestoreSession(restored_trades);
    }

    // Canal de precios (WebSocket → estrategia + UI)
    let (price_tx, price_rx) = mpsc::channel::<MiniTickerEvent>(200);

    // Canal de comandos (UI → motor)
    let (cmd_tx, cmd_rx) = mpsc::channel::<AppCommand>(16);

    // Canal watch para cambio de símbolo en tiempo real
    let (symbol_tx, symbol_rx) = watch::channel(config.dca.symbol.clone());

    // ----------------------------------------------------------------
    // Tarea 1: WebSocket de precios (se reconecta automáticamente)
    // ----------------------------------------------------------------
    tokio::spawn(async move {
        websocket::run_price_stream(symbol_rx, price_tx).await;
    });

    // ----------------------------------------------------------------
    // Tarea 2: Motor de estrategia DCA
    // ----------------------------------------------------------------
    {
        let state_ref = Arc::clone(&state);
        let client_ref = Arc::clone(&client);
        let symbol = config.dca.symbol.clone();
        let max_daily = config.risk.max_daily_spend;

        tokio::spawn(run_strategy_engine(
            state_ref,
            client_ref,
            price_rx,
            cmd_rx,
            symbol,
            config_path,
            state_path,
            max_daily,
            symbol_tx,
        ));
    }

    // ----------------------------------------------------------------
    // Tarea principal: TUI (bloquea el hilo principal)
    // ----------------------------------------------------------------
    let mut tui = Tui::new(Arc::clone(&state), cmd_tx)?;
    tui.run().await?;

    tracing::info!("Bot detenido.");
    Ok(())
}

/// Motor principal de la estrategia DCA
async fn run_strategy_engine(
    state: Arc<Mutex<AppState>>,
    client: Arc<BinanceClient>,
    mut price_rx: mpsc::Receiver<MiniTickerEvent>,
    mut cmd_rx: mpsc::Receiver<AppCommand>,
    mut symbol: String,
    config_path: std::path::PathBuf,
    state_path: std::path::PathBuf,
    max_daily: f64,
    symbol_tx: watch::Sender<String>,
) {
    let mut strategy_tick = tokio::time::interval(Duration::from_secs(1));
    let mut balance_tick = tokio::time::interval(Duration::from_secs(30));

    // Primera actualización de balance
    refresh_balance(&state, &client).await;

    loop {
        tokio::select! {
            // Evento de precio del WebSocket
            Some(event) = price_rx.recv() => {
                let mut s = state.lock().await;
                s.current_price = event.close_f64();
                s.change_24h_pct = event.change_pct();
                s.high_24h = event.high_price.parse().unwrap_or(s.high_24h);
                s.low_24h = event.low_price.parse().unwrap_or(s.low_24h);
            }

            // Comandos del UI
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    AppCommand::RestoreSessionContinue => {
                        let mut s = state.lock().await;
                        let count = s.strategy.trades.len();
                        s.log(&format!("Sesión anterior restaurada: {} compra(s) previa(s)", count));
                        s.ui_mode = UiMode::Normal;
                    }
                    AppCommand::RestoreSessionDiscard => {
                        let mut s = state.lock().await;
                        s.strategy.clear_trades();
                        s.log("Sesión anterior descartada. Comenzando desde cero.");
                        s.ui_mode = UiMode::Normal;
                    }
                    AppCommand::OpenConfirmStart => {
                        let mut s = state.lock().await;
                        // Pre-cargar la selección con el valor actual del config
                        s.confirm_auto_restart = s.strategy.config.auto_restart;
                        s.ui_mode = UiMode::ConfirmStart;
                    }
                    AppCommand::ConfirmToggleAutoRestart => {
                        let mut s = state.lock().await;
                        s.confirm_auto_restart = !s.confirm_auto_restart;
                    }
                    AppCommand::Start => {
                        let mut s = state.lock().await;
                        s.strategy.config.auto_restart = s.confirm_auto_restart;
                        s.ui_mode = UiMode::Normal;
                        s.strategy.start();
                        let mode = if s.confirm_auto_restart { "automático" } else { "manual" };
                        s.log(&format!("Estrategia DCA iniciada (reinicio: {})", mode));
                    }
                    AppCommand::Stop => {
                        let mut s = state.lock().await;
                        s.strategy.stop();
                        s.log("Estrategia DCA detenida");
                    }
                    AppCommand::Quit => {
                        state.lock().await.should_quit = true;
                        break;
                    }

                    // --- Panel de configuración ---
                    AppCommand::OpenConfig => {
                        let mut s = state.lock().await;
                        // Pre-cargar el buffer de monto con el valor actual
                        let amt = s.strategy.config.quote_amount;
                        s.cfg_amount_buf = format!("{}", amt);
                        s.ui_mode = UiMode::Config;
                    }
                    AppCommand::CloseConfig => {
                        state.lock().await.ui_mode = UiMode::Normal;
                    }
                    AppCommand::CfgTabNext => {
                        let mut s = state.lock().await;
                        s.cfg_tab = (s.cfg_tab + 1) % 2;
                    }
                    AppCommand::CfgNavUp => {
                        let mut s = state.lock().await;
                        let len = SYMBOLS.len();
                        s.cfg_symbol_idx = if s.cfg_symbol_idx == 0 { len - 1 } else { s.cfg_symbol_idx - 1 };
                    }
                    AppCommand::CfgNavDown => {
                        let mut s = state.lock().await;
                        s.cfg_symbol_idx = (s.cfg_symbol_idx + 1) % SYMBOLS.len();
                    }
                    AppCommand::CfgInputChar(c) => {
                        let mut s = state.lock().await;
                        // Solo permitir dígitos y un punto decimal
                        if c.is_ascii_digit() || (c == '.' && !s.cfg_amount_buf.contains('.')) {
                            s.cfg_amount_buf.push(c);
                        }
                    }
                    AppCommand::CfgBackspace => {
                        let mut s = state.lock().await;
                        s.cfg_amount_buf.pop();
                    }
                    AppCommand::PostSaleRestart => {
                        let mut s = state.lock().await;
                        s.ui_mode = UiMode::Normal;
                        s.strategy.start();
                        s.log("Ciclo DCA reiniciado");
                    }
                    AppCommand::PostSaleDismiss => {
                        state.lock().await.ui_mode = UiMode::Normal;
                    }

                    AppCommand::CfgConfirm => {
                        let tab = state.lock().await.cfg_tab;
                        if tab == 0 {
                            // Cambiar símbolo
                            let new_symbol = {
                                let s = state.lock().await;
                                SYMBOLS[s.cfg_symbol_idx].to_string()
                            };
                            if new_symbol != symbol {
                                symbol = new_symbol.clone();
                                let (base, quote) = parse_symbol(&new_symbol);
                                let (current_symbol, current_amount) = {
                                    let mut s = state.lock().await;
                                    s.base_asset = base;
                                    s.quote_asset = quote;
                                    s.current_price = 0.0;
                                    s.change_24h_pct = 0.0;
                                    s.high_24h = 0.0;
                                    s.low_24h = 0.0;
                                    s.strategy.clear_trades();
                                    s.strategy.stop();
                                    s.strategy.config.symbol = new_symbol.clone();
                                    s.ui_mode = UiMode::Normal;
                                    s.log(&format!("Símbolo cambiado a {}", new_symbol));
                                    (new_symbol.clone(), s.strategy.config.quote_amount)
                                };
                                // Guardar en config.toml
                                if let Err(e) = Config::save_dca(&config_path, &current_symbol, current_amount) {
                                    state.lock().await.log_error(&format!("No se pudo guardar config: {}", e));
                                }
                                // Guardar snapshot vacío para el nuevo símbolo
                                {
                                    let s = state.lock().await;
                                    let snap = s.strategy.to_snapshot(&current_symbol);
                                    if let Err(e) = snap.save(&state_path) {
                                        tracing::warn!("No se pudo guardar estado: {}", e);
                                    }
                                }
                                // Señalar al WebSocket que reconecte con el nuevo símbolo
                                let _ = symbol_tx.send(new_symbol);
                                // Actualizar balance para el nuevo par
                                refresh_balance(&state, &client).await;
                            } else {
                                state.lock().await.ui_mode = UiMode::Normal;
                            }
                        } else {
                            // Cambiar monto
                            let (amount, buf) = {
                                let s = state.lock().await;
                                (s.cfg_amount_buf.parse::<f64>().ok(), s.cfg_amount_buf.clone())
                            };
                            match amount {
                                Some(v) if v >= 1.0 => {
                                    let current_symbol = {
                                        let mut s = state.lock().await;
                                        s.strategy.config.quote_amount = v;
                                        s.ui_mode = UiMode::Normal;
                                        s.log(&format!("Monto por compra: ${:.2} USDT", v));
                                        s.strategy.config.symbol.clone()
                                    };
                                    // Guardar en config.toml
                                    if let Err(e) = Config::save_dca(&config_path, &current_symbol, v) {
                                        state.lock().await.log_error(&format!("No se pudo guardar config: {}", e));
                                    }
                                }
                                _ => {
                                    state.lock().await.log_error(&format!(
                                        "Monto inválido: '{}' (mínimo $1)", buf
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            // Tick de estrategia (cada 1 segundo)
            _ = strategy_tick.tick() => {
                evaluate_strategy(&state, &client, &symbol, max_daily, &state_path).await;
            }

            // Actualización periódica de balances (cada 30s)
            _ = balance_tick.tick() => {
                refresh_balance(&state, &client).await;
            }
        }
    }
}

/// Evalúa las condiciones de la estrategia y ejecuta órdenes si corresponde
async fn evaluate_strategy(
    state: &Arc<Mutex<AppState>>,
    client: &Arc<BinanceClient>,
    symbol: &str,
    max_daily: f64,
    state_path: &std::path::Path,
) {
    let (price, should_buy, should_tp, should_sl, should_trailing_tp, qty, amount, pnl, pnl_pct, auto_restart) = {
        let mut s = state.lock().await;
        let now = chrono::Utc::now();
        s.strategy.tick(now);

        let price = s.current_price;
        if price == 0.0 {
            return; // Aún no tenemos precio
        }

        s.strategy.update_price_peak(price);

        let buy = s.strategy.should_buy(price, now, max_daily);
        let tp = s.strategy.should_take_profit(price);
        let sl = s.strategy.should_stop_loss(price);
        let trailing_tp = s.strategy.should_trailing_tp(price);
        let qty = s.strategy.total_quantity();
        let amount = s.strategy.config.quote_amount;
        let pnl = s.strategy.pnl(price);
        let pnl_pct = s.strategy.pnl_pct(price);
        let auto_restart = s.strategy.config.auto_restart;
        (price, buy, tp, sl, trailing_tp, qty, amount, pnl, pnl_pct, auto_restart)
    };

    // --- Stop Loss (prioridad máxima) ---
    if should_sl && qty > 0.0 {
        {
            let mut s = state.lock().await;
            s.log(&format!(
                "⚠ STOP LOSS activado! Vendiendo {:.6} @ ${:.2}",
                qty, price
            ));
        }
        match client.market_sell_qty(symbol, qty).await {
            Ok(order) => {
                let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(0.0);
                let snap = {
                    let mut s = state.lock().await;
                    s.strategy.state = DcaState::StopLossReached;
                    s.strategy.stop();
                    s.strategy.clear_trades();
                    s.log(&format!("✓ STOP LOSS ejecutado. Recibido: ${:.2}", received));
                    s.ui_mode = UiMode::PostSale(SaleResult {
                        kind: "STOP LOSS".to_string(),
                        received,
                        pnl,
                        pnl_pct,
                    });
                    s.strategy.to_snapshot(symbol)
                };
                if let Err(e) = snap.save(state_path) {
                    tracing::warn!("No se pudo guardar estado: {}", e);
                }
            }
            Err(e) => {
                state.lock().await.log_error(&format!("Stop loss falló: {}", e));
            }
        }
        return;
    }

    // --- Take Profit ---
    if should_tp && qty > 0.0 {
        {
            let mut s = state.lock().await;
            let pnl = s.strategy.pnl(price);
            s.log(&format!(
                "✓ TAKE PROFIT! P&L: +${:.2} Vendiendo {:.6} @ ${:.2}",
                pnl, qty, price
            ));
        }
        match client.market_sell_qty(symbol, qty).await {
            Ok(order) => {
                let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(0.0);
                let snap = {
                    let mut s = state.lock().await;
                    s.strategy.state = DcaState::TakeProfitReached;
                    s.strategy.clear_trades();
                    s.log(&format!("✓ TAKE PROFIT ejecutado. Recibido: ${:.2}", received));
                    if auto_restart {
                        s.strategy.start();
                        s.log("Auto-reinicio activado. Ciclo DCA reiniciado.");
                    } else {
                        s.strategy.stop();
                        s.ui_mode = UiMode::PostSale(SaleResult {
                            kind: "TAKE PROFIT".to_string(),
                            received,
                            pnl,
                            pnl_pct,
                        });
                    }
                    s.strategy.to_snapshot(symbol)
                };
                if let Err(e) = snap.save(state_path) {
                    tracing::warn!("No se pudo guardar estado: {}", e);
                }
            }
            Err(e) => {
                state.lock().await.log_error(&format!("Take profit falló: {}", e));
            }
        }
        return;
    }

    // --- Trailing Take Profit ---
    if should_trailing_tp && qty > 0.0 {
        {
            let mut s = state.lock().await;
            let pnl = s.strategy.pnl(price);
            let peak = s.strategy.price_peak;
            let drop = ((peak - price) / peak) * 100.0;
            s.log(&format!(
                "↓ TRAILING TP! Máximo: ${:.4}  Caída: {:.2}%  P&L: +${:.2}  Vendiendo {:.6} @ ${:.4}",
                peak, drop, pnl, qty, price
            ));
        }
        match client.market_sell_qty(symbol, qty).await {
            Ok(order) => {
                let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(0.0);
                let snap = {
                    let mut s = state.lock().await;
                    s.strategy.state = DcaState::TakeProfitReached;
                    s.strategy.clear_trades();
                    s.log(&format!("✓ TRAILING TP ejecutado. Recibido: ${:.2}", received));
                    if auto_restart {
                        s.strategy.start();
                        s.log("Auto-reinicio activado. Ciclo DCA reiniciado.");
                    } else {
                        s.strategy.stop();
                        s.ui_mode = UiMode::PostSale(SaleResult {
                            kind: "TRAILING TP".to_string(),
                            received,
                            pnl,
                            pnl_pct,
                        });
                    }
                    s.strategy.to_snapshot(symbol)
                };
                if let Err(e) = snap.save(state_path) {
                    tracing::warn!("No se pudo guardar estado: {}", e);
                }
            }
            Err(e) => {
                state.lock().await.log_error(&format!("Trailing TP falló: {}", e));
            }
        }
        return;
    }

    // --- Compra DCA ---
    if should_buy {
        {
            let s = state.lock().await;
            let order_num = s.strategy.trades.len() + 1;
            drop(s);
            tracing::info!("Ejecutando compra DCA #{} de ${:.2}", order_num, amount);
        }
        match client.market_buy_quote(symbol, amount).await {
            Ok(order) => {
                let exec_qty: f64 = order.executed_qty.parse().unwrap_or(0.0);
                let cost: f64 = order.cummulative_quote_qty.parse().unwrap_or(amount);
                let actual_price = if exec_qty > 0.0 { cost / exec_qty } else { price };

                let snap = {
                    let mut s = state.lock().await;
                    let num = s.strategy.trades.len() + 1;
                    let base_asset = s.base_asset.clone();
                    s.strategy.record_buy(order.order_id, actual_price, exec_qty, cost);
                    s.log(&format!(
                        "BUY #{}: {:.6} {} @ ${:.4} (${:.2})",
                        num, exec_qty, base_asset, actual_price, cost
                    ));
                    s.strategy.to_snapshot(symbol)
                };
                if let Err(e) = snap.save(state_path) {
                    tracing::warn!("No se pudo guardar estado: {}", e);
                }
            }
            Err(e) => {
                state.lock().await.log_error(&format!("Compra fallida: {}", e));
            }
        }
    }
}

/// Actualiza los balances desde la API
async fn refresh_balance(state: &Arc<Mutex<AppState>>, client: &Arc<BinanceClient>) {
    let (base, quote) = {
        let s = state.lock().await;
        (s.base_asset.clone(), s.quote_asset.clone())
    };

    match client.get_account().await {
        Ok(account) => {
            let mut s = state.lock().await;
            s.base_balance = account.get_free(&base);
            s.quote_balance = account.get_free(&quote);
            tracing::debug!(
                "Balance actualizado: {} {} / {} {}",
                s.base_balance, base, s.quote_balance, quote
            );
        }
        Err(e) => {
            tracing::warn!("No se pudo actualizar balance: {}", e);
        }
    }
}

/// Extrae base y quote asset de un símbolo de Binance
/// Ej: "BTCUSDT" → ("BTC", "USDT")
fn parse_symbol(symbol: &str) -> (String, String) {
    const QUOTE_ASSETS: &[&str] = &["USDT", "BUSD", "USDC", "TUSD", "BTC", "ETH", "BNB", "DAI"];
    for qa in QUOTE_ASSETS {
        if symbol.ends_with(qa) && symbol.len() > qa.len() {
            let base = &symbol[..symbol.len() - qa.len()];
            return (base.to_string(), qa.to_string());
        }
    }
    // fallback: dividir por la mitad
    let mid = symbol.len() / 2;
    (symbol[..mid].to_string(), symbol[mid..].to_string())
}
