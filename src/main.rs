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
use app::{AlertLevel, AppCommand, AppState, DEFAULT_SYMBOLS, SaleResult, StrategySlot, UiMode, MAX_SLOTS};
use config::{AlertsConfig, Config, Direction, DcaConfig};
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

    tracing::info!("Starting Trading View...");

    // Cargar configuración
    let (config, config_path) = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\n❌ Configuration error:\n   {}\n", e);
            eprintln!("📝 Edit config.toml with your Binance API keys");
            std::process::exit(1);
        }
    };

    // Ruta del archivo de estado persistente
    let state_path = config::exe_dir().join("strategy_state.json");

    // Crear cliente REST de Binance
    let client = Arc::new(BinanceClient::new(config.binance.clone())?);

    // Test de conectividad
    client.ping().await.map_err(|e| {
        anyhow::anyhow!("Could not connect to Binance: {}", e)
    })?;
    tracing::info!("Connectivity OK");

    // Sincronizar reloj con Binance para evitar error -1021
    client.sync_time().await.map_err(|e| {
        anyhow::anyhow!("Could not synchronize time with Binance: {}", e)
    })?;

    // Obtener lista de pares USDT disponibles en Binance (mainnet o testnet)
    let available_symbols: Vec<String> = match client.get_usdt_symbols().await {
        Ok(syms) if !syms.is_empty() => {
            tracing::info!("{} USDT pairs obtained from Binance", syms.len());
            syms
        }
        Ok(_) | Err(_) => {
            tracing::warn!("Could not obtain pairs from Binance, using default list");
            DEFAULT_SYMBOLS.iter().map(|s| s.to_string()).collect()
        }
    };

    // Cargar snapshots anteriores
    let snapshots = load_snapshots(&state_path);

    // Crear los slots iniciales
    let mut slots: Vec<StrategySlot> = Vec::new();
    let mut next_id = 0usize;
    let mut restore_info: Vec<(String, Direction, usize, bool)> = Vec::new();

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

            restore_info.push((snap.symbol.clone(), snap.direction.clone(), trade_count, strat.state.is_active()));

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

    let ui_mode = if restore_info.iter().any(|(_, _, c, active)| *c > 0 || *active) {
        UiMode::RestoreSession(restore_info)
    } else {
        UiMode::Normal
    };

    let state = Arc::new(Mutex::new(AppState {
        slots,
        selected_slot: 0,
        prices: HashMap::new(),
        alert_levels: HashMap::new(),
        symbols: available_symbols,
        log: std::collections::VecDeque::new(),
        should_quit: false,
        ui_mode,
        new_strat_symbol_idx: 0,
        new_strat_direction: Direction::Long,
        new_strat_auto_restart: config.dca.auto_restart,
        new_strat_auto_flip: config.dca.auto_flip,
        new_strat_has_bnb: config.dca.has_bnb_balance,
        cfg_amount_buf: String::new(),
        cfg_has_bnb: config.dca.has_bnb_balance,
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
    // Tarea 2: Motor de alertas S/R (rolling window, cada 5 min)
    // ----------------------------------------------------------------
    {
        let state_ref = Arc::clone(&state);
        let client_ref = Arc::clone(&client);
        let alerts_config = config.alerts.clone();
        tokio::spawn(run_alert_engine(state_ref, client_ref, alerts_config));
    }

    // ----------------------------------------------------------------
    // Tarea 3: Motor de estrategia multi-slot
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

    tracing::info!("Bot stopped.");
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

        AppCommand::RestoreSessionContinue => {
            let mut s = state.lock().await;
            s.log("Previous sessions restored. Active strategies have been RESUMED.");
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
                s.log("Previous session discarded. Starting from scratch.");
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

        AppCommand::ToggleStartStopSelected => {
            let mut s = state.lock().await;
            let mut log_msg = None;
            if let Some(slot) = s.selected_mut() {
                if slot.strategy.state.is_active() {
                    slot.strategy.stop();
                    log_msg = Some(format!("Strategy for {} STOPPED.", slot.symbol));
                } else {
                    slot.strategy.start();
                    log_msg = Some(format!("Strategy for {} STARTED.", slot.symbol));
                }
            }
            if let Some(msg) = log_msg {
                s.log(&msg);
                drop(s);
                save_all_snapshots(state, state_path).await;
            }
        }

        AppCommand::ToggleAutoFlip => {
            let mut s = state.lock().await;
            let mut log_msg = None;
            if let Some(slot) = s.selected_mut() {
                slot.strategy.config.auto_flip = !slot.strategy.config.auto_flip;
                let status = if slot.strategy.config.auto_flip { "ENABLED" } else { "DISABLED" };
                log_msg = Some(format!("Auto-Flip {} for {}", status, slot.symbol));
            }
            if let Some(msg) = log_msg {
                s.log(&msg);
                drop(s);
                save_all_snapshots(state, state_path).await;
            }
        }

        // --- Borrado de slot (D) ---
        AppCommand::OpenConfirmDelete => {
            let mut s = state.lock().await;
            if s.slots.len() <= 1 {
                s.log_error("Cannot delete the last slot.");
                return;
            }

            s.ui_mode = UiMode::ConfirmDelete;
        }
        AppCommand::ConfirmDeleteNow => {
            let id = {
                let mut s = state.lock().await;
                s.ui_mode = UiMode::Normal;
                s.selected().map(|sl| sl.id)
            };

            if let Some(id) = id {
                let mut s = state.lock().await;
                s.remove_slot(id);
                s.log("Slot removed.");
                drop(s);
                
                update_symbol_watch(state, symbol_tx).await;
                save_all_snapshots(state, state_path).await;
                refresh_balance(state, client).await;
            }
        }

        // --- Modal nueva estrategia (S) ---
        AppCommand::OpenNewStrategy => {
            let mut s = state.lock().await;
            // Pre-seleccionar el primer símbolo no usado
            let used: Vec<String> = s.slots.iter().map(|sl| sl.symbol.clone()).collect();
            let idx = s.symbols
                .iter()
                .position(|sym| !used.contains(sym))
                .unwrap_or(0);
            s.new_strat_symbol_idx = idx;
            s.new_strat_direction = Direction::Long;
            s.new_strat_auto_restart = base_config.auto_restart;
            s.new_strat_auto_flip = base_config.auto_flip;
            s.ui_mode = UiMode::NewStrategy;
        }
        AppCommand::NewStratSymbolUp => {
            let mut s = state.lock().await;
            let len = s.symbols.len();
            if len > 0 {
                s.new_strat_symbol_idx =
                    if s.new_strat_symbol_idx == 0 { len - 1 } else { s.new_strat_symbol_idx - 1 };
            }
        }
        AppCommand::NewStratSymbolDown => {
            let mut s = state.lock().await;
            let len = s.symbols.len();
            if len > 0 {
                s.new_strat_symbol_idx = (s.new_strat_symbol_idx + 1) % len;
            }
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
        AppCommand::NewStratToggleAutoFlip => {
            let mut s = state.lock().await;
            s.new_strat_auto_flip = !s.new_strat_auto_flip;
        }
        AppCommand::NewStratToggleBnb => {
            let mut s = state.lock().await;
            s.new_strat_has_bnb = !s.new_strat_has_bnb;
        }
        AppCommand::NewStratCancel => {
            state.lock().await.ui_mode = UiMode::Normal;
        }
        AppCommand::NewStratConfirm => {
            let (symbol, direction, auto_restart, auto_flip, has_bnb, can_add) = {
                let s = state.lock().await;
                let idx = s.new_strat_symbol_idx.min(s.symbols.len().saturating_sub(1));
                let sym = s.symbols.get(idx).cloned().unwrap_or_else(|| "BTCUSDT".to_string());
                let dir = s.new_strat_direction.clone();
                let ar = s.new_strat_auto_restart;
                let af = s.new_strat_auto_flip;
                let bnb = s.new_strat_has_bnb;
                let can = s.slots.len() < MAX_SLOTS;
                (sym, dir, ar, af, bnb, can)
            };

            if !can_add {
                state.lock().await.log_error("Maximum strategies reached (4).");
                return;
            }

            let (base, quote) = parse_symbol(&symbol);
            let mut cfg = base_config.clone();
            cfg.symbol = symbol.clone();
            cfg.direction = direction.clone();
            cfg.auto_restart = auto_restart;
            cfg.auto_flip = auto_flip;
            cfg.has_bnb_balance = has_bnb;
            let mut strat = DcaStrategy::new(cfg);
            strat.start();

            {
                let mut s = state.lock().await;
                let id = s.alloc_slot_id();
                let dir_label = match direction {
                    Direction::Long  => "LONG",
                    Direction::Short => "SHORT",
                };
                s.log(&format!("New strategy: {} {} started", symbol, dir_label));
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
            s.log("DCA cycle restarted.");
            drop(s);
            save_all_snapshots(state, state_path).await;
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
            let (amt, bnb) = s
                .selected()
                .map(|sl| (sl.strategy.config.quote_amount, sl.strategy.config.has_bnb_balance))
                .unwrap_or((base_config.quote_amount, base_config.has_bnb_balance));
            s.cfg_amount_buf = format!("{}", amt);
            s.cfg_has_bnb = bnb;
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
                s.log("No open position to close.");
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
                state.lock().await.log("No open position to close.");
                return;
            }

            let log_msg = match direction {
                Direction::Long  => format!("⚠ MANUAL CLOSE [{}]: Selling {:.6} @ ${:.2}", symbol, qty, price),
                Direction::Short => format!("⚠ MANUAL CLOSE [{}]: Rebuying {:.6} @ ${:.2}", symbol, qty, price),
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
                            "✓ MANUAL CLOSE [{}] executed. Received: ${:.2}",
                            symbol, received
                        ));
                        s.ui_mode = UiMode::PostSale(
                            slot_id,
                            SaleResult {
                                kind: "MANUAL CLOSE".to_string(),
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
                        .log_error(&format!("Manual close [{}] failed: {}", symbol, e));
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
                        let bnb = s.cfg_has_bnb;
                        // Aplicar a todos los slots
                        for slot in s.slots.iter_mut() {
                            slot.strategy.config.quote_amount = v;
                            slot.strategy.config.has_bnb_balance = bnb;
                        }
                        s.ui_mode = UiMode::Normal;
                        s.log(&format!("Config updated: ${:.2} USDT, BNB Fees: {} (all slots)", v, if bnb { "YES" } else { "NO" }));
                    }
                    if let Err(e) = Config::save_dca(config_path, &base_config.symbol, v) {
                        state.lock().await.log_error(&format!(
                            "Could not save config: {}",
                            e
                        ));
                    }
                }
                _ => {
                    state.lock().await.log_error(&format!(
                        "Invalid amount: '{}' (minimum $1)",
                        buf
                    ));
                }
            }
        }

        AppCommand::CfgToggleBnb => {
            let mut s = state.lock().await;
            s.cfg_has_bnb = !s.cfg_has_bnb;
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
         qty, amount, pnl, pnl_pct, auto_restart, auto_flip, cooldown_minutes, symbol, price_peak, price_trough) =
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
        let auto_restart        = slot.strategy.config.auto_restart;
        let auto_flip           = slot.strategy.config.auto_flip;
        let cooldown_minutes    = slot.strategy.config.restart_cooldown_minutes;
        let symbol         = slot.symbol.clone();
        let price_peak     = slot.strategy.price_peak;
        let price_trough   = slot.strategy.price_trough;

        (price, direction, should_entry, should_tp, should_sl, should_trailing_tp,
         qty, amount, pnl, pnl_pct, auto_restart, auto_flip, cooldown_minutes, symbol, price_peak, price_trough)
    };

    // =====================================================================
    // Stop Loss (prioridad máxima)
    // =====================================================================
    if should_sl && qty > 0.0 {
        let log_msg = match direction {
            Direction::Long  => format!("⚠ STOP LOSS [{}]! Selling {:.6} @ ${:.2}", symbol, qty, price),
            Direction::Short => format!("⚠ STOP LOSS [{}]! Re-buying {:.6} @ ${:.2}", symbol, qty, price),
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
                    s.log(&format!("✓ STOP LOSS [{}] executed. Received: ${:.2}", symbol, received));
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
                state.lock().await.log_error(&format!("Stop loss [{}] failed: {}", symbol, e));
            }
        }
        return;
    }

    // =====================================================================
    // Take Profit
    // =====================================================================
    if should_tp && qty > 0.0 {
        let log_msg = match direction {
            Direction::Long  => format!("✓ TAKE PROFIT [{}]! P&L: +${:.2}  Selling {:.6} @ ${:.2}", symbol, pnl, qty, price),
            Direction::Short => format!("✓ TAKE PROFIT [{}]! P&L: +${:.2}  Re-buying {:.6} @ ${:.2}", symbol, pnl, qty, price),
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
                    let mut flipped_to = None;
                    if let Some(slot) = s.slot_by_id_mut(slot_id) {
                        slot.strategy.state = DcaState::TakeProfitReached;
                        slot.strategy.clear_trades();
                        if auto_restart {
                            if auto_flip {
                                slot.strategy.config.direction = slot.strategy.config.direction.flip();
                                flipped_to = Some(slot.strategy.config.direction.clone());
                            }
                            slot.strategy.start_after_tp(cooldown_minutes);
                        } else {
                            slot.strategy.stop();
                        }
                    }

                    if let Some(dir) = flipped_to {
                        let dir_label = match dir {
                            Direction::Long => "LONG",
                            Direction::Short => "SHORT",
                        };
                        s.log(&format!("Auto-flip enabled. Switched to {} mode.", dir_label));
                    }
                    s.log(&format!("✓ TAKE PROFIT [{}] executed. Received: ${:.2}", symbol, received));
                    if auto_restart {
                        s.log("Auto-restart enabled. DCA cycle restarted.");
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
                state.lock().await.log_error(&format!("Take profit [{}] failed: {}", symbol, e));
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
                    "↓ TRAILING TP [{}]! Max: ${:.4}  Drop: {:.2}%  P&L: +${:.2}",
                    symbol, price_peak, drop, pnl
                )
            }
            Direction::Short => {
                let rise = ((price - price_trough) / price_trough) * 100.0;
                format!(
                    "↑ TRAILING TP [{}]! Min: ${:.4}  Rise: {:.2}%  P&L: +${:.2}",
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
                    let mut flipped_to = None;
                    if let Some(slot) = s.slot_by_id_mut(slot_id) {
                        slot.strategy.state = DcaState::TakeProfitReached;
                        slot.strategy.clear_trades();
                        if auto_restart {
                            if auto_flip {
                                slot.strategy.config.direction = slot.strategy.config.direction.flip();
                                flipped_to = Some(slot.strategy.config.direction.clone());
                            }
                            slot.strategy.start_after_tp(cooldown_minutes);
                        } else {
                            slot.strategy.stop();
                        }
                    }

                    if let Some(dir) = flipped_to {
                        let dir_label = match dir {
                            Direction::Long => "LONG",
                            Direction::Short => "SHORT",
                        };
                        s.log(&format!("Auto-flip enabled. Switched to {} mode.", dir_label));
                    }
                    s.log(&format!("✓ TRAILING TP [{}] executed. Received: ${:.2}", symbol, received));
                    if auto_restart {
                        s.log("Auto-restart enabled. DCA cycle restarted.");
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
                state.lock().await.log_error(&format!("Trailing TP [{}] failed: {}", symbol, e));
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
                    "Executing DCA LONG buy [{}] #{} of ${:.2}",
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
                        let mut s = state.lock().await;
                        let mut err_msg = format!("Buy [{}] failed: {}", symbol, e);
                        
                        if err_msg.contains("-2010") {
                            if let Some(slot) = s.slot_by_id(slot_id) {
                                let needed = amount - slot.quote_balance;
                                if needed > 0.0 {
                                    err_msg = format!("Buy [{}] failed: Insufficient balance. You need ${:.2} more {}.", symbol, needed, slot.quote_asset);
                                }
                            }
                        }
                        
                        s.log_error(&err_msg);
                        if let Some(slot) = s.slot_by_id_mut(slot_id) {
                            slot.strategy.stop();
                            slot.strategy.state = DcaState::Idle;
                        }
                        s.log(&format!("Strategy for {} STOPPED due to error.", symbol));
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
                    "Executing DCA SHORT sell [{}] #{}: {:.6}",
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
                                    "SHORT #{} [{}]: sold {:.6} {} @ ${:.4} (${:.2})",
                                    num, symbol, exec_qty, base, actual_price, received
                                ));
                            }
                        }
                        save_all_snapshots(state, state_path).await;
                    }
                    Err(e) => {
                        let mut s = state.lock().await;
                        let mut err_msg = format!("Short entry [{}] failed: {}", symbol, e);
                        
                        if err_msg.contains("-2010") {
                            if let Some(slot) = s.slot_by_id(slot_id) {
                                let needed = qty_to_sell - slot.base_balance;
                                if needed > 0.0 {
                                    err_msg = format!("Short entry [{}] failed: Insufficient balance. You need {:.6} more {}.", symbol, needed, slot.base_asset);
                                }
                            }
                        }
                        
                        s.log_error(&err_msg);
                        if let Some(slot) = s.slot_by_id_mut(slot_id) {
                            slot.strategy.stop();
                            slot.strategy.state = DcaState::Idle;
                        }
                        s.log(&format!("Strategy for {} STOPPED due to error.", symbol));
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
        tracing::warn!("Could not save state: {}", e);
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
            tracing::debug!("Balances updated for {} slot(s)", s.slots.len());
        }
        Err(e) => {
            tracing::warn!("Could not update balance: {}", e);
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

/// Beep del sistema para alertas de soporte/resistencia
fn play_alert_sound() {
    // BEL character: la mayoría de terminales/consolas emiten un beep
    eprint!("\x07");
}

/// Motor de alertas S/R: cada 5 minutos descarga klines, calcula soporte/resistencia
/// con rolling window y dispara alertas cuando el precio cruza un nivel.
async fn run_alert_engine(
    state: Arc<Mutex<AppState>>,
    client: Arc<BinanceClient>,
    cfg: AlertsConfig,
) {
    // Primera ejecución después de 30s (dar tiempo al WebSocket para recibir precios)
    tokio::time::sleep(Duration::from_secs(30)).await;

    let mut tick = tokio::time::interval(Duration::from_secs(300)); // cada 5 minutos
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let limit = (cfg.rolling_window + 1) as u32; // +1 para excluir la vela actual (incompleta)
    let cooldown = Duration::from_secs(cfg.cooldown_minutes * 60);

    loop {
        tick.tick().await;

        // Obtener todos los símbolos activos
        let symbols: Vec<String> = state.lock().await.slots.iter()
            .map(|s| s.symbol.clone())
            .collect();

        for symbol in symbols {
            // Descargar velas (endpoint público, sin firma)
            let klines = match client.get_klines(&symbol, &cfg.candle_interval, limit).await {
                Ok(k) if k.len() > 1 => k,
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("get_klines({}) error: {}", symbol, e);
                    continue;
                }
            };

            // Usar solo velas cerradas (excluir la última, que puede estar incompleta)
            let completed = &klines[..klines.len() - 1];
            let resistance = completed.iter().map(|k| k.high).fold(f64::NEG_INFINITY, f64::max);
            let support    = completed.iter().map(|k| k.low ).fold(f64::INFINITY,     f64::min);

            // Precio actual del símbolo
            let current_price = {
                let s = state.lock().await;
                s.prices.get(&symbol).map(|m| m.price).unwrap_or(0.0)
            };
            if current_price == 0.0 { continue; }

            let now = std::time::Instant::now();

            // Leer precio previo y últimas alertas
            let (prev_price, last_sup, last_res) = {
                let s = state.lock().await;
                let l = s.alert_levels.get(&symbol);
                (
                    l.map(|x| x.prev_price).unwrap_or(current_price),
                    l.and_then(|x| x.last_support_alert),
                    l.and_then(|x| x.last_resistance_alert),
                )
            };

            // Detección de cruce de nivel
            let support_broken    = current_price < support    && prev_price >= support;
            let resistance_broken = current_price > resistance && prev_price <= resistance;

            let sup_ok = last_sup.map_or(true, |t| now.duration_since(t) >= cooldown);
            let res_ok = last_res.map_or(true, |t| now.duration_since(t) >= cooldown);

            if support_broken && sup_ok {
                let msg = format!(
                    "[{}] Support broken! ${:.2} < Support ${:.2}",
                    symbol, current_price, support
                );
                {
                    let mut s = state.lock().await;
                    s.log_alert(&msg);
                    let level = s.alert_levels.entry(symbol.clone()).or_insert(AlertLevel {
                        resistance,
                        support,
                        prev_price: current_price,
                        last_support_alert: None,
                        last_resistance_alert: None,
                    });
                    level.last_support_alert = Some(now);
                }
                play_alert_sound();
            }

            if resistance_broken && res_ok {
                let msg = format!(
                    "[{}] Resistance broken! ${:.2} > Resistance ${:.2}",
                    symbol, current_price, resistance
                );
                {
                    let mut s = state.lock().await;
                    s.log_alert(&msg);
                    let level = s.alert_levels.entry(symbol.clone()).or_insert(AlertLevel {
                        resistance,
                        support,
                        prev_price: current_price,
                        last_support_alert: None,
                        last_resistance_alert: None,
                    });
                    level.last_resistance_alert = Some(now);
                }
                play_alert_sound();
            }

            // Actualizar niveles y precio previo para la próxima iteración
            {
                let mut s = state.lock().await;
                let level = s.alert_levels.entry(symbol.clone()).or_insert(AlertLevel {
                    resistance,
                    support,
                    prev_price: current_price,
                    last_support_alert: None,
                    last_resistance_alert: None,
                });
                level.resistance = resistance;
                level.support    = support;
                level.prev_price = current_price;
            }
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
    let mid = symbol.len() / 2;
    (symbol[..mid].to_string(), symbol[mid..].to_string())
}
