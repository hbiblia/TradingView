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
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
    },
    Frame, Terminal,
};
use tokio::sync::{mpsc, Mutex};

use crate::app::{AppCommand, AppState, SaleResult, UiMode, SYMBOLS};
use crate::strategy::dca::DcaState;

const TICK_MS: u64 = 150; // ~6 FPS de refresco

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    state: Arc<Mutex<AppState>>,
    cmd_tx: mpsc::Sender<AppCommand>,
    trade_table_state: TableState,
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

        Ok(Self {
            terminal,
            state,
            cmd_tx,
            trade_table_state: TableState::default(),
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut event_stream = EventStream::new();
        let tick = Duration::from_millis(TICK_MS);

        loop {
            // Render
            {
                let state = self.state.lock().await;
                let ts = &self.trade_table_state;
                self.terminal.draw(|f| Self::render(f, &state, ts))?;
            }

            // Leer eventos con timeout
            tokio::select! {
                _ = tokio::time::sleep(tick) => {
                    // solo redibuja
                }
                maybe_event = event_stream.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            if self.handle_key(key.code, key.modifiers).await? {
                                break; // quit
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

    async fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
        let ui_mode = self.state.lock().await.ui_mode.clone();

        match ui_mode {
            UiMode::RestoreSession(_) => {
                match code {
                    KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Enter => {
                        let _ = self.cmd_tx.send(AppCommand::RestoreSessionContinue).await;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        let _ = self.cmd_tx.send(AppCommand::RestoreSessionDiscard).await;
                    }
                    _ => {}
                }
            }
            UiMode::PostSale(_) => {
                match code {
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        let _ = self.cmd_tx.send(AppCommand::PostSaleRestart).await;
                    }
                    KeyCode::Esc | KeyCode::Enter
                    | KeyCode::Char('x') | KeyCode::Char('X')
                    | KeyCode::Char('q') | KeyCode::Char('Q') => {
                        let _ = self.cmd_tx.send(AppCommand::PostSaleDismiss).await;
                    }
                    _ => {}
                }
            }
            UiMode::ConfirmStart => {
                match code {
                    KeyCode::Enter | KeyCode::Char('s') | KeyCode::Char('S') => {
                        let _ = self.cmd_tx.send(AppCommand::Start).await;
                    }
                    // Cambiar entre Manual / Automático
                    KeyCode::Tab | KeyCode::Left | KeyCode::Right
                    | KeyCode::Char('h') | KeyCode::Char('l') => {
                        let _ = self.cmd_tx.send(AppCommand::ConfirmToggleAutoRestart).await;
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N')
                    | KeyCode::Char('q') | KeyCode::Char('Q') => {
                        let _ = self.cmd_tx.send(AppCommand::CloseConfig).await;
                    }
                    _ => {}
                }
            }
            UiMode::Config => {
                match code {
                    KeyCode::Esc => {
                        let _ = self.cmd_tx.send(AppCommand::CloseConfig).await;
                    }
                    KeyCode::Tab => {
                        let _ = self.cmd_tx.send(AppCommand::CfgTabNext).await;
                    }
                    KeyCode::Enter => {
                        let _ = self.cmd_tx.send(AppCommand::CfgConfirm).await;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let tab = self.state.lock().await.cfg_tab;
                        if tab == 0 {
                            let _ = self.cmd_tx.send(AppCommand::CfgNavUp).await;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let tab = self.state.lock().await.cfg_tab;
                        if tab == 0 {
                            let _ = self.cmd_tx.send(AppCommand::CfgNavDown).await;
                        }
                    }
                    KeyCode::Char(c) => {
                        let tab = self.state.lock().await.cfg_tab;
                        if tab == 1 {
                            let _ = self.cmd_tx.send(AppCommand::CfgInputChar(c)).await;
                        }
                    }
                    KeyCode::Backspace => {
                        let tab = self.state.lock().await.cfg_tab;
                        if tab == 1 {
                            let _ = self.cmd_tx.send(AppCommand::CfgBackspace).await;
                        }
                    }
                    _ => {}
                }
            }
            UiMode::Normal => {
                match code {
                    // Salir
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                        let _ = self.cmd_tx.send(AppCommand::Quit).await;
                        return Ok(true);
                    }
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = self.cmd_tx.send(AppCommand::Quit).await;
                        return Ok(true);
                    }
                    // Iniciar estrategia (muestra modal de confirmación)
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        let _ = self.cmd_tx.send(AppCommand::OpenConfirmStart).await;
                    }
                    // Detener estrategia
                    KeyCode::Char('x') | KeyCode::Char('X') => {
                        let _ = self.cmd_tx.send(AppCommand::Stop).await;
                    }
                    // Abrir panel de configuración
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        let _ = self.cmd_tx.send(AppCommand::OpenConfig).await;
                    }
                    // Scroll tabla de trades
                    KeyCode::Up | KeyCode::Char('k') => {
                        let state = self.state.lock().await;
                        let len = state.strategy.trades.len();
                        drop(state);
                        if len > 0 {
                            let i = self
                                .trade_table_state
                                .selected()
                                .map(|i| if i == 0 { len - 1 } else { i - 1 })
                                .unwrap_or(0);
                            self.trade_table_state.select(Some(i));
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let state = self.state.lock().await;
                        let len = state.strategy.trades.len();
                        drop(state);
                        if len > 0 {
                            let i = self
                                .trade_table_state
                                .selected()
                                .map(|i| (i + 1) % len)
                                .unwrap_or(0);
                            self.trade_table_state.select(Some(i));
                        }
                    }
                    _ => {}
                }
            }
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
    // Rendering
    // -----------------------------------------------------------

    fn render(f: &mut Frame, state: &AppState, trade_state: &TableState) {
        let size = f.area();

        // Layout vertical principal
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),  // header
                Constraint::Length(9),  // precio + DCA stats
                Constraint::Min(6),     // trades
                Constraint::Length(7),  // log
                Constraint::Length(3),  // footer (controles)
            ])
            .split(size);

        Self::render_header(f, state, chunks[0]);
        Self::render_stats(f, state, chunks[1]);
        Self::render_trades(f, state, trade_state, chunks[2]);
        Self::render_log(f, state, chunks[3]);
        Self::render_footer(f, state, chunks[4]);

        // Overlay de restauración de sesión (al inicio)
        if let UiMode::RestoreSession(count) = &state.ui_mode {
            Self::render_restore_session_panel(f, *count, state);
        }

        // Overlay de confirmación de inicio
        if state.ui_mode == UiMode::ConfirmStart {
            Self::render_confirm_start_panel(f, state);
        }

        // Overlay de configuración (encima de todo)
        if state.ui_mode == UiMode::Config {
            Self::render_config_panel(f, state);
        }

        // Overlay post-venta
        if let UiMode::PostSale(result) = &state.ui_mode {
            Self::render_post_sale_panel(f, result, &state.quote_asset);
        }
    }

    fn render_header(f: &mut Frame, state: &AppState, area: Rect) {
        let symbol = format!(
            "{} / {}",
            state.base_asset, state.quote_asset
        );
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");

        let (status_color, status_label) = match &state.strategy.state {
            DcaState::Running => (Color::Green, "● ACTIVO"),
            DcaState::TakeProfitReached => (Color::Cyan, "✓ TAKE PROFIT"),
            DcaState::StopLossReached => (Color::Red, "✗ STOP LOSS"),
            DcaState::MaxOrdersReached => (Color::Yellow, "■ MAX ÓRDENES"),
            DcaState::Error(_) => (Color::Red, "✗ ERROR"),
            DcaState::Idle => (Color::DarkGray, "○ DETENIDO"),
        };

        let title_spans = vec![
            Span::styled(
                " BINANCE DCA BOT ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("│ "),
            Span::styled(&symbol, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::raw(" │ "),
            Span::styled(status_label, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
            Span::raw(" │ "),
            Span::styled(now.to_string(), Style::default().fg(Color::DarkGray)),
            Span::raw(" "),
        ];

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

    fn render_stats(f: &mut Frame, state: &AppState, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(area);

        // Panel izquierdo: Precio y Balances
        {
            let change_color = if state.change_24h_pct >= 0.0 { Color::Green } else { Color::Red };
            let change_sign = if state.change_24h_pct >= 0.0 { "+" } else { "" };

            let price_text = vec![
                Line::from(vec![
                    Span::styled(
                        format!(" ${:.2}", state.current_price),
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!("{}{:.2}% 24h", change_sign, state.change_24h_pct),
                        Style::default().fg(change_color),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" H: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("${:.2}", state.high_24h), Style::default().fg(Color::Green)),
                    Span::styled("  L: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("${:.2}", state.low_24h), Style::default().fg(Color::Red)),
                ]),
                Line::from(""),
                Line::from(Span::styled(" Balance ", Style::default().fg(Color::DarkGray))),
                Line::from(vec![
                    Span::styled(format!(" {}: ", state.base_asset), Style::default().fg(Color::Yellow)),
                    Span::styled(format!("{:.6}", state.base_balance), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(format!(" {}: ", state.quote_asset), Style::default().fg(Color::Yellow)),
                    Span::styled(format!("{:.2}", state.quote_balance), Style::default().fg(Color::White)),
                ]),
            ];

            f.render_widget(
                Paragraph::new(price_text).block(
                    Block::default()
                        .title(" Precio ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
                cols[0],
            );
        }

        // Panel derecho: DCA Stats
        {
            let avg = state.strategy.average_cost();
            let invested = state.strategy.total_invested();
            let qty = state.strategy.total_quantity();
            let pnl = state.strategy.pnl(state.current_price);
            let pnl_pct = state.strategy.pnl_pct(state.current_price);
            let orders_count = state.strategy.trades.len();
            let max_orders = state.strategy.config.max_orders;
            let countdown = state.strategy.next_buy_countdown();
            let daily_spent = state.strategy.daily_spent;
            let quote_amount = state.strategy.config.quote_amount;
            let price_peak = state.strategy.price_peak;
            let trailing_trigger = state.strategy.trailing_tp_trigger_price();
            let trailing_configured = state.strategy.config.trailing_tp_pct > 0.0;

            let (pnl_color, pnl_sign) = if pnl >= 0.0 { (Color::Green, "+") } else { (Color::Red, "") };

            // Línea de trailing TP: mostrar peak y trigger si está activo
            let trailing_line = if trailing_configured && price_peak > 0.0 {
                let drop_so_far = ((price_peak - state.current_price) / price_peak) * 100.0;
                let trigger_color = if state.current_price <= trailing_trigger { Color::Yellow } else { Color::Cyan };
                Line::from(vec![
                    Span::styled(" Trail TP:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("peak ${:.4}  vende <${:.4} ({:.2}%↓)",
                            price_peak, trailing_trigger, drop_so_far),
                        Style::default().fg(trigger_color),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(" Próx compra: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(countdown, Style::default().fg(Color::Cyan)),
                ])
            };

            let dca_text = vec![
                Line::from(vec![
                    Span::styled(" Costo prom: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("${:.4}", avg), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Invertido:  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("${:.2} {}", invested, state.quote_asset), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Cantidad:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{:.6} {}", qty, state.base_asset), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" P&L:        ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{}{:.2} $ ({}{:.2}%)", pnl_sign, pnl, pnl_sign, pnl_pct),
                        Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
                    ),
                ]),
                trailing_line,
                Line::from(vec![
                    Span::styled(" Órdenes:    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{} / {}", orders_count, max_orders), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Monto/compra:", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!(" ${:.2}  Gasto hoy: ${:.2}", quote_amount, daily_spent),
                        Style::default().fg(Color::Yellow),
                    ),
                ]),
            ];

            f.render_widget(
                Paragraph::new(dca_text).block(
                    Block::default()
                        .title(" Estrategia DCA ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Magenta)),
                ),
                cols[1],
            );
        }
    }

    fn render_trades(f: &mut Frame, state: &AppState, table_state: &TableState, area: Rect) {
        let header_cells = ["#", "Precio Compra", "Cantidad", "Costo (USDT)", "P&L actual", "Fecha/Hora"]
            .iter()
            .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        let header = Row::new(header_cells).height(1).bottom_margin(0);

        let current_price = state.current_price;
        let rows: Vec<Row> = state
            .strategy
            .trades
            .iter()
            .enumerate()
            .rev()
            .map(|(i, t)| {
                let trade_pnl = (current_price - t.buy_price) * t.quantity;
                let (pnl_color, sign) = if trade_pnl >= 0.0 { (Color::Green, "+") } else { (Color::Red, "") };
                Row::new(vec![
                    Cell::from(format!("{}", i + 1)),
                    Cell::from(format!("${:.4}", t.buy_price)),
                    Cell::from(format!("{:.6}", t.quantity)),
                    Cell::from(format!("${:.2}", t.cost)),
                    Cell::from(format!("{}{:.2}$", sign, trade_pnl)).style(Style::default().fg(pnl_color)),
                    Cell::from(t.timestamp.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S").to_string()),
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
                    .title(format!(" Historial de Operaciones ({}) ", state.strategy.trades.len()))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Blue)),
            )
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("► ");

        let mut ts = table_state.clone();
        f.render_stateful_widget(table, area, &mut ts);
    }

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
                } else if msg.contains("BUY") || msg.contains("TAKE PROFIT") {
                    Color::Green
                } else if msg.contains("STOP LOSS") || msg.contains("SELL") {
                    Color::Red
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

    fn render_footer(f: &mut Frame, state: &AppState, area: Rect) {
        let controls = match &state.ui_mode {
            UiMode::RestoreSession(_) => vec![
                Span::raw(" "),
                Span::styled("[C / Enter]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Continuar  "),
                Span::styled("[N / Esc]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Nueva sesión"),
            ],
            UiMode::ConfirmStart => vec![
                Span::raw(" "),
                Span::styled("[Tab / ←→]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" Manual/Auto  "),
                Span::styled("[Enter / S]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Iniciar  "),
                Span::styled("[Esc / N]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Cancelar"),
            ],
            UiMode::Config => vec![
                Span::raw(" "),
                Span::styled("[Tab]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" Cambiar tab  "),
                Span::styled("[↑↓]", Style::default().fg(Color::Cyan)),
                Span::raw(" Navegar  "),
                Span::styled("[Enter]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Confirmar  "),
                Span::styled("[Esc]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Cerrar"),
            ],
            UiMode::PostSale(_) => vec![
                Span::raw(" "),
                Span::styled("[S]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Reiniciar ciclo  "),
                Span::styled("[Esc]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(" Quedar detenido"),
            ],
            UiMode::Normal => vec![
                Span::raw(" "),
                Span::styled("[S]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" Iniciar  "),
                Span::styled("[X]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(" Detener  "),
                Span::styled("[C]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" Config  "),
                Span::styled("[↑↓]", Style::default().fg(Color::Cyan)),
                Span::raw(" Scroll  "),
                Span::styled("[Q]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Salir"),
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
    // Modal de restauración de sesión anterior
    // -----------------------------------------------------------

    fn render_restore_session_panel(f: &mut Frame, count: usize, state: &AppState) {
        let size = f.area();

        let popup_w = 52u16.min(size.width.saturating_sub(4));
        let popup_h = 13u16.min(size.height.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default()
                .title(" ↩ Sesión anterior encontrada ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let trade_label = if count == 1 { "compra" } else { "compras" };
        let symbol = format!("{}{}", state.base_asset, state.quote_asset);
        let invested = state.strategy.total_invested();
        let avg = state.strategy.average_cost();

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Se encontró una sesión de ", Style::default().fg(Color::White)),
                Span::styled(&symbol, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(" con:", Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ● ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{} {} registrada(s)", count, trade_label),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("  ● ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("Invertido: ${:.2}  Precio prom: ${:.4}", invested, avg),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  ¿Deseas continuar donde lo dejaste?",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [C / Enter] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled("Continuar sesión anterior", Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  [N / Esc]   ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::styled("Descartar y empezar desde cero", Style::default().fg(Color::DarkGray)),
            ]),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }

    // -----------------------------------------------------------
    // Modal de confirmación de inicio
    // -----------------------------------------------------------

    fn render_confirm_start_panel(f: &mut Frame, state: &AppState) {
        let size = f.area();

        let popup_w = 50u16.min(size.width.saturating_sub(4));
        let popup_h = 14u16.min(size.height.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default()
                .title(" ▶ Iniciar ciclo DCA ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            area,
        );

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let cfg = &state.strategy.config;
        let interval_h = cfg.interval_minutes / 60;
        let interval_m = cfg.interval_minutes % 60;
        let interval_str = if interval_h > 0 {
            format!("{}h {}min", interval_h, interval_m)
        } else {
            format!("{}min", interval_m)
        };
        let tp_str = if cfg.take_profit_pct > 0.0 {
            format!("{:.1}%", cfg.take_profit_pct)
        } else {
            "desact.".to_string()
        };
        let sl_str = if cfg.stop_loss_pct > 0.0 {
            format!("{:.1}%", cfg.stop_loss_pct)
        } else {
            "desact.".to_string()
        };

        // Estilos del selector Manual / Automático
        let auto = state.confirm_auto_restart;
        let sel_style   = Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
        let unsel_style = Style::default().fg(Color::DarkGray);
        let manual_style = if !auto { sel_style } else { unsel_style };
        let auto_style   = if  auto { sel_style } else { unsel_style };

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Par:        ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}{}", state.base_asset, state.quote_asset),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Monto:      ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("${:.2} por compra", cfg.quote_amount),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Intervalo:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(&interval_str, Style::default().fg(Color::White)),
                Span::styled("   TP: ", Style::default().fg(Color::DarkGray)),
                Span::styled(&tp_str, Style::default().fg(Color::Green)),
                Span::styled("  SL: ", Style::default().fg(Color::DarkGray)),
                Span::styled(&sl_str, Style::default().fg(Color::Red)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  ────────────────────────────────────────",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Tras venta: ", Style::default().fg(Color::DarkGray)),
                Span::styled(" Manual ", manual_style),
                Span::raw("  "),
                Span::styled(" Automático ", auto_style),
                Span::styled("  ← Tab", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    if !auto { "  Pide confirmación antes de reiniciar " }
                             else { "  Reinicia el ciclo DCA sin preguntar  " },
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  ────────────────────────────────────────",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [Enter / S] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled("Iniciar  ", Style::default().fg(Color::White)),
                Span::styled("[Esc / N] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::styled("Cancelar", Style::default().fg(Color::DarkGray)),
            ]),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }

    // -----------------------------------------------------------
    // Panel de configuración (overlay)
    // -----------------------------------------------------------

    fn render_config_panel(f: &mut Frame, state: &AppState) {
        let size = f.area();

        let popup_w = 54u16.min(size.width.saturating_sub(4));
        let popup_h = 16u16.min(size.height.saturating_sub(4));
        let popup_x = (size.width.saturating_sub(popup_w)) / 2;
        let popup_y = (size.height.saturating_sub(popup_h)) / 2;
        let area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

        // Limpiar el área (fondo opaco)
        f.render_widget(Clear, area);

        // Borde exterior
        f.render_widget(
            Block::default()
                .title(" ⚙ Configuración ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            area,
        );

        // Área interior
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner);

        // Tabs
        let tab_line = Line::from(vec![
            Span::styled(
                " Moneda ",
                if state.cfg_tab == 0 {
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw("  "),
            Span::styled(
                " Monto USDT ",
                if state.cfg_tab == 1 {
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
        ]);
        f.render_widget(Paragraph::new(tab_line), chunks[0]);

        if state.cfg_tab == 0 {
            Self::render_symbol_tab(f, state, chunks[1]);
        } else {
            Self::render_amount_tab(f, state, chunks[1]);
        }
    }

    fn render_symbol_tab(f: &mut Frame, state: &AppState, area: Rect) {
        let current_symbol = format!("{}{}", state.base_asset, state.quote_asset);
        let visible = area.height as usize;

        let offset = if state.cfg_symbol_idx + 1 > visible {
            state.cfg_symbol_idx + 1 - visible
        } else {
            0
        };

        let lines: Vec<Line> = SYMBOLS
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(idx, &sym)| {
                let is_selected = idx == state.cfg_symbol_idx;
                let is_current = sym == current_symbol;

                let label = if is_current {
                    format!(" {} ← actual", sym)
                } else {
                    format!(" {}", sym)
                };

                let style = if is_selected {
                    Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)
                } else if is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::White)
                };

                Line::from(Span::styled(label, style))
            })
            .collect();

        f.render_widget(Paragraph::new(lines), area);
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

        let (border_color, title_color) = if result.kind == "STOP LOSS" {
            (Color::Red, Color::Red)
        } else {
            (Color::Green, Color::Green)
        };

        let title = format!(" {} ", result.kind);
        f.render_widget(
            Block::default()
                .title(title)
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
                Span::styled("Recibido:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("${:.2} {}", result.received, quote_asset),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Ganancia:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}{:.2} {} ({}{:.2}%)", pnl_sign, result.pnl, quote_asset, pnl_sign, result.pnl_pct),
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
                "¿Qué querés hacer?",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [S] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled("Reiniciar ciclo DCA inmediatamente", Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  [Esc] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled("Quedar detenido", Style::default().fg(Color::DarkGray)),
            ]),
        ];

        f.render_widget(
            Paragraph::new(lines).style(Style::default().fg(title_color)),
            inner,
        );
    }

    fn render_amount_tab(f: &mut Frame, state: &AppState, area: Rect) {
        let current = state.strategy.config.quote_amount;
        let buf = &state.cfg_amount_buf;

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(" Actual: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("${:.2} USDT", current),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Nuevo:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}▌", if buf.is_empty() { "_" } else { buf }),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" USDT", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                " Mínimo: $1.00  (Binance requiere ~$5)",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        f.render_widget(Paragraph::new(lines), area);
    }
}
