use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
    Frame, Terminal,
};
use tokio::sync::{mpsc, Mutex};

use crate::app::{AppCommand, AppState, SaleResult, UiMode, MAX_SLOTS};
use crate::config::Direction as TradeDirection;
use crate::strategy::dca::DcaState;

const TICK_MS: u64 = 150; // ~6 FPS refresh rate

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    state: Arc<Mutex<AppState>>,
    cmd_tx: mpsc::Sender<AppCommand>,
}

impl Tui {
    pub fn new(
        state: Arc<Mutex<AppState>>,
        cmd_tx: mpsc::Sender<AppCommand>,
    ) -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(Self { terminal, state, cmd_tx })
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut event_stream = EventStream::new();
        let tick = Duration::from_millis(TICK_MS);

        loop {
            {
                let state = self.state.lock().await;
                self.terminal.draw(|f| Self::render(f, &state))?;
            }

            tokio::select! {
                _ = tokio::time::sleep(tick) => {}
                maybe_event = event_stream.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            if self.handle_key(key.code, key.modifiers).await? {
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            tracing::error!("Event error: {}", e);
                        }
                        _ => {}
                    }
                }
            }

            if self.state.lock().await.should_quit {
                break;
            }
        }

        self.cleanup()?;
        Ok(())
    }

    async fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
        let ui_mode = self.state.lock().await.ui_mode.clone();

        match ui_mode {
            // ----------------------------------------------------------------
            UiMode::RestoreSession(_) => match code {
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Enter => {
                    let _ = self.cmd_tx.send(AppCommand::RestoreSessionContinue).await;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    let _ = self.cmd_tx.send(AppCommand::RestoreSessionDiscard).await;
                }
                _ => {}
            },

            // ----------------------------------------------------------------
            UiMode::PostSale(slot_id, _) => match code {
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    let _ = self.cmd_tx.send(AppCommand::PostSaleRestart(slot_id)).await;
                }
                _ => {
                    let _ = self.cmd_tx.send(AppCommand::PostSaleDismiss(slot_id)).await;
                }
            },

            // ----------------------------------------------------------------
            UiMode::NewStrategy => match code {
                KeyCode::Enter => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratConfirm).await;
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratCancel).await;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratSymbolUp).await;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratSymbolDown).await;
                }
                KeyCode::Tab => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratToggleDirection).await;
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratToggleAutoRestart).await;
                }
                KeyCode::Char('f') | KeyCode::Char('F') => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratToggleAutoFlip).await;
                }
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    let _ = self.cmd_tx.send(AppCommand::NewStratToggleBnb).await;
                }
                _ => {}
            },

            // ----------------------------------------------------------------
            UiMode::Config => match code {
                KeyCode::Esc => {
                    let _ = self.cmd_tx.send(AppCommand::CloseConfig).await;
                }
                KeyCode::Enter => {
                    let _ = self.cmd_tx.send(AppCommand::CfgConfirm).await;
                }
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    let _ = self.cmd_tx.send(AppCommand::CfgToggleBnb).await;
                }
                KeyCode::Char(c) => {
                    let _ = self.cmd_tx.send(AppCommand::CfgInputChar(c)).await;
                }
                KeyCode::Backspace => {
                    let _ = self.cmd_tx.send(AppCommand::CfgBackspace).await;
                }
                _ => {}
            },

            // ----------------------------------------------------------------
            UiMode::ConfirmClose => match code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let _ = self.cmd_tx.send(AppCommand::ConfirmCloseNow).await;
                }
                _ => {
                    let _ = self.cmd_tx.send(AppCommand::CloseConfig).await;
                }
            },

            // ----------------------------------------------------------------
            UiMode::ConfirmDelete => match code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let _ = self.cmd_tx.send(AppCommand::ConfirmDeleteNow).await;
                }
                _ => {
                    let _ = self.cmd_tx.send(AppCommand::CloseConfig).await;
                }
            },

            // ----------------------------------------------------------------
            UiMode::Normal => match code {
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                    let _ = self.cmd_tx.send(AppCommand::Quit).await;
                    return Ok(true);
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = self.cmd_tx.send(AppCommand::Quit).await;
                    return Ok(true);
                }
                // Nueva estrategia
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    let slots_len = self.state.lock().await.slots.len();
                    if slots_len < MAX_SLOTS {
                        let _ = self.cmd_tx.send(AppCommand::OpenNewStrategy).await;
                    }
                }
                // Iniciar/Detener slot seleccionado (X)
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    let _ = self.cmd_tx.send(AppCommand::ToggleStartStopSelected).await;
                }
                // Cerrar posición a mercado ahora (pide confirmación)
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    let _ = self.cmd_tx.send(AppCommand::OpenConfirmClose).await;
                }
                // Borrar slot seleccionado (Delete o D)
                KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Delete => {
                    let _ = self.cmd_tx.send(AppCommand::OpenConfirmDelete).await;
                }
                // Alternar Auto-Flip
                KeyCode::Char('f') | KeyCode::Char('F') => {
                    let _ = self.cmd_tx.send(AppCommand::ToggleAutoFlip).await;
                }
                // Configuración (monto)
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    let _ = self.cmd_tx.send(AppCommand::OpenConfig).await;
                }
                // Navegar slots
                KeyCode::Up | KeyCode::Char('k') => {
                    let _ = self.cmd_tx.send(AppCommand::SlotSelectUp).await;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let _ = self.cmd_tx.send(AppCommand::SlotSelectDown).await;
                }
                _ => {}
            },
        }
        Ok(false)
    }

    fn cleanup(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    // -----------------------------------------------------------
    // Rendering principal
    // -----------------------------------------------------------

    fn render(f: &mut Frame, state: &AppState) {
        let size = f.area();

        // Layout vertical principal
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),  // header
                Constraint::Min(10),    // body (split horizontal)
                Constraint::Length(7),  // log
                Constraint::Length(3),  // footer
            ])
            .split(size);

        // Body: split horizontal → slot list | contenido del slot
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(16), // lista de slots
                Constraint::Min(0),     // contenido principal
            ])
            .split(main_chunks[1]);

        // Contenido principal: stats + trades
        let content_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(10), // precio + DCA stats (10 = 8 contenido + 2 bordes + 1 S/R)
                Constraint::Min(6),    // historial de operaciones
            ])
            .split(body_chunks[1]);

        Self::render_header(f, state, main_chunks[0]);
        Self::render_slot_list(f, state, body_chunks[0]);
        Self::render_stats(f, state, content_chunks[0]);
        Self::render_trades(f, state, content_chunks[1]);
        Self::render_log(f, state, main_chunks[2]);
        Self::render_footer(f, state, main_chunks[3]);

        // Overlays (encima de todo)
        match &state.ui_mode {
            UiMode::RestoreSession(slots_info) => {
                Self::render_restore_session_panel(f, slots_info);
            }
            UiMode::NewStrategy => {
                Self::render_new_strategy_panel(f, state);
            }
            UiMode::Config => {
                Self::render_config_panel(f, state);
            }
            UiMode::PostSale(_, result) => {
                let quote_asset = state
                    .selected()
                    .map(|s| s.quote_asset.as_str())
                    .unwrap_or("USDT");
                Self::render_post_sale_panel(f, result, quote_asset);
            }
            UiMode::ConfirmClose => {
                Self::render_confirm_close_panel(f, state);
            }
            UiMode::ConfirmDelete => {
                Self::render_confirm_delete_panel(f, state);
            }
            UiMode::Normal => {}
        }
    }

    // -----------------------------------------------------------
    // Header
    // -----------------------------------------------------------

    fn render_header(f: &mut Frame, state: &AppState, area: Rect) {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");

        let title_spans = if let Some(slot) = state.selected() {
            let symbol = format!("{} / {}", slot.base_asset, slot.quote_asset);
            let (status_color, status_label) = match &slot.strategy.state {
                DcaState::Running           => (Color::Green, "● ACTIVE"),
                DcaState::TakeProfitReached => (Color::Cyan, "✓ TAKE PROFIT"),
                DcaState::StopLossReached   => (Color::Red, "✗ STOP LOSS"),
                DcaState::MaxOrdersReached  => (Color::Yellow, "■ MAX ORDERS"),
                DcaState::Error(_)          => (Color::Red, "✗ ERROR"),
                DcaState::Idle              => (Color::DarkGray, "○ STOPPED"),
            };
            let (dir_label, dir_color) = match slot.strategy.config.direction {
                TradeDirection::Long  => ("▲ LONG",  Color::Green),
                TradeDirection::Short => ("▼ SHORT", Color::Red),
            };
            vec![
                Span::styled(
                    " Trading View ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw("│ "),
                Span::styled(
                    symbol,
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    dir_label,
                    Style::default().fg(dir_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" │ "),
                Span::styled(
                    status_label,
                    Style::default().fg(status_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" │ "),
                Span::styled(now.to_string(), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
            ]
        } else {
            vec![
                Span::styled(
                    " Trading View ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw("│ "),
                Span::styled(
                    "No active strategies — Press [S] to start",
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" │ "),
                Span::styled(now.to_string(), Style::default().fg(Color::DarkGray)),
            ]
        };

        let paragraph = Paragraph::new(Line::from(title_spans))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .alignment(Alignment::Left);

        f.render_widget(paragraph, area);
    }

    // -----------------------------------------------------------
    // Panel izquierdo: lista de slots
    // -----------------------------------------------------------

    fn render_slot_list(f: &mut Frame, state: &AppState, area: Rect) {
        let mut lines: Vec<Line> = state
            .slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let is_selected = i == state.selected_slot;
                let prefix = if is_selected { "►" } else { " " };
                let base = &slot.base_asset[..slot.base_asset.len().min(5)];
                let dir_arrow = match slot.strategy.config.direction {
                    TradeDirection::Long  => "▲",
                    TradeDirection::Short => "▼",
                };
                let (status_dot, status_color) = match &slot.strategy.state {
                    DcaState::Running           => ("●", Color::Green),
                    DcaState::TakeProfitReached => ("●", Color::Cyan),
                    DcaState::StopLossReached   => ("●", Color::Magenta),
                    DcaState::MaxOrdersReached  => ("●", Color::Yellow),
                    DcaState::Error(_)          => ("●", Color::LightRed),
                    DcaState::Idle              => ("●", Color::Red),
                };
                let dir_color = match slot.strategy.config.direction {
                    TradeDirection::Long  => Color::Green,
                    TradeDirection::Short => Color::Red,
                };
                let sel_style = if is_selected {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };

                let flip_icon = if slot.strategy.config.auto_flip { "↺" } else { " " };

                Line::from(vec![
                    Span::styled(format!("{} ", prefix), sel_style),
                    Span::styled(base.to_string(), sel_style),
                    Span::raw(" "),
                    Span::styled(dir_arrow.to_string(), Style::default().fg(dir_color)),
                    Span::styled(flip_icon.to_string(), Style::default().fg(Color::Magenta)),
                    Span::raw(" "),
                    Span::styled(status_dot.to_string(), Style::default().fg(status_color)),
                ])
            })
            .collect();

        // Pista para agregar nueva estrategia
        if state.slots.len() < MAX_SLOTS {
            lines.push(Line::from(Span::styled(
                "  [S] New",
                Style::default().fg(Color::DarkGray),
            )));
        }

        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .title(" Slots ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            area,
        );
    }

    // -----------------------------------------------------------
    // Panel de estadísticas (precio + DCA stats)
    // -----------------------------------------------------------

    fn render_stats(f: &mut Frame, state: &AppState, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(area);

        let (base, quote, base_bal, quote_bal) = state
            .selected()
            .map(|s| {
                (
                    s.base_asset.clone(),
                    s.quote_asset.clone(),
                    s.base_balance,
                    s.quote_balance,
                )
            })
            .unwrap_or_default();

        // Panel izquierdo: Precio y Balances
        {
            let market = state.selected_market();

            let change_color = if market.change_24h_pct >= 0.0 { Color::Green } else { Color::Red };
            let change_sign  = if market.change_24h_pct >= 0.0 { "+" } else { "" };

            let mut price_text = vec![
                Line::from(vec![
                    Span::styled("── MARKETS ──────────────────", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!(" ${:.2}", market.price),
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!("{}{:.2}% 24h", change_sign, market.change_24h_pct),
                        Style::default().fg(change_color),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" H: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("${:.2}", market.high_24h), Style::default().fg(Color::Green)),
                    Span::styled("  L: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("${:.2}", market.low_24h), Style::default().fg(Color::Red)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("── BALANCE ──────────────────", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled(format!(" {}: ", base), Style::default().fg(Color::Yellow)),
                    Span::styled(format!("{:.6}", base_bal), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(format!(" {}: ", quote), Style::default().fg(Color::Yellow)),
                    Span::styled(format!("{:.2}", quote_bal), Style::default().fg(Color::White)),
                ]),
            ];

            // Niveles de Soporte/Resistencia
            if let Some(sym) = state.selected().map(|s| s.symbol.clone()) {
                if let Some(level) = state.alert_levels.get(&sym) {
                    if level.resistance > 0.0 {
                        price_text.push(Line::from(""));
                        price_text.push(Line::from(vec![
                            Span::styled("── TECH LEVELS ──────────────", Style::default().fg(Color::DarkGray)),
                        ]));
                        price_text.push(Line::from(vec![
                            Span::styled(" Support:    ", Style::default().fg(Color::DarkGray)),
                            Span::styled(format!("${:.2}", level.support), Style::default().fg(Color::Green)),
                        ]));
                        price_text.push(Line::from(vec![
                            Span::styled(" Resistance: ", Style::default().fg(Color::DarkGray)),
                            Span::styled(format!("${:.2}", level.resistance), Style::default().fg(Color::Red)),
                        ]));
                    }
                }
            }

            f.render_widget(
                Paragraph::new(price_text).block(
                    Block::default()
                        .title(" Price ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
                cols[0],
            );
        }

        // Panel derecho: DCA Stats
        {
            let slot = match state.selected() {
                Some(s) => s,
                None => {
                    f.render_widget(
                        Block::default()
                            .title(" DCA Strategy ")
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Magenta)),
                        cols[1],
                    );
                    return;
                }
            };

            let price       = state.selected_price();
            let avg         = slot.strategy.average_cost();
            let invested    = slot.strategy.total_invested();
            let qty         = slot.strategy.total_quantity();
            let pnl         = slot.strategy.pnl(price);
            let pnl_pct     = slot.strategy.pnl_pct(price);
            let orders_count = slot.strategy.trades.len();
            let max_orders  = slot.strategy.config.max_orders;
            let countdown   = slot.strategy.next_buy_countdown();
            let daily_spent = slot.strategy.daily_spent;
            let quote_amount = slot.strategy.config.quote_amount;
            let trailing_trigger = slot.strategy.trailing_tp_trigger_price();
            let trailing_configured = slot.strategy.config.trailing_tp_pct > 0.0;
            let direction   = &slot.strategy.config.direction;
            let quote_asset = &slot.quote_asset;
            let base_asset  = &slot.base_asset;

            let (pnl_color, pnl_sign) = if pnl >= 0.0 { (Color::Green, "+") } else { (Color::Red, "") };

            // Línea de trailing TP (dirección-aware)
            let trailing_line = match direction {
                TradeDirection::Long => {
                    let price_peak = slot.strategy.price_peak;
                    if trailing_configured && price_peak > 0.0 {
                        let drop_so_far = ((price_peak - price) / price_peak) * 100.0;
                        let trigger_color = if price <= trailing_trigger {
                            Color::Yellow
                        } else {
                            Color::Cyan
                        };
                        Line::from(vec![
                            Span::styled(" Trail TP:   ", Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                format!(
                                    "peak ${:.4}  closes <${:.4} ({:.2}%↓)",
                                    price_peak, trailing_trigger, drop_so_far
                                ),
                                Style::default().fg(trigger_color),
                            ),
                        ])
                    } else {
                        Line::from(vec![
                            Span::styled(" Next buy:    ", Style::default().fg(Color::DarkGray)),
                            Span::styled(countdown, Style::default().fg(Color::Cyan)),
                        ])
                    }
                }
                TradeDirection::Short => {
                    let price_trough = slot.strategy.price_trough;
                    let valid = price_trough < f64::MAX && price_trough > 0.0;
                    if trailing_configured && valid {
                        let rise_so_far = ((price - price_trough) / price_trough) * 100.0;
                        let trigger_color = if price >= trailing_trigger {
                            Color::Yellow
                        } else {
                            Color::Cyan
                        };
                        Line::from(vec![
                            Span::styled(" Trail TP:   ", Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                format!(
                                    "trough ${:.4}  closes >${:.4} ({:.2}%↑)",
                                    price_trough, trailing_trigger, rise_so_far
                                ),
                                Style::default().fg(trigger_color),
                            ),
                        ])
                    } else {
                        Line::from(vec![
                            Span::styled(" Next sell:   ", Style::default().fg(Color::DarkGray)),
                            Span::styled(countdown, Style::default().fg(Color::Cyan)),
                        ])
                    }
                }
            };

            let (avg_label, invested_label, qty_label, entry_label) = match direction {
                TradeDirection::Long  => (" Avg cost:    ", " Invested:   ", " Quantity:   ", " Buy amount:  "),
                TradeDirection::Short => (" Sell price:  ", " Received:   ", " Sold:       ", " Sell amount: "),
            };

            let dca_text = vec![
                Line::from(vec![
                    Span::styled("── STATE ───────────────────", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled(" Mode:       ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if slot.strategy.config.auto_restart { "Auto-Restart ✓ " } else { "Manual " },
                        Style::default().fg(if slot.strategy.config.auto_restart { Color::Cyan } else { Color::DarkGray })
                    ),
                    Span::styled(
                        if slot.strategy.config.auto_flip { "↺ L↔S" } else { "" },
                        Style::default().fg(Color::Magenta)
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Status:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled("● ", Style::default().fg(match &slot.strategy.state {
                        DcaState::Running => Color::Green,
                        DcaState::Idle => Color::Red,
                        DcaState::TakeProfitReached => Color::Cyan,
                        DcaState::StopLossReached => Color::Magenta,
                        DcaState::MaxOrdersReached => Color::Yellow,
                        DcaState::Error(_) => Color::LightRed,
                    })),
                    Span::styled(
                        slot.strategy.state.label().to_string(),
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("── POSITION ────────────────", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled(avg_label, Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("${:.4}", avg), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(invested_label, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("${:.2} {}", invested, quote_asset),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(qty_label, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:.6} {}", qty, base_asset),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Orders:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{} / {}", orders_count, max_orders),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(entry_label, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!(" ${:.2}  Today: ${:.2}", quote_amount, daily_spent),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled("  Next: ", Style::default().fg(Color::DarkGray)),
                    {
                        let can_buy = match direction {
                            TradeDirection::Long => quote_bal >= quote_amount,
                            TradeDirection::Short => {
                                let needed_qty = if price > 0.0 { quote_amount / price } else { 0.0 };
                                base_bal >= needed_qty
                            }
                        };
                        if can_buy {
                            Span::styled("✓ OK", Style::default().fg(Color::Green))
                        } else {
                            Span::styled("⚠ LOW", Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD))
                        }
                    }
                ]),
                Line::from(vec![
                    Span::styled(" Liq. Safety: ", Style::default().fg(Color::DarkGray)),
                    {
                        let warning = match direction {
                            TradeDirection::Long => {
                                if qty > 0.0 && base_bal < (qty * 0.9995) {
                                    Some(format!("Need {:.6} more {}", qty - base_bal, base_asset))
                                } else { None }
                            }
                            TradeDirection::Short => {
                                let cost = qty * price;
                                if qty > 0.0 && quote_bal < cost {
                                    Some(format!("Need ${:.2} more {}", cost - quote_bal, quote_asset))
                                } else { None }
                            }
                        };
                        if let Some(msg) = warning {
                            Span::styled(format!("⚠ INSUFFICIENT ({})", msg), Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD))
                        } else if qty > 0.0 {
                            Span::styled("✓ READY TO CLOSE", Style::default().fg(Color::Green))
                        } else {
                            Span::styled("-", Style::default().fg(Color::DarkGray))
                        }
                    }
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("── PERFORMANCE ─────────────", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled(" P&L:        ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{}{:.2} $ ({}{:.2}%)", pnl_sign, pnl, pnl_sign, pnl_pct),
                        Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
                    ),
                ]),
                trailing_line,
            ];

            f.render_widget(
                Paragraph::new(dca_text).block(
                    Block::default()
                        .title(" DCA Strategy ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Magenta)),
                ),
                cols[1],
            );
        }
    }

    // -----------------------------------------------------------
    // Historial de operaciones
    // -----------------------------------------------------------

    fn render_trades(f: &mut Frame, state: &AppState, area: Rect) {
        let slot = match state.selected() {
            Some(s) => s,
            None => {
                f.render_widget(
                    Block::default()
                        .title(" Trade History ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Blue)),
                    area,
                );
                return;
            }
        };

        let price = state.selected_price();
        let direction = &slot.strategy.config.direction;

        let entry_col_header = match direction {
            TradeDirection::Long  => "Buy Price",
            TradeDirection::Short => "Sell Price",
        };
        let header_arr = ["#", entry_col_header, "Quantity", "USDT", "Current P&L", "Date/Time"];
        let header_cells = header_arr.into_iter().map(|h| {
            Cell::from(h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        });
        let header = Row::new(header_cells).height(1).bottom_margin(0);

        let rows: Vec<Row> = slot
            .strategy
            .trades
            .iter()
            .enumerate()
            .rev()
            .map(|(i, t)| {
                let trade_pnl = match direction {
                    TradeDirection::Long  => (price - t.buy_price) * t.quantity,
                    TradeDirection::Short => (t.buy_price - price) * t.quantity,
                };
                let (pnl_color, sign) =
                    if trade_pnl >= 0.0 { (Color::Green, "+") } else { (Color::Red, "") };
                Row::new(vec![
                    Cell::from(format!("{}", i + 1)),
                    Cell::from(format!("${:.4}", t.buy_price)),
                    Cell::from(format!("{:.6}", t.quantity)),
                    Cell::from(format!("${:.2}", t.cost)),
                    Cell::from(format!("{}{:.2}$", sign, trade_pnl))
                        .style(Style::default().fg(pnl_color)),
                    Cell::from(
                        t.timestamp
                            .with_timezone(&chrono::Local)
                            .format("%m-%d %H:%M:%S")
                            .to_string(),
                    ),
                ])
                .height(1)
            })
            .collect();

        let widths = [
            Constraint::Length(4),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(13),
            Constraint::Length(12),
            Constraint::Min(16),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .title(format!(
                        " Trade History ({}) ",
                        slot.strategy.trades.len()
                    ))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Blue)),
            );

        f.render_widget(table, area);
    }

    // -----------------------------------------------------------
    // Log
    // -----------------------------------------------------------

    fn render_log(f: &mut Frame, state: &AppState, area: Rect) {
        let log_lines: Vec<Line> = state
            .log
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|msg| {
                let color = if msg.contains("⚠") || msg.contains("error") || msg.contains("Error") {
                    Color::Red
                } else if msg.contains("STOP LOSS") {
                    Color::Red
                } else if msg.contains("ALERT") {
                    Color::Yellow
                } else if msg.contains("TAKE PROFIT") || msg.contains("TRAILING TP") {
                    Color::Green
                } else if msg.contains("SHORT #") {
                    Color::Cyan
                } else if msg.contains("BUY #") {
                    Color::Green
                } else {
                    Color::Gray
                };
                Line::from(Span::styled(format!(" {}", msg), Style::default().fg(color)))
            })
            .collect();

        f.render_widget(
            Paragraph::new(log_lines)
                .block(
                    Block::default()
                        .title(" Log ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    // -----------------------------------------------------------
    // Footer de controles
    // -----------------------------------------------------------

    fn render_footer(f: &mut Frame, state: &AppState, area: Rect) {
        let controls = match &state.ui_mode {
            UiMode::RestoreSession(_) => vec![
                Span::raw(" "),
                Span::styled("[C / Enter]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Continue  "),
                Span::styled("[N / Esc]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" New session"),
            ],
            UiMode::NewStrategy => vec![
                Span::raw(" "),
                Span::styled("[↑↓]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" Symbol  "),
                Span::styled("[Tab]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" LONG/SHORT  "),
                Span::styled("[←→]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" Restart  "),
                Span::styled("[Enter]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Start  "),
                Span::styled("[Esc]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Cancel"),
            ],
            UiMode::Config => vec![
                Span::raw(" "),
                Span::styled("[0-9 .]", Style::default().fg(Color::Cyan)),
                Span::raw(" Enter amount  "),
                Span::styled("[Enter]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Confirm  "),
                Span::styled("[Esc]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Cancel"),
            ],
            UiMode::PostSale(_slot_id, _) => vec![
                Span::raw(" "),
                Span::styled("[S]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Restart cycle  "),
                Span::styled("[Esc / any key]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(" Stay stopped"),
            ],
            UiMode::ConfirmClose => vec![
                Span::raw(" "),
                Span::styled("[Enter / Y]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Close market position  "),
                Span::styled("[Esc / N]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(" Cancel"),
            ],
            UiMode::ConfirmDelete => vec![
                Span::raw(" "),
                Span::styled("[Enter / Y]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Confirm delete slot  "),
                Span::styled("[Esc / N]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(" Cancel"),
            ],
            UiMode::Normal => vec![
                Span::raw(" "),
                Span::styled("[S]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" New  "),
                Span::styled("[X]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(if state.selected_slot_is_active() { " Pause  " } else { " Start  " }),
                Span::styled("[V]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Sell now  "),
                Span::styled("[F]", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                Span::raw(" Flip  "),
                Span::styled("[D]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Delete  "),
                Span::styled("[C]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" Config  "),
                Span::styled("[↑↓]", Style::default().fg(Color::Cyan)),
                Span::raw(" Slots  "),
                Span::styled("[Q]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Exit"),
            ],
        };

        f.render_widget(
            Paragraph::new(Line::from(controls))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .alignment(Alignment::Left),
            area,
        );
    }

    // -----------------------------------------------------------
    // Modal: restauración de sesiones anteriores
    // -----------------------------------------------------------

    fn render_restore_session_panel(
        f: &mut Frame,
        slots_info: &[(String, TradeDirection, usize, bool)],
    ) {
        let size = f.area();
        let slot_count = slots_info.len().max(1);
        let popup_h = (9 + slot_count as u16).min(size.height.saturating_sub(4));
        let popup_w = 54u16.min(size.width.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default()
                .title(" ↩ Previous sessions found ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Saved sessions:",
                Style::default().fg(Color::White),
            )),
            Line::from(""),
        ];

        for (sym, dir, count, active) in slots_info {
            let (dir_label, dir_color) = match dir {
                TradeDirection::Long  => ("▲ LONG",  Color::Green),
                TradeDirection::Short => ("▼ SHORT", Color::Red),
            };
            let trade_label = if *count == 1 { "buy" } else { "buys" };
            let status = if *active { "  ACTIVE" } else { "" };
            lines.push(Line::from(vec![
                Span::styled("  ● ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    sym.clone(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(dir_label, Style::default().fg(dir_color)),
                Span::styled(
                    format!("  {} {}", count, trade_label),
                    Style::default().fg(Color::White),
                ),
                Span::styled(status, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Do you want to continue where you left off?",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  [C / Enter] ",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::styled("Continue previous session", Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(
                "  [N / Esc]   ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Discard and start from scratch",
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        f.render_widget(Paragraph::new(lines), inner);
    }

    // -----------------------------------------------------------
    // Modal: nueva estrategia (S)
    // -----------------------------------------------------------

    fn render_new_strategy_panel(f: &mut Frame, state: &AppState) {
        let size = f.area();
        let popup_w = 46u16.min(size.width.saturating_sub(4));
        let popup_h = 17u16.min(size.height.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default()
                .title(" ▶ New DCA Strategy ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let used_symbols: Vec<String> = state.slots.iter().map(|s| s.symbol.clone()).collect();

        let sel_style =
            Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD);
        let used_style = Style::default().fg(Color::DarkGray);
        let normal_style = Style::default().fg(Color::White);

        let dir_long_style = if state.new_strat_direction == TradeDirection::Long {
            Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let dir_short_style = if state.new_strat_direction == TradeDirection::Short {
            Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let manual_style = if !state.new_strat_auto_restart {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let auto_style = if state.new_strat_auto_restart {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let flip_off_style = if !state.new_strat_auto_flip {
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let flip_on_style = if state.new_strat_auto_flip {
            Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let bnb_off_style = if !state.new_strat_has_bnb {
            Style::default().fg(Color::Black).bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let bnb_on_style = if state.new_strat_has_bnb {
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        // Lista de símbolos con scroll (visible = 5 a la vez)
        let visible = 5usize;
        let sel = state.new_strat_symbol_idx.min(state.symbols.len().saturating_sub(1));
        let offset = if sel + 1 > visible { sel + 1 - visible } else { 0 };

        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            " Symbol (↑↓):",
            Style::default().fg(Color::DarkGray),
        ))];

        for (idx, sym) in state.symbols.iter().enumerate().skip(offset).take(visible) {
            let is_sel = idx == state.new_strat_symbol_idx;
            let is_used = used_symbols.contains(sym);
            let prefix = if is_sel { " ► " } else { "   " };
            let label = if is_used {
                format!("{}{} ← in use", prefix, sym)
            } else {
                format!("{}{}", prefix, sym)
            };
            let style = if is_sel {
                sel_style
            } else if is_used {
                used_style
            } else {
                normal_style
            };
            lines.push(Line::from(Span::styled(label, style)));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" Direction (Tab):  ", Style::default().fg(Color::DarkGray)),
            Span::styled(" ▲ LONG ", dir_long_style),
            Span::raw("  "),
            Span::styled(" ▼ SHORT ", dir_short_style),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" Restart (←→):     ", Style::default().fg(Color::DarkGray)),
            Span::styled(" Manual ", manual_style),
            Span::raw("  "),
            Span::styled(" Auto ", auto_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Dir Flip (F):     ", Style::default().fg(Color::DarkGray)),
            Span::styled(" Off ", flip_off_style),
            Span::raw("  "),
            Span::styled(" ▲↔▼ Invert Dir ↺ ", flip_on_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled("   ↳ Flips Long↔Short direction after each TP", Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Pay Fees w/ BNB(B):", Style::default().fg(Color::DarkGray)),
            Span::styled(" No ", bnb_off_style),
            Span::raw("      "),
            Span::styled(" Yes (25% Disc) ", bnb_on_style),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" [Enter] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled("Create and Start Strategy", Style::default().fg(Color::White)),
            Span::styled("[Esc] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
        ]));

        f.render_widget(Paragraph::new(lines), inner);
    }

    // -----------------------------------------------------------
    // Panel de configuración (solo monto USDT)
    // -----------------------------------------------------------

    fn render_config_panel(f: &mut Frame, state: &AppState) {
        let size = f.area();
        let popup_w = 46u16.min(size.width.saturating_sub(4));
        let popup_h = 13u16.min(size.height.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default()
                .title(" ⚙ Global Config ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let current = state
            .selected()
            .map(|s| s.strategy.config.quote_amount)
            .unwrap_or(0.0);
        let buf = &state.cfg_amount_buf;
        let has_bnb = state.cfg_has_bnb;

        let bnb_on_style = if has_bnb {
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let bnb_off_style = if !has_bnb {
            Style::default().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(" USDT Amount: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}▌", if buf.is_empty() { "_" } else { buf }),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" (Current: ${:.1})", current), Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Pay Fees w/ BNB (B): ", Style::default().fg(Color::DarkGray)),
                Span::styled(" No ", bnb_off_style),
                Span::raw(" "),
                Span::styled(" Yes (25% Disc) ", bnb_on_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                " (these settings apply to ALL active slots)",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    " [Enter] ",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Save All    ", Style::default().fg(Color::White)),
                Span::styled(
                    " [Esc] ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
            ]),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }

    // -----------------------------------------------------------
    // Overlay: confirmación de cierre manual (V)
    // -----------------------------------------------------------

    fn render_confirm_close_panel(f: &mut Frame, state: &AppState) {
        let size = f.area();
        let popup_w = 50u16.min(size.width.saturating_sub(4));
        let popup_h = 12u16.min(size.height.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default()
                .title(" ⚡ Market Close Position ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let slot = state.selected();
        let price = state.selected_price();

        let (symbol, qty, pnl, pnl_pct, dir_label, quote) = if let Some(sl) = slot {
            let dir = match sl.strategy.config.direction {
                TradeDirection::Long  => "Market SELL",
                TradeDirection::Short => "Market BUY (rebuy)",
            };
            (
                sl.symbol.clone(),
                sl.strategy.total_quantity(),
                sl.strategy.pnl(price),
                sl.strategy.pnl_pct(price),
                dir,
                sl.quote_asset.clone(),
            )
        } else {
            return;
        };

        let (pnl_color, pnl_sign) = if pnl >= 0.0 { (Color::Green, "+") } else { (Color::Red, "") };

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Pair:      ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    symbol,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Action:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(dir_label, Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("  Quantity: ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:.6}", qty), Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  Curr. P&L: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}{:.2} {} ({}{:.2}%)", pnl_sign, pnl, quote, pnl_sign, pnl_pct),
                    Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  This action does not wait for take profit.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  [Enter / Y] ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Execute now    ", Style::default().fg(Color::White)),
                Span::styled(
                    "[Esc / N] ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
            ]),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_confirm_delete_panel(f: &mut Frame, state: &AppState) {
        let size = f.area();
        let has_position = state.selected().map(|sl| sl.strategy.total_quantity() > 0.0).unwrap_or(false);
        
        // Ajustar altura si hay advertencia de posición
        let popup_h = if has_position { 12u16 } else { 10u16 }.min(size.height.saturating_sub(4));
        let popup_w = 55u16.min(size.width.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default()
                .title(" 🗑 Borrar Slot ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let symbol = state.selected().map(|sl| sl.symbol.clone()).unwrap_or_default();

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  ¿Confirmas borrar el slot de ", Style::default().fg(Color::White)),
                Span::styled(
                    symbol,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw("?"),
            ]),
        ];

        if has_position {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ⚠ POSICIÓN ABIERTA DETECTADA", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  Si borras, el bot dejará de gestionarla.", Style::default().fg(Color::Red)),
            ]));
        } else {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Se perderá el historial local de este ciclo.",
                Style::default().fg(Color::DarkGray),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  [Enter / Y] ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled("Borrar ahora   ", Style::default().fg(Color::White)),
            Span::styled(
                "[Esc / N] ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled("Cancelar", Style::default().fg(Color::DarkGray)),
        ]));

        f.render_widget(Paragraph::new(lines), inner);
    }

    // -----------------------------------------------------------
    // Overlay post-venta
    // -----------------------------------------------------------

    fn render_post_sale_panel(f: &mut Frame, result: &SaleResult, quote_asset: &str) {
        let size = f.area();
        let popup_w = 50u16.min(size.width.saturating_sub(4));
        let popup_h = 13u16.min(size.height.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);

        let (border_color, _title_color) = if result.kind == "STOP LOSS" {
            (Color::Red, Color::Red)
        } else {
            (Color::Green, Color::Green)
        };

        f.render_widget(
            Block::default()
                .title(format!(" {} ", result.kind))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color).add_modifier(Modifier::BOLD)),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let (pnl_color, pnl_sign) = if result.pnl >= 0.0 {
            (Color::Green, "+")
        } else {
            (Color::Red, "")
        };

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("Received:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("${:.2} {}", result.received, quote_asset),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Profit:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "{}{:.2} {} ({}{:.2}%)",
                        pnl_sign, result.pnl, quote_asset, pnl_sign, result.pnl_pct
                    ),
                    Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "─────────────────────────────────────",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "What do you want to do?",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  [S] ",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Restart DCA cycle immediately",
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "  [Esc / any key] ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Stay stopped"),
            ]),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }
}
