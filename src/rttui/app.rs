use anyhow::{anyhow, Result};
use crossterm::{
    event::{self, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use probe_rs_rtt::RttChannel;
use std::convert::TryInto;
use std::io::{Read, Seek, Write};
use textwrap::wrap_iter;
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    widgets::{Axis, Block, Borders, Chart, Dataset, List, Paragraph, Tabs, Text},
    Terminal,
};

use super::channel::ChannelState;
use super::event::{Event, Events};
use super::DataFormat;

use event::{DisableMouseCapture, KeyModifiers};

/// App holds the state of the application
pub struct App {
    tabs: Vec<ChannelState>,
    current_tab: usize,

    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    events: Events,
}

fn pull_channel<C: RttChannel>(channels: &mut Vec<C>, n: usize) -> Option<C> {
    let c = channels
        .iter()
        .enumerate()
        .find_map(|(i, c)| if c.number() == n { Some(i) } else { None });

    c.map(|c| channels.remove(c))
}

impl App {
    pub fn new(mut rtt: probe_rs_rtt::Rtt, config: &crate::config::Config) -> Result<Self> {
        let mut tabs = Vec::new();
        let mut up_channels = rtt.up_channels().drain().collect::<Vec<_>>();
        let mut down_channels = rtt.down_channels().drain().collect::<Vec<_>>();
        if !config.rtt.channels.is_empty() {
            for channel in &config.rtt.channels {
                tabs.push(ChannelState::new(
                    channel.up.and_then(|up| pull_channel(&mut up_channels, up)),
                    channel
                        .down
                        .and_then(|down| pull_channel(&mut down_channels, down)),
                    channel.name.clone(),
                    config.rtt.show_timestamps,
                ))
            }
        } else {
            for channel in up_channels.into_iter() {
                let number = channel.number();
                tabs.push(ChannelState::new(
                    Some(channel),
                    pull_channel(&mut down_channels, number),
                    None,
                    config.rtt.show_timestamps,
                ));
            }

            for channel in down_channels {
                tabs.push(ChannelState::new(
                    None,
                    Some(channel),
                    None,
                    config.rtt.show_timestamps,
                ));
            }
        }

        // Code farther down relies on tabs being configured and might panic
        // otherwise.
        if tabs.len() == 0 {
            return Err(anyhow!(
                "Failed to initialize RTT UI: No RTT channels configured"
            ));
        }

        let events = Events::new();

        enable_raw_mode().unwrap();
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen).unwrap();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).unwrap();
        let _ = terminal.hide_cursor();

        Ok(Self {
            tabs,
            current_tab: 0,

            terminal,
            events,
        })
    }

    pub fn get_rtt_symbol<'b, T: Read + Seek>(file: &'b mut T) -> Option<u64> {
        let mut buffer = Vec::new();
        if let Ok(_) = file.read_to_end(&mut buffer) {
            if let Ok(binary) = goblin::elf::Elf::parse(&buffer.as_slice()) {
                for sym in &binary.syms {
                    if let Some(Ok(name)) = binary.strtab.get(sym.st_name) {
                        if name == "_SEGGER_RTT" {
                            return Some(sym.st_value);
                        }
                    }
                }
            }
        }

        log::warn!("No RTT header info was present in the ELF file. Does your firmware run RTT?");
        None
    }

    pub fn render(&mut self) {
        let input = self.current_tab().input().to_owned();
        let has_down_channel = self.current_tab().has_down_channel();
        let scroll_offset = self.current_tab().scroll_offset();
        let messages = self.current_tab().messages().clone();
        let data = self.current_tab().data().clone();
        let mut messages_wrapped: Vec<String> = Vec::new();
        let tabs = &self.tabs;
        let current_tab = self.current_tab;
        let mut height = 0;

        match current_tab {
            //String todo deal with enums instead
            0 => {
                self.terminal
                    .draw(|mut f| {
                        let constraints = if has_down_channel {
                            &[
                                Constraint::Length(1),
                                Constraint::Min(1),
                                Constraint::Length(1),
                            ][..]
                        } else {
                            &[Constraint::Length(1), Constraint::Min(1)][..]
                        };
                        let chunks = Layout::default()
                            .direction(Direction::Vertical)
                            .margin(0)
                            .constraints(constraints)
                            .split(f.size());

                        let tab_names = tabs.iter().map(|t| t.name()).collect::<Vec<_>>();
                        let tabs = Tabs::default()
                            .titles(&tab_names.as_slice())
                            .select(current_tab)
                            .style(Style::default().fg(Color::Black).bg(Color::Yellow))
                            .highlight_style(
                                Style::default()
                                    .fg(Color::Green)
                                    .bg(Color::Yellow)
                                    .modifier(Modifier::BOLD),
                            );
                        f.render_widget(tabs, chunks[0]);

                        height = chunks[1].height as usize;

                        // We need to collect to generate message_num :(
                        messages_wrapped = messages
                            .iter()
                            .map(|m| {
                                wrap_iter(m, chunks[1].width as usize).map(|cow| cow.into_owned())
                            })
                            .flatten()
                            .collect();

                        let message_num = messages_wrapped.len();

                        let messages: Vec<Text> = messages_wrapped
                            .iter()
                            .skip(message_num - (height + scroll_offset).min(message_num))
                            .take(height)
                            .map(|m| Text::raw(m))
                            .collect();

                        let messages = List::new(messages.iter().cloned())
                            .block(Block::default().borders(Borders::NONE));
                        f.render_widget(messages, chunks[1]);

                        if has_down_channel {
                            let text = [Text::raw(input.clone())];
                            let input = Paragraph::new(text.iter())
                                .style(Style::default().fg(Color::Yellow).bg(Color::Blue));
                            f.render_widget(input, chunks[2]);
                        }
                    })
                    .unwrap();

                let message_num = messages_wrapped.len();
                let scroll_offset = self.tabs[self.current_tab].scroll_offset();
                if message_num < height + scroll_offset {
                    self.current_tab_mut()
                        .set_scroll_offset(message_num - height.min(message_num));
                }
            }
            //binary
            _ => {
                self.terminal
                    .draw(|mut f| {
                        let constraints = if has_down_channel {
                            &[
                                Constraint::Length(1),
                                Constraint::Min(1),
                                Constraint::Length(1),
                            ][..]
                        } else {
                            &[Constraint::Length(1), Constraint::Min(1)][..]
                        };
                        let chunks = Layout::default()
                            .direction(Direction::Vertical)
                            .margin(0)
                            .constraints(constraints)
                            .split(f.size());

                        let tab_names = tabs.iter().map(|t| t.name()).collect::<Vec<_>>();
                        let tabs = Tabs::default()
                            .titles(&tab_names.as_slice())
                            .select(current_tab)
                            .style(Style::default().fg(Color::Black).bg(Color::Yellow))
                            .highlight_style(
                                Style::default()
                                    .fg(Color::Green)
                                    .bg(Color::Yellow)
                                    .modifier(Modifier::BOLD),
                            );
                        f.render_widget(tabs, chunks[0]);

                        let max_x = 128;

                        let dater = data
                            .chunks_exact(4)
                            .map(|bytes| {
                                //impossible to fail?
                                f32::from_le_bytes(bytes.try_into().unwrap())
                            })
                            .rev()
                            .take(max_x * 3)
                            .rev();

                        let x = dater
                            .clone()
                            .step_by(3)
                            .enumerate()
                            .map(|(i, val)| (i as f64, val as f64))
                            .collect::<Vec<(f64, f64)>>();

                        let y = dater
                            .clone()
                            .skip(1)
                            .step_by(3)
                            .enumerate()
                            .map(|(i, val)| (i as f64, val as f64))
                            .collect::<Vec<(f64, f64)>>();

                        let z = dater
                            .clone()
                            .skip(2)
                            .step_by(3)
                            .enumerate()
                            .map(|(i, val)| (i as f64, val as f64))
                            .collect::<Vec<(f64, f64)>>();

                        //in our case no ord for f32 so need a nan datatype to do .min or max
                        let min = -2000.0;
                        let max = 2000.0;

                        let x_labels = [
                            format!("{}", 0.0),
                            format!("{}", (0.0 + x.len() as f64) / 2.0),
                            format!("{}", x.len()),
                        ];
                        let y_labels = &[min.to_string(), "0".to_string(), max.to_string()];

                        let datasets = [
                            Dataset::default()
                                .name("x")
                                .marker(symbols::Marker::Braille)
                                .style(Style::default().fg(Color::Yellow))
                                .data(&x),
                            Dataset::default()
                                .name("y")
                                .marker(symbols::Marker::Braille)
                                .style(Style::default().fg(Color::Blue))
                                .data(&y),
                            Dataset::default()
                                .name("z")
                                .marker(symbols::Marker::Braille)
                                .style(Style::default().fg(Color::Green))
                                .data(&z),
                        ];
                        let chart = Chart::default()
                            .block(
                                Block::default()
                                    .title("Chart 1")
                                    .title_style(
                                        Style::default().fg(Color::Cyan).modifier(Modifier::BOLD),
                                    )
                                    .borders(Borders::ALL),
                            )
                            .x_axis(
                                Axis::default()
                                    .title("X Axis")
                                    .style(Style::default().fg(Color::Gray))
                                    .labels_style(Style::default().modifier(Modifier::ITALIC))
                                    .bounds([0.0, x.len() as f64])
                                    .labels(&x_labels),
                            )
                            .y_axis(
                                Axis::default()
                                    .title("Y Axis")
                                    .style(Style::default().fg(Color::Gray))
                                    .labels_style(Style::default().modifier(Modifier::ITALIC))
                                    .bounds([min, max])
                                    .labels(y_labels),
                            )
                            .datasets(&datasets);
                        f.render_widget(chart, chunks[1]);
                    })
                    .unwrap();
            }
        }
    }

    /// Returns true if the application should exit.
    pub fn handle_event(&mut self) -> bool {
        match self.events.next().unwrap() {
            Event::Input(event) => match event.code {
                KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    clean_up_terminal();
                    let _ = self.terminal.show_cursor();
                    true
                }
                KeyCode::F(n) => {
                    let n = n as usize - 1;
                    if n < self.tabs.len() {
                        self.current_tab = n as usize;
                    }
                    false
                }
                KeyCode::Enter => {
                    self.push_rtt();
                    false
                }
                KeyCode::Char(c) => {
                    self.current_tab_mut().input_mut().push(c);
                    false
                }
                KeyCode::Backspace => {
                    self.current_tab_mut().input_mut().pop();
                    false
                }
                KeyCode::PageUp => {
                    self.current_tab_mut().scroll_up();
                    false
                }
                KeyCode::PageDown => {
                    self.current_tab_mut().scroll_down();
                    false
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub fn current_tab(&self) -> &ChannelState {
        &self.tabs[self.current_tab]
    }

    pub fn current_tab_mut(&mut self) -> &mut ChannelState {
        &mut self.tabs[self.current_tab]
    }

    /// Polls the RTT target for new data on all channels.
    pub fn poll_rtt(&mut self) {
        for (i, channel) in self.tabs.iter_mut().enumerate() {
            //for now, just assume 0 is string everything else is binaryle
            let fmt = match i {
                0 => DataFormat::String,
                _ => DataFormat::BinaryLE,
            };
            channel.poll_rtt(fmt);
        }
    }

    pub fn push_rtt(&mut self) {
        self.tabs[self.current_tab].push_rtt();
    }
}

pub fn clean_up_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}
