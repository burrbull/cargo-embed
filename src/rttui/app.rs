use anyhow::{anyhow, Result};
use crossterm::{
    event::{self, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use probe_rs_rtt::RttChannel;
use std::convert::TryInto;
use std::io::{Read, Seek, Write};
use std::{fmt::write, path::PathBuf};
use textwrap::wrap_iter;
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    text::{Span, Spans},
    widgets::{Axis, Block, Borders, Chart, Dataset, List, ListItem, Paragraph, Tabs},
    Terminal,
};

use super::{
    channel::{ChannelState, DataFormat},
    event::{Event, Events},
};

use event::{DisableMouseCapture, KeyModifiers};

/// App holds the state of the application
pub struct App {
    tabs: Vec<ChannelState>,
    current_tab: usize,

    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    events: Events,
    history_path: Option<PathBuf>,
    logname: String,
}

fn pull_channel<C: RttChannel>(channels: &mut Vec<C>, n: usize) -> Option<C> {
    let c = channels
        .iter()
        .enumerate()
        .find_map(|(i, c)| if c.number() == n { Some(i) } else { None });

    c.map(|c| channels.remove(c))
}

impl App {
    pub fn new(
        mut rtt: probe_rs_rtt::Rtt,
        config: &crate::config::Config,
        logname: String,
    ) -> Result<Self> {
        let mut tabs = Vec::new();
        if !config.rtt.channels.is_empty() {
            let mut up_channels = rtt.up_channels().drain().collect::<Vec<_>>();
            let mut down_channels = rtt.down_channels().drain().collect::<Vec<_>>();
            for channel in &config.rtt.channels {
                tabs.push(ChannelState::new(
                    channel.up.and_then(|up| pull_channel(&mut up_channels, up)),
                    channel
                        .down
                        .and_then(|down| pull_channel(&mut down_channels, down)),
                    channel.name.clone(),
                    config.rtt.show_timestamps,
                    channel.format,
                ))
            }
        } else {
            let up_channels = rtt.up_channels().drain();
            let mut down_channels = rtt.down_channels().drain().collect::<Vec<_>>();
            for channel in up_channels.into_iter() {
                let number = channel.number();
                tabs.push(ChannelState::new(
                    Some(channel),
                    pull_channel(&mut down_channels, number),
                    None,
                    config.rtt.show_timestamps,
                    DataFormat::String,
                ));
            }

            for channel in down_channels {
                tabs.push(ChannelState::new(
                    None,
                    Some(channel),
                    None,
                    config.rtt.show_timestamps,
                    DataFormat::String,
                ));
            }
        }

        // Code farther down relies on tabs being configured and might panic
        // otherwise.
        if tabs.is_empty() {
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

        let history_path = {
            if !config.rtt.log_enabled {
                None
            } else {
                //when is the right time if ever to fail if the directory or file cant be created?
                //should we create the path on startup or when we write
                match std::fs::create_dir_all(&config.rtt.log_path) {
                    Ok(_) => Some(config.rtt.log_path.clone()),
                    Err(_) => {
                        log::warn!("Could not create log directory");
                        None
                    }
                }
            }
        };

        Ok(Self {
            tabs,
            current_tab: 0,
            terminal,
            events,
            history_path,
            logname,
        })
    }

    pub fn get_rtt_symbol<T: Read + Seek>(file: &mut T) -> Option<u64> {
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_ok() {
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

    pub fn render(
        &mut self,
        defmt_state: &Option<(defmt_decoder::Table, Option<defmt_elf2table::Locations>)>,
    ) {
        let input = self.current_tab().input().to_owned();
        let has_down_channel = self.current_tab().has_down_channel();
        let scroll_offset = self.current_tab().scroll_offset();
        let messages = self.current_tab().messages().clone();
        let data = self.current_tab().data().clone();
        let tabs = &self.tabs;
        let current_tab = self.current_tab;
        let mut height = 0;
        let mut messages_wrapped: Vec<String> = Vec::new();

        match tabs[current_tab].format() {
            DataFormat::String => {
                self.terminal
                    .draw(|f| {
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

                        let tab_names = tabs
                            .iter()
                            .map(|t| Spans::from(t.name()))
                            .collect::<Vec<_>>();
                        let tabs = Tabs::new(tab_names)
                            .select(current_tab)
                            .style(Style::default().fg(Color::Black).bg(Color::Yellow))
                            .highlight_style(
                                Style::default()
                                    .fg(Color::Green)
                                    .bg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
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

                        let messages: Vec<ListItem> = messages_wrapped
                            .iter()
                            .skip(message_num - (height + scroll_offset).min(message_num))
                            .take(height)
                            .map(|s| ListItem::new(vec![Spans::from(Span::raw(s))]))
                            .collect();

                        let messages = List::new(messages.as_slice())
                            .block(Block::default().borders(Borders::NONE));
                        f.render_widget(messages, chunks[1]);

                        if has_down_channel {
                            let input = Paragraph::new(Spans::from(vec![Span::raw(input.clone())]))
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
            DataFormat::BinaryLE => {
                self.terminal
                    .draw(|f| {
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

                        let tab_names = tabs
                            .iter()
                            .map(|t| Spans::from(t.name()))
                            .collect::<Vec<_>>();
                        let tabs = Tabs::new(tab_names)
                            .select(current_tab)
                            .style(Style::default().fg(Color::Black).bg(Color::Yellow))
                            .highlight_style(
                                Style::default()
                                    .fg(Color::Green)
                                    .bg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
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

                        let datasets = vec![
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
                        let italic = Style::default().add_modifier(Modifier::ITALIC);
                        let chart = Chart::new(datasets)
                            .block(
                                Block::default()
                                    .title(Span::styled(
                                        "Chart 1",
                                        Style::default()
                                            .fg(Color::Cyan)
                                            .add_modifier(Modifier::BOLD),
                                    ))
                                    .borders(Borders::ALL),
                            )
                            .x_axis(
                                Axis::default()
                                    .title("X Axis")
                                    .style(Style::default().fg(Color::Gray))
                                    .bounds([0.0, x.len() as f64])
                                    .labels(
                                        x_labels.iter().map(|l| Span::styled(l, italic)).collect(),
                                    ),
                            )
                            .y_axis(
                                Axis::default()
                                    .title("Y Axis")
                                    .style(Style::default().fg(Color::Gray))
                                    .bounds([min, max])
                                    .labels(
                                        y_labels.iter().map(|l| Span::styled(l, italic)).collect(),
                                    ),
                            );
                        f.render_widget(chart, chunks[1]);
                    })
                    .unwrap();
            }
            binle_or_defmt => {
                self.terminal
                    .draw(|f| {
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

                        let tab_names = tabs
                            .iter()
                            .map(|t| Spans::from(t.name()))
                            .collect::<Vec<_>>();
                        let tabs = Tabs::new(tab_names)
                            .select(current_tab)
                            .style(Style::default().fg(Color::Black).bg(Color::Yellow))
                            .highlight_style(
                                Style::default()
                                    .fg(Color::Green)
                                    .bg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            );
                        f.render_widget(tabs, chunks[0]);

                        height = chunks[1].height as usize;

                        // probably pretty bad
                        match binle_or_defmt {
                            DataFormat::BinaryLE => {
                                // NOTE: temporary unreachable
                                messages_wrapped.push(data.iter().fold(
                                    String::new(),
                                    |mut output, byte| {
                                        let _ = write(&mut output, format_args!("{:#04x}, ", byte));
                                        output
                                    },
                                ));
                            }
                            DataFormat::Defmt => {
                                let (table, locs) = defmt_state.as_ref().expect(
                                "Running rtt in defmt mode but table or locations could not be loaded.",
                            );
                                let mut frames = vec![];

                                frames.extend_from_slice(&data);

                                while let Ok((frame, consumed)) =
                                    defmt_decoder::decode(&frames, table)
                                {
                                    // NOTE(`[]` indexing) all indices in `table` have already been
                                    // verified to exist in the `locs` map.
                                    let loc = locs.as_ref().map(|locs| &locs[&frame.index()]);

                                    messages_wrapped.push(format!("{}", frame.display(false)));
                                    if let Some(loc) = loc {
                                        let relpath = if let Ok(relpath) =
                                            loc.file.strip_prefix(&std::env::current_dir().unwrap())
                                        {
                                            relpath
                                        } else {
                                            // not relative; use full path
                                            &loc.file
                                        };

                                        messages_wrapped.push(format!(
                                            "└─ {}:{}",
                                            relpath.display(),
                                            loc.line
                                        ));
                                    }

                                    let num_frames = frames.len();
                                    frames.rotate_left(consumed);
                                    frames.truncate(num_frames - consumed);
                                }
                            }
                            DataFormat::String => unreachable!("You encountered a bug. Please open an issue on Github."),
                        }

                        let message_num = messages_wrapped.len();

                        let messages: Vec<ListItem> = messages_wrapped
                            .iter()
                            .skip(message_num - (height + scroll_offset).min(message_num))
                            .take(height)
                            .map(|s| ListItem::new(vec![Spans::from(Span::raw(s))]))
                            .collect();

                        let messages = List::new(messages.as_slice())
                            .block(Block::default().borders(Borders::NONE));
                        f.render_widget(messages, chunks[1]);

                        if has_down_channel {
                            let input = Paragraph::new(Spans::from(vec![Span::raw(input.clone())]))
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
        }
    }

    /// Returns true if the application should exit.
    pub fn handle_event(&mut self) -> bool {
        match self.events.next().unwrap() {
            Event::Input(event) => match event.code {
                KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    clean_up_terminal();
                    let _ = self.terminal.show_cursor();

                    if let Some(path) = &self.history_path {
                        for (i, tab) in self.tabs.iter().enumerate() {
                            let extension = match tab.format() {
                                DataFormat::String => "txt",
                                DataFormat::BinaryLE => "dat",
                                DataFormat::Defmt => {
                                    panic!("You encountered a bug. Please open an issue on Github.")
                                }
                            };

                            let name = format!("{}_channel{}.{}", self.logname, i, extension);
                            let final_path = path.join(name);

                            match std::fs::File::create(final_path.clone()) {
                                Ok(mut file) => {
                                    match tab.format() {
                                        DataFormat::String => {
                                            for line in tab.messages() {
                                                match writeln!(file, "{}", line) {
                                                    Ok(_) => {}
                                                    Err(e) => {
                                                        eprintln!(
                                                            "\nError writing log channel {}: {}",
                                                            i, e
                                                        );
                                                        continue;
                                                    }
                                                }
                                            }
                                        }
                                        DataFormat::BinaryLE => match file.write(&tab.data()) {
                                            Ok(_) => {}
                                            Err(e) => {
                                                eprintln!(
                                                    "\nError writing log channel {}: {}",
                                                    i, e
                                                );
                                                continue;
                                            }
                                        },
                                        DataFormat::Defmt => {
                                            log::error!("Cannot write defmt output to disk.")
                                        }
                                    };
                                }
                                Err(e) => {
                                    eprintln!(
                                        "\nCould not create log file {:?}: {}",
                                        final_path.clone(),
                                        e
                                    );
                                }
                            }
                        }
                    }
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
        for channel in &mut self.tabs {
            channel.poll_rtt();
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
