mod api;
mod app;
mod config;
mod models;
mod strategy;
mod ui;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch, Mutex};

use api::client::BinanceClient;
use api::websocket;
use app::{AppCommand, AppState, SaleResult, StrategySlot, UiMode, SYMBOLS, MAX_SLOTS};
use config::{Config, Direction, DcaConfig};
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

    // Ruta del archivo de estado persistente
    let state_path = config::exe_dir().join("strategy_state.json");

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

    // Cargar snapshots anteriores
    let snapshots = load_snapshots(&state_path);

    // Crear los slots iniciales
    let mut slots: Vec<StrategySlot> = Vec::new();
    let mut next_id = 0usize;
    let mut restore_info: Vec<(String, Direction, usize)> = Vec::new();

    if !snapshots.is_empty() {
        // Restaurar desde snapshots previos
        for snap in &snapshots {
            if slots.len() >= MAX_SLOTS {
                break;
            }
            let (base, quote) = parse_symbol(&snap.symbol);
            let mut strat_config = config.dca.clone();
            strat_config.symbol = snap.symbol.clone();
            strat_config.direction = snap.direction.clone();
            let mut strat = DcaStrategy::new(strat_config);
            let trade_count = snap.trades.len();
            strat.restore_from_snapshot(snap.clone());

            restore_info.push((snap.symbol.clone(), snap.direction.clone(), trade_count));

            slots.push(StrategySlot {
                id: next_id,
                strategy: strat,
                symbol: snap.symbol.clone(),
                base_asset: base,
                quote_asset: quote,
                base_balance: 0.0,
                quote_balance: 0.0,
            });
            next_id += 1;
        }
    } else {
        // Crear slot inicial desde config
        let (base, quote) = parse_symbol(&config.dca.symbol);
        let strat = DcaStrategy::new(config.dca.clone());
        slots.push(StrategySlot {
            id: next_id,
            strategy: strat,
            symbol: config.dca.symbol.clone(),
            base_asset: base,
            quote_asset: quote,
            base_balance: 0.0,
            quote_balance: 0.0,
        });
        next_id += 1;
    }

    // Símbolos activos para WebSocket
    let initial_symbols: Vec<String> = slots.iter().map(|s| s.symbol.clone()).collect();

    let ui_mode = if restore_info.iter().any(|(_, _, c)| *c > 0) {
        UiMode::RestoreSession(restore_info)
    } else {
        UiMode::Normal
    };

    let state = Arc::new(Mutex::new(AppState {
        slots,
        selected_slot: 0,
        prices: HashMap::new(),
        log: std::collections::VecDeque::new(),
        should_quit: false,
        ui_mode,
        new_strat_symbol_idx: 0,
        new_strat_direction: Direction::Long,
        new_strat_auto_restart: config.dca.auto_restart,
        cfg_amount_buf: String::new(),
        next_slot_id: next_id,
    }));

    // Canal de precios (WebSocket → motor)
    let (price_tx, price_rx) = mpsc::channel::<MiniTickerEvent>(200);

    // Canal de comandos (UI → motor)
    let (cmd_tx, cmd_rx) = mpsc::channel::<AppCommand>(16);

    // Canal watch para la lista de símbolos activos
    let (symbol_tx, symbol_rx) = watch::channel::<Vec<String>>(initial_symbols);

    // ----------------------------------------------------------------
    // Tarea 1: WebSocket de precios (se reconecta automáticamente)
    // ----------------------------------------------------------------
    tokio::spawn(async move {
        websocket::run_price_stream(symbol_rx, price_tx).await;
    });

    // ----------------------------------------------------------------
    // Tarea 2: Motor de estrategia multi-slot
    // ----------------------------------------------------------------
    {
        let state_ref = Arc::clone(&state);
        let client_ref = Arc::clone(&client);
        let max_daily = config.risk.max_daily_spend;
        let dca_config = config.dca.clone();

        tokio::spawn(run_strategy_engine(
            state_ref,
            client_ref,
            price_rx,
            cmd_rx,
            config_path,
            state_path,
            max_daily,
            dca_config,
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

/// Motor principal multi-slot de la estrategia DCA
async fn run_strategy_engine(
    state: Arc<Mutex<AppState>>,
    client: Arc<BinanceClient>,
    mut price_rx: mpsc::Receiver<MiniTickerEvent>,
    mut cmd_rx: mpsc::Receiver<AppCommand>,
    config_path: std::path::PathBuf,
    state_path: std::path::PathBuf,
    max_daily: f64,
    base_config: DcaConfig,
    symbol_tx: watch::Sender<Vec<String>>,
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
                let sym = event.symbol.clone();
                let entry = s.prices.entry(sym).or_default();
                entry.price = event.close_f64();
                entry.change_24h_pct = event.change_pct();
                entry.high_24h = event.high_price.parse().unwrap_or(entry.high_24h);
                entry.low_24h = event.low_price.parse().unwrap_or(entry.low_24h);
            }

            // Comandos del UI
            Some(cmd) = cmd_rx.recv() => {
                handle_command(
                    cmd,
                    &state,
                    &client,
                    &config_path,
                    &state_path,
                    &base_config,
                    &symbol_tx,
                ).await;
                if state.lock().await.should_quit {
                    break;
                }
            }

            // Tick de estrategia (cada 1 segundo): evalúa todos los slots
            _ = strategy_tick.tick() => {
                let ids: Vec<usize> = state.lock().await.slots.iter().map(|s| s.id).collect();
                for id in ids {
                    evaluate_slot(&state, &client, id, max_daily, &state_path).await;
                }
            }

            // Actualización periódica de balances (cada 30s)
            _ = balance_tick.tick() => {
                refresh_balance(&state, &client).await;
            }
        }
    }
}

/// Procesa un comando del UI
async fn handle_command(
    cmd: AppCommand,
    state: &Arc<Mutex<AppState>>,
    client: &Arc<BinanceClient>,
    config_path: &std::path::Path,
    state_path: &std::path::Path,
    base_config: &DcaConfig,
    symbol_tx: &watch::Sender<Vec<String>>,
) {
    match cmd {
        AppCommand::Quit => {
            state.lock().await.should_quit = true;
        }

        // --- Restauración de sesión ---
        AppCommand::RestoreSessionContinue => {
            let mut s = state.lock().await;
            s.log("Sesiones anteriores restauradas. Actívalas manualmente con [S].");
            s.ui_mode = UiMode::Normal;
        }
        AppCommand::RestoreSessionDiscard => {
            {
                let mut s = state.lock().await;
                s.slots.clear();
                s.selected_slot = 0;
                let (base, quote) = parse_symbol(&base_config.symbol);
                let strat = DcaStrategy::new(base_config.clone());
                let id = s.alloc_slot_id();
                s.slots.push(StrategySlot {
                    id,
                    strategy: strat,
                    symbol: base_config.symbol.clone(),
                    base_asset: base,
                    quote_asset: quote,
                    base_balance: 0.0,
                    quote_balance: 0.0,
                });
                s.log("Sesión anterior descartada. Comenzando desde cero.");
                s.ui_mode = UiMode::Normal;
            }
            update_symbol_watch(state, symbol_tx).await;
            save_all_snapshots(state, state_path).await;
            refresh_balance(state, client).await;
        }

        // --- Navegación de slots ---
        AppCommand::SlotSelectUp => {
            let mut s = state.lock().await;
            if s.selected_slot > 0 {
                s.selected_slot -= 1;
            }
        }
        AppCommand::SlotSelectDown => {
            let mut s = state.lock().await;
            let len = s.slots.len();
            if s.selected_slot + 1 < len {
                s.selected_slot += 1;
            }
        }

        // --- Detener slot seleccionado ---
        AppCommand::StopSelected => {
            let mut s = state.lock().await;
            if let Some(slot) = s.selected_mut() {
                slot.strategy.stop();
            }
            s.log("Estrategia detenida.");
        }

        // --- Modal nueva estrategia (S) ---
        AppCommand::OpenNewStrategy => {
            let mut s = state.lock().await;
            // Pre-seleccionar un símbolo no usado
            let used: Vec<String> = s.slots.iter().map(|sl| sl.symbol.clone()).collect();
            let idx = SYMBOLS
                .iter()
                .position(|&sym| !used.contains(&sym.to_string()))
                .unwrap_or(0);
            s.new_strat_symbol_idx = idx;
            s.new_strat_direction = Direction::Long;
            s.new_strat_auto_restart = base_config.auto_restart;
            s.ui_mode = UiMode::NewStrategy;
        }
        AppCommand::NewStratSymbolUp => {
            let mut s = state.lock().await;
            let len = SYMBOLS.len();
            s.new_strat_symbol_idx =
                if s.new_strat_symbol_idx == 0 { len - 1 } else { s.new_strat_symbol_idx - 1 };
        }
        AppCommand::NewStratSymbolDown => {
            let mut s = state.lock().await;
            s.new_strat_symbol_idx = (s.new_strat_symbol_idx + 1) % SYMBOLS.len();
        }
        AppCommand::NewStratToggleDirection => {
            let mut s = state.lock().await;
            s.new_strat_direction = match s.new_strat_direction {
                Direction::Long  => Direction::Short,
                Direction::Short => Direction::Long,
            };
        }
        AppCommand::NewStratToggleAutoRestart => {
            let mut s = state.lock().await;
            s.new_strat_auto_restart = !s.new_strat_auto_restart;
        }
        AppCommand::NewStratCancel => {
            state.lock().await.ui_mode = UiMode::Normal;
        }
        AppCommand::NewStratConfirm => {
            let (symbol, direction, auto_restart, can_add) = {
                let s = state.lock().await;
                let sym = SYMBOLS[s.new_strat_symbol_idx].to_string();
                let dir = s.new_strat_direction.clone();
                let ar = s.new_strat_auto_restart;
                let can = s.slots.len() < MAX_SLOTS;
                (sym, dir, ar, can)
            };

            if !can_add {
                state.lock().await.log_error("Máximo de estrategias alcanzado (4).");
                return;
            }

            let (base, quote) = parse_symbol(&symbol);
            let mut cfg = base_config.clone();
            cfg.symbol = symbol.clone();
            cfg.direction = direction.clone();
            cfg.auto_restart = auto_restart;
            let mut strat = DcaStrategy::new(cfg);
            strat.start();

            {
                let mut s = state.lock().await;
                let id = s.alloc_slot_id();
                let dir_label = match direction {
                    Direction::Long  => "LONG",
                    Direction::Short => "SHORT",
                };
                s.log(&format!("Nueva estrategia: {} {} iniciada", symbol, dir_label));
                s.slots.push(StrategySlot {
                    id,
                    strategy: strat,
                    symbol: symbol.clone(),
                    base_asset: base,
                    quote_asset: quote,
                    base_balance: 0.0,
                    quote_balance: 0.0,
                });
                s.selected_slot = s.slots.len() - 1;
                s.ui_mode = UiMode::Normal;
            }

            update_symbol_watch(state, symbol_tx).await;
            save_all_snapshots(state, state_path).await;
            refresh_balance(state, client).await;
        }

        // --- Post-venta ---
        AppCommand::PostSaleRestart(slot_id) => {
            let mut s = state.lock().await;
            if let Some(slot) = s.slot_by_id_mut(slot_id) {
                slot.strategy.start();
            }
            s.ui_mode = UiMode::Normal;
            s.log("Ciclo DCA reiniciado.");
        }
        AppCommand::PostSaleDismiss(slot_id) => {
            let mut s = state.lock().await;
            if let UiMode::PostSale(id, _) = &s.ui_mode {
                if *id == slot_id {
                    s.ui_mode = UiMode::Normal;
                }
            }
        }

        // --- Panel de configuración (solo monto) ---
        AppCommand::OpenConfig => {
            let mut s = state.lock().await;
            let amt = s
                .selected()
                .map(|sl| sl.strategy.config.quote_amount)
                .unwrap_or(base_config.quote_amount);
            s.cfg_amount_buf = format!("{}", amt);
            s.ui_mode = UiMode::Config;
        }
        AppCommand::CloseConfig => {
            state.lock().await.ui_mode = UiMode::Normal;
        }
        AppCommand::CfgInputChar(c) => {
            let mut s = state.lock().await;
            if c.is_ascii_digit() || (c == '.' && !s.cfg_amount_buf.contains('.')) {
                s.cfg_amount_buf.push(c);
            }
        }
        AppCommand::CfgBackspace => {
            state.lock().await.cfg_amount_buf.pop();
        }
        // --- Cierre manual de posición ---
        AppCommand::OpenConfirmClose => {
            let mut s = state.lock().await;
            let has_position = s
                .selected()
                .map(|sl| sl.strategy.total_quantity() > 0.0)
                .unwrap_or(false);
            if has_position {
                s.ui_mode = UiMode::ConfirmClose;
            } else {
                s.log("No hay posición abierta para cerrar.");
            }
        }
        AppCommand::ConfirmCloseNow => {
            let (slot_id, symbol, qty, direction, price, pnl, pnl_pct) = {
                let s = state.lock().await;
                let slot = match s.selected() {
                    Some(sl) => sl,
                    None => {
                        drop(s);
                        state.lock().await.ui_mode = UiMode::Normal;
                        return;
                    }
                };
                let price = s.selected_price();
                (
                    slot.id,
                    slot.symbol.clone(),
                    slot.strategy.total_quantity(),
                    slot.strategy.config.direction.clone(),
                    price,
                    slot.strategy.pnl(price),
                    slot.strategy.pnl_pct(price),
                )
            };

            state.lock().await.ui_mode = UiMode::Normal;

            if qty <= 0.0 {
                state.lock().await.log("No hay posición abierta para cerrar.");
                return;
            }

            let log_msg = match direction {
                Direction::Long  => format!("⚠ CIERRE MANUAL [{}]: Vendiendo {:.6} @ ${:.2}", symbol, qty, price),
                Direction::Short => format!("⚠ CIERRE MANUAL [{}]: Recomprando {:.6} @ ${:.2}", symbol, qty, price),
            };
            state.lock().await.log(&log_msg);

            let order_result = match direction {
                Direction::Long  => client.market_sell_qty(&symbol, qty).await,
                Direction::Short => client.market_buy_qty(&symbol, qty).await,
            };

            match order_result {
                Ok(order) => {
                    let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(0.0);
                    {
                        let mut s = state.lock().await;
                        if let Some(slot) = s.slot_by_id_mut(slot_id) {
                            slot.strategy.stop();
                            slot.strategy.clear_trades();
                        }
                        s.log(&format!(
                            "✓ CIERRE MANUAL [{}] ejecutado. Recibido: ${:.2}",
                            symbol, received
                        ));
                        s.ui_mode = UiMode::PostSale(
                            slot_id,
                            SaleResult {
                                kind: "CIERRE MANUAL".to_string(),
                                received,
                                pnl,
                                pnl_pct,
                            },
                        );
                    }
                    save_all_snapshots(state, state_path).await;
                }
                Err(e) => {
                    state
                        .lock()
                        .await
                        .log_error(&format!("Cierre manual [{}] falló: {}", symbol, e));
                }
            }
        }

        AppCommand::CfgConfirm => {
            let (amount, buf) = {
                let s = state.lock().await;
                (s.cfg_amount_buf.parse::<f64>().ok(), s.cfg_amount_buf.clone())
            };
            match amount {
                Some(v) if v >= 1.0 => {
                    {
                        let mut s = state.lock().await;
                        // Aplicar a todos los slots
                        for slot in s.slots.iter_mut() {
                            slot.strategy.config.quote_amount = v;
                        }
                        s.ui_mode = UiMode::Normal;
                        s.log(&format!("Monto por operación: ${:.2} USDT (todos los slots)", v));
                    }
                    if let Err(e) = Config::save_dca(config_path, &base_config.symbol, v) {
                        state.lock().await.log_error(&format!(
                            "No se pudo guardar config: {}",
                            e
                        ));
                    }
                }
                _ => {
                    state.lock().await.log_error(&format!(
                        "Monto inválido: '{}' (mínimo $1)",
                        buf
                    ));
                }
            }
        }
    }
}

/// Evalúa las condiciones de un slot y ejecuta órdenes si corresponde
async fn evaluate_slot(
    state: &Arc<Mutex<AppState>>,
    client: &Arc<BinanceClient>,
    slot_id: usize,
    max_daily: f64,
    state_path: &std::path::Path,
) {
    let (price, direction, should_entry, should_tp, should_sl, should_trailing_tp,
         qty, amount, pnl, pnl_pct, auto_restart, symbol, price_peak, price_trough) =
    {
        let mut s = state.lock().await;
        let now = chrono::Utc::now();

        // Tick del timer
        if let Some(slot) = s.slot_by_id_mut(slot_id) {
            slot.strategy.tick(now);
        }

        // Obtener símbolo
        let sym = match s.slot_by_id(slot_id) {
            Some(sl) => sl.symbol.clone(),
            None => return,
        };

        // Obtener precio actual
        let price = s.prices.get(&sym).map(|m| m.price).unwrap_or(0.0);
        if price == 0.0 {
            return;
        }

        // Actualizar extremo (peak para LONG, trough para SHORT)
        if let Some(slot) = s.slot_by_id_mut(slot_id) {
            slot.strategy.update_price_peak(price);
        }

        // Leer decisiones y datos del slot
        let slot = match s.slot_by_id(slot_id) {
            Some(sl) => sl,
            None => return,
        };

        let direction      = slot.strategy.config.direction.clone();
        let should_entry   = slot.strategy.should_buy(price, now, max_daily);
        let should_tp      = slot.strategy.should_take_profit(price);
        let should_sl      = slot.strategy.should_stop_loss(price);
        let should_trailing_tp = slot.strategy.should_trailing_tp(price);
        let qty            = slot.strategy.total_quantity();
        let amount         = slot.strategy.config.quote_amount;
        let pnl            = slot.strategy.pnl(price);
        let pnl_pct        = slot.strategy.pnl_pct(price);
        let auto_restart   = slot.strategy.config.auto_restart;
        let symbol         = slot.symbol.clone();
        let price_peak     = slot.strategy.price_peak;
        let price_trough   = slot.strategy.price_trough;

        (price, direction, should_entry, should_tp, should_sl, should_trailing_tp,
         qty, amount, pnl, pnl_pct, auto_restart, symbol, price_peak, price_trough)
    };

    // =====================================================================
    // Stop Loss (prioridad máxima)
    // =====================================================================
    if should_sl && qty > 0.0 {
        let log_msg = match direction {
            Direction::Long  => format!("⚠ STOP LOSS [{}]! Vendiendo {:.6} @ ${:.2}", symbol, qty, price),
            Direction::Short => format!("⚠ STOP LOSS [{}]! Recomprando {:.6} @ ${:.2}", symbol, qty, price),
        };
        state.lock().await.log(&log_msg);

        let order_result = match direction {
            Direction::Long  => client.market_sell_qty(&symbol, qty).await,
            Direction::Short => client.market_buy_qty(&symbol, qty).await,
        };

        match order_result {
            Ok(order) => {
                let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(0.0);
                {
                    let mut s = state.lock().await;
                    if let Some(slot) = s.slot_by_id_mut(slot_id) {
                        slot.strategy.state = DcaState::StopLossReached;
                        slot.strategy.stop();
                        slot.strategy.clear_trades();
                    }
                    s.log(&format!("✓ STOP LOSS [{}] ejecutado. Recibido: ${:.2}", symbol, received));
                    s.ui_mode = UiMode::PostSale(slot_id, SaleResult {
                        kind: "STOP LOSS".to_string(),
                        received,
                        pnl,
                        pnl_pct,
                    });
                }
                save_all_snapshots(state, state_path).await;
            }
            Err(e) => {
                state.lock().await.log_error(&format!("Stop loss [{}] falló: {}", symbol, e));
            }
        }
        return;
    }

    // =====================================================================
    // Take Profit
    // =====================================================================
    if should_tp && qty > 0.0 {
        let log_msg = match direction {
            Direction::Long  => format!("✓ TAKE PROFIT [{}]! P&L: +${:.2}  Vendiendo {:.6} @ ${:.2}", symbol, pnl, qty, price),
            Direction::Short => format!("✓ TAKE PROFIT [{}]! P&L: +${:.2}  Recomprando {:.6} @ ${:.2}", symbol, pnl, qty, price),
        };
        state.lock().await.log(&log_msg);

        let order_result = match direction {
            Direction::Long  => client.market_sell_qty(&symbol, qty).await,
            Direction::Short => client.market_buy_qty(&symbol, qty).await,
        };

        match order_result {
            Ok(order) => {
                let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(0.0);
                {
                    let mut s = state.lock().await;
                    if let Some(slot) = s.slot_by_id_mut(slot_id) {
                        slot.strategy.state = DcaState::TakeProfitReached;
                        slot.strategy.clear_trades();
                        if auto_restart {
                            slot.strategy.start();
                        } else {
                            slot.strategy.stop();
                        }
                    }
                    s.log(&format!("✓ TAKE PROFIT [{}] ejecutado. Recibido: ${:.2}", symbol, received));
                    if auto_restart {
                        s.log("Auto-reinicio activado. Ciclo DCA reiniciado.");
                    } else {
                        s.ui_mode = UiMode::PostSale(slot_id, SaleResult {
                            kind: "TAKE PROFIT".to_string(),
                            received,
                            pnl,
                            pnl_pct,
                        });
                    }
                }
                save_all_snapshots(state, state_path).await;
            }
            Err(e) => {
                state.lock().await.log_error(&format!("Take profit [{}] falló: {}", symbol, e));
            }
        }
        return;
    }

    // =====================================================================
    // Trailing Take Profit
    // =====================================================================
    if should_trailing_tp && qty > 0.0 {
        let log_msg = match direction {
            Direction::Long => {
                let drop = ((price_peak - price) / price_peak) * 100.0;
                format!(
                    "↓ TRAILING TP [{}]! Máx: ${:.4}  Caída: {:.2}%  P&L: +${:.2}",
                    symbol, price_peak, drop, pnl
                )
            }
            Direction::Short => {
                let rise = ((price - price_trough) / price_trough) * 100.0;
                format!(
                    "↑ TRAILING TP [{}]! Mín: ${:.4}  Subida: {:.2}%  P&L: +${:.2}",
                    symbol, price_trough, rise, pnl
                )
            }
        };
        state.lock().await.log(&log_msg);

        let order_result = match direction {
            Direction::Long  => client.market_sell_qty(&symbol, qty).await,
            Direction::Short => client.market_buy_qty(&symbol, qty).await,
        };

        match order_result {
            Ok(order) => {
                let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(0.0);
                {
                    let mut s = state.lock().await;
                    if let Some(slot) = s.slot_by_id_mut(slot_id) {
                        slot.strategy.state = DcaState::TakeProfitReached;
                        slot.strategy.clear_trades();
                        if auto_restart {
                            slot.strategy.start();
                        } else {
                            slot.strategy.stop();
                        }
                    }
                    s.log(&format!("✓ TRAILING TP [{}] ejecutado. Recibido: ${:.2}", symbol, received));
                    if auto_restart {
                        s.log("Auto-reinicio activado. Ciclo DCA reiniciado.");
                    } else {
                        s.ui_mode = UiMode::PostSale(slot_id, SaleResult {
                            kind: "TRAILING TP".to_string(),
                            received,
                            pnl,
                            pnl_pct,
                        });
                    }
                }
                save_all_snapshots(state, state_path).await;
            }
            Err(e) => {
                state.lock().await.log_error(&format!("Trailing TP [{}] falló: {}", symbol, e));
            }
        }
        return;
    }

    // =====================================================================
    // Entrada DCA
    //   LONG:  compra USDT → base asset      (market_buy_quote)
    //   SHORT: vende base asset → recibe USDT (market_sell_qty)
    // =====================================================================
    if should_entry {
        match direction {
            Direction::Long => {
                let order_num = {
                    state.lock().await
                        .slot_by_id(slot_id)
                        .map(|sl| sl.strategy.trades.len() + 1)
                        .unwrap_or(1)
                };
                tracing::info!(
                    "Ejecutando compra DCA LONG [{}] #{} de ${:.2}",
                    symbol, order_num, amount
                );

                match client.market_buy_quote(&symbol, amount).await {
                    Ok(order) => {
                        let exec_qty: f64 = order.executed_qty.parse().unwrap_or(0.0);
                        let cost: f64 = order.cummulative_quote_qty.parse().unwrap_or(amount);
                        let actual_price = if exec_qty > 0.0 { cost / exec_qty } else { price };
                        {
                            let mut s = state.lock().await;
                            if let Some(slot) = s.slot_by_id_mut(slot_id) {
                                let num = slot.strategy.trades.len() + 1;
                                let base = slot.base_asset.clone();
                                slot.strategy.record_buy(order.order_id, actual_price, exec_qty, cost);
                                s.log(&format!(
                                    "BUY #{} [{}]: {:.6} {} @ ${:.4} (${:.2})",
                                    num, symbol, exec_qty, base, actual_price, cost
                                ));
                            }
                        }
                        save_all_snapshots(state, state_path).await;
                    }
                    Err(e) => {
                        state.lock().await.log_error(&format!("Compra [{}] fallida: {}", symbol, e));
                    }
                }
            }

            Direction::Short => {
                let qty_to_sell = if price > 0.0 { amount / price } else { return };
                let order_num = {
                    state.lock().await
                        .slot_by_id(slot_id)
                        .map(|sl| sl.strategy.trades.len() + 1)
                        .unwrap_or(1)
                };
                tracing::info!(
                    "Ejecutando venta DCA SHORT [{}] #{}: {:.6}",
                    symbol, order_num, qty_to_sell
                );

                match client.market_sell_qty(&symbol, qty_to_sell).await {
                    Ok(order) => {
                        let exec_qty: f64 = order.executed_qty.parse().unwrap_or(0.0);
                        let received: f64 = order.cummulative_quote_qty.parse().unwrap_or(amount);
                        let actual_price = if exec_qty > 0.0 { received / exec_qty } else { price };
                        {
                            let mut s = state.lock().await;
                            if let Some(slot) = s.slot_by_id_mut(slot_id) {
                                let num = slot.strategy.trades.len() + 1;
                                let base = slot.base_asset.clone();
                                slot.strategy.record_buy(order.order_id, actual_price, exec_qty, received);
                                s.log(&format!(
                                    "SHORT #{} [{}]: vendido {:.6} {} @ ${:.4} (${:.2})",
                                    num, symbol, exec_qty, base, actual_price, received
                                ));
                            }
                        }
                        save_all_snapshots(state, state_path).await;
                    }
                    Err(e) => {
                        state.lock().await.log_error(&format!(
                            "Venta SHORT [{}] fallida: {}",
                            symbol, e
                        ));
                    }
                }
            }
        }
    }
}

/// Actualiza el canal watch con la lista actual de símbolos
async fn update_symbol_watch(
    state: &Arc<Mutex<AppState>>,
    symbol_tx: &watch::Sender<Vec<String>>,
) {
    let symbols: Vec<String> = state.lock().await.slots.iter().map(|s| s.symbol.clone()).collect();
    let _ = symbol_tx.send(symbols);
}

/// Guarda todos los slots como Vec<StrategySnapshot>
async fn save_all_snapshots(state: &Arc<Mutex<AppState>>, path: &std::path::Path) {
    let snapshots: Vec<StrategySnapshot> = {
        let s = state.lock().await;
        s.slots.iter().map(|sl| sl.strategy.to_snapshot(&sl.symbol)).collect()
    };
    if let Err(e) = save_snapshots(&snapshots, path) {
        tracing::warn!("No se pudo guardar estado: {}", e);
    }
}

/// Actualiza los balances de todos los slots con una sola llamada a la API
async fn refresh_balance(state: &Arc<Mutex<AppState>>, client: &Arc<BinanceClient>) {
    match client.get_account().await {
        Ok(account) => {
            let mut s = state.lock().await;
            for slot in s.slots.iter_mut() {
                slot.base_balance = account.get_free(&slot.base_asset);
                slot.quote_balance = account.get_free(&slot.quote_asset);
            }
            tracing::debug!("Balances actualizados para {} slot(s)", s.slots.len());
        }
        Err(e) => {
            tracing::warn!("No se pudo actualizar balance: {}", e);
        }
    }
}

/// Carga snapshots desde disco (array JSON o single object para compatibilidad)
fn load_snapshots(path: &std::path::Path) -> Vec<StrategySnapshot> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    // Intentar array primero (nuevo formato)
    if let Ok(snaps) = serde_json::from_str::<Vec<StrategySnapshot>>(&content) {
        return snaps;
    }
    // Fallback: single object (formato anterior de una sola estrategia)
    if let Ok(snap) = serde_json::from_str::<StrategySnapshot>(&content) {
        return vec![snap];
    }
    vec![]
}

/// Guarda Vec<StrategySnapshot> como JSON
fn save_snapshots(snapshots: &[StrategySnapshot], path: &std::path::Path) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(snapshots)?;
    std::fs::write(path, json)?;
    Ok(())
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
    let mid = symbol.len() / 2;
    (symbol[..mid].to_string(), symbol[mid..].to_string())
}
