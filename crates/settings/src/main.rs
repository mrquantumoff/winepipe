//! winepipe-settings: COSMIC settings UI for the WinePipe ASIO driver.
//!
//! ```text
//! winepipe-settings [--prefix DIR] [--wine PATH]
//! ```
//! Without arguments it lists the Wine prefixes it can find and preselects
//! `$WINEPREFIX`. The driver's ASIO control panel button launches it with
//! both arguments set for the calling application.

mod prefix;

use std::path::PathBuf;

use cosmic::app::{Core, Task};
use cosmic::iced::{Alignment, Length, Size};
use cosmic::widget::{self, settings};
use cosmic::{Application, ApplicationExt, Element};

use prefix::{Prefix, Settings, WineBinaries, BUFFER_SIZES, MAX_CHANNELS};

struct Args {
    prefix: Option<PathBuf>,
    wine: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args { prefix: None, wine: None };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--prefix" => a.prefix = it.next().map(PathBuf::from),
            "--wine" => a.wine = it.next(),
            "-h" | "--help" => {
                println!("usage: winepipe-settings [--prefix DIR] [--wine PATH]");
                std::process::exit(0);
            }
            other => {
                eprintln!("winepipe-settings: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

fn main() -> cosmic::iced::Result {
    let args = parse_args();
    let mut settings = cosmic::app::Settings::default().size(Size::new(720.0, 1000.0));
    if let Some(theme) = fallback_icon_theme() {
        settings = settings.default_icon_theme(theme);
    }
    cosmic::app::run::<App>(settings, args)
}

/// libcosmic draws its widgets' icons (spin buttons, dropdown arrows) from
/// the COSMIC icon theme. Outside COSMIC that theme usually isn't
/// installed. Adwaita's symbolic set uses the same names and shapes (other
/// themes draw e.g. `list-remove-symbolic` as a cross), so prefer it.
fn fallback_icon_theme() -> Option<String> {
    let dirs: Vec<PathBuf> = [dirs::data_dir().map(|d| d.join("icons")), dirs::home_dir().map(|h| h.join(".icons"))]
        .into_iter()
        .flatten()
        .chain(["/usr/local/share/icons", "/usr/share/icons"].map(PathBuf::from))
        .collect();
    let installed = |theme: &str| dirs.iter().any(|d| d.join(theme).join("index.theme").is_file());
    if installed("Cosmic") {
        None
    } else if installed("Adwaita") {
        Some("Adwaita".into())
    } else {
        Some("breeze".into())
    }
}

#[derive(Debug, Clone)]
enum Message {
    SelectPrefix(usize),
    Wine(String),
    Inputs(u32),
    Outputs(u32),
    BufferSize(usize),
    Fixed(bool),
    Autoconnect(bool),
    Format(usize),
    ClientName(String),
    Defaults,
    Revert,
    Save,
    /// (prefix, wine, what was written, result)
    Saved(PathBuf, String, Settings, Result<(), String>),
    Register,
    Registered(PathBuf, Result<String, String>),
}

#[derive(Clone, PartialEq)]
enum Status {
    None,
    Busy(String),
    Ok(String),
    Error(String),
}

struct App {
    core: Core,
    prefixes: Vec<Prefix>,
    labels: Vec<String>,
    selected: Option<usize>,
    wine: String,
    wine_override: Option<String>,
    binaries: WineBinaries,
    saved: Settings,
    edit: Settings,
    inputs_label: String,
    outputs_label: String,
    buffer_labels: Vec<String>,
    formats: Vec<String>,
    status: Status,
}

impl App {
    fn prefix(&self) -> Option<&Prefix> {
        self.selected.and_then(|i| self.prefixes.get(i))
    }

    fn busy(&self) -> bool {
        matches!(self.status, Status::Busy(_))
    }

    fn dirty(&self) -> bool {
        self.edit != self.saved
    }

    fn load_selected(&mut self) {
        let Some(p) = self.prefix().map(|p| p.path.clone()) else { return };
        self.saved = prefix::load_settings(&p);
        self.edit = self.saved.clone();
        self.wine = self.wine_override.clone().unwrap_or_else(|| prefix::guess_wine(&p, &self.binaries));
        self.refresh_labels();
        self.status = Status::None;
    }

    fn refresh_labels(&mut self) {
        self.inputs_label = self.edit.inputs.to_string();
        self.outputs_label = self.edit.outputs.to_string();
    }

    fn refresh_registration(&mut self, path: &std::path::Path) {
        if let Some(p) = self.prefixes.iter_mut().find(|p| p.path == path) {
            p.registration = prefix::registration(path);
            self.labels = self.prefixes.iter().map(Prefix::label).collect();
        }
    }

    fn prefix_section(&self) -> Element<'_, Message> {
        let mut sec = settings::section().title("Wine prefix");
        if self.prefixes.is_empty() {
            return sec.add(settings::item("No Wine prefixes found", widget::text::body(""))).into();
        }
        sec = sec
            .add(settings::item("Prefix", widget::dropdown(&self.labels, self.selected, Message::SelectPrefix)))
            .add(
                settings::item::builder("Wine binary")
                    .description("The wine used to run this prefix; used to save settings")
                    .control(widget::text_input("wine", &self.wine).on_input(Message::Wine).width(Length::Fixed(300.0))),
            );
        let reg = self.prefix().map(|p| p.registration.clone()).unwrap_or_default();
        let (state, action) = match (reg.x64, reg.x86) {
            (true, true) => ("Registered (64-bit and 32-bit)", "Re-register"),
            (true, false) => ("Registered (64-bit only)", "Register"),
            (false, true) => ("Registered (32-bit only)", "Register"),
            (false, false) => ("Not registered in this prefix", "Register"),
        };
        sec.add(
            settings::item::builder("Driver")
                .description(state)
                .control(widget::button::standard(action).on_press_maybe((!self.busy()).then_some(Message::Register))),
        )
        .into()
    }

    fn settings_sections(&self) -> Vec<Element<'_, Message>> {
        let e = &self.edit;
        let channels = settings::section()
            .title("Channels")
            .add(settings::item(
                "Inputs",
                widget::spin_button(&self.inputs_label, "Inputs", e.inputs, 1, 0, MAX_CHANNELS, Message::Inputs),
            ))
            .add(settings::item(
                "Outputs",
                widget::spin_button(&self.outputs_label, "Outputs", e.outputs, 1, 0, MAX_CHANNELS, Message::Outputs),
            ));

        let buffer_idx = BUFFER_SIZES.iter().position(|&b| b == e.buffer_size);
        let buffer = settings::section()
            .title("Buffer")
            .add(
                settings::item::builder("Preferred buffer size")
                    .description("Offered to the application. The size it picks becomes PipeWire's graph quantum while it runs.")
                    .control(widget::dropdown(&self.buffer_labels, buffer_idx, Message::BufferSize)),
            )
            .add(
                settings::item::builder("Fixed buffer size")
                    .description("Only allow the preferred size")
                    .toggler(e.fixed_buffer_size, Message::Fixed),
            );

        let pipewire = settings::section()
            .title("PipeWire")
            .add(
                settings::item::builder("Connect automatically")
                    .description("Link outputs to the default output device and inputs from the default input device on start")
                    .toggler(e.autoconnect, Message::Autoconnect),
            )
            .add(
                settings::item::builder("Node name")
                    .description("How the driver appears in the PipeWire graph")
                    .control(
                        widget::text_input("WinePipe", &e.client_name)
                            .on_input(Message::ClientName)
                            .width(Length::Fixed(240.0)),
                    ),
            );

        let format = settings::section().title("Format").add(
            settings::item::builder("Sample format")
                .description("Float32 suits nearly every host; Int32 is for old ones that need it")
                .control(widget::dropdown(&self.formats, Some(e.int32 as usize), Message::Format)),
        );

        vec![channels.into(), buffer.into(), pipewire.into(), format.into()]
    }

    fn footer_bar(&self) -> Element<'_, Message> {
        let space = cosmic::theme::spacing();
        let status: Element<'_, Message> = match &self.status {
            Status::None if self.dirty() => widget::text::caption("Unsaved changes").into(),
            Status::None => {
                widget::text::caption("Changes apply the next time an application loads the driver.").into()
            }
            Status::Busy(s) => widget::text::caption(s.clone()).into(),
            Status::Ok(s) => widget::text::caption(s.clone()).into(),
            Status::Error(s) => widget::text::caption(s.clone())
                .class(cosmic::theme::Text::Color(cosmic::iced::Color::from_rgb(0.9, 0.3, 0.3)))
                .into(),
        };
        let can_edit = self.prefix().is_some() && !self.busy();
        widget::row::with_children(vec![
            widget::container(status).width(Length::Fill).into(),
            widget::button::standard("Defaults")
                .on_press_maybe((can_edit && self.edit != Settings::default()).then_some(Message::Defaults))
                .into(),
            widget::button::standard("Revert").on_press_maybe((can_edit && self.dirty()).then_some(Message::Revert)).into(),
            widget::button::suggested("Save").on_press_maybe((can_edit && self.dirty()).then_some(Message::Save)).into(),
        ])
        .spacing(space.space_xs)
        .align_y(Alignment::Center)
        .padding([space.space_xs, space.space_m])
        .into()
    }
}

impl Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = Args;
    type Message = Message;

    const APP_ID: &'static str = "io.github.mrquantumoff.WinePipe";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, args: Args) -> (Self, Task<Message>) {
        core.window.show_maximize = false;
        let mut extra: Vec<PathBuf> = args.prefix.iter().cloned().collect();
        if let Some(p) = std::env::var_os("WINEPREFIX") {
            extra.push(PathBuf::from(p));
        }
        let prefixes = prefix::discover(&extra);
        let mut app = App {
            core,
            labels: prefixes.iter().map(Prefix::label).collect(),
            selected: (!prefixes.is_empty()).then_some(0),
            prefixes,
            wine: "wine".into(),
            wine_override: args.wine,
            binaries: WineBinaries::load(),
            saved: Settings::default(),
            edit: Settings::default(),
            inputs_label: String::new(),
            outputs_label: String::new(),
            buffer_labels: BUFFER_SIZES
                .iter()
                .map(|&b| format!("{b} frames  ({:.1} ms at 48 kHz)", b as f64 * 1000.0 / 48000.0))
                .collect(),
            formats: vec!["Float32 (recommended)".into(), "Int32".into()],
            status: Status::None,
        };
        app.load_selected();
        app.set_header_title("WinePipe".into());
        let title = match app.core.main_window_id() {
            Some(id) => app.set_window_title("WinePipe Settings".into(), id),
            None => Task::none(),
        };
        (app, title)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SelectPrefix(i) => {
                // Results of a running save/registration belong to the
                // prefix they were started for; keep it selected meanwhile.
                if i < self.prefixes.len() && !self.busy() {
                    self.selected = Some(i);
                    // an explicit --wine only applies to the prefix it came with
                    self.wine_override = None;
                    self.load_selected();
                }
            }
            Message::Wine(w) => self.wine = w,
            Message::Inputs(v) => {
                self.edit.inputs = v.min(MAX_CHANNELS);
                self.refresh_labels();
            }
            Message::Outputs(v) => {
                self.edit.outputs = v.min(MAX_CHANNELS);
                self.refresh_labels();
            }
            Message::BufferSize(i) => {
                if let Some(&b) = BUFFER_SIZES.get(i) {
                    self.edit.buffer_size = b;
                }
            }
            Message::Fixed(v) => self.edit.fixed_buffer_size = v,
            Message::Autoconnect(v) => self.edit.autoconnect = v,
            Message::Format(i) => self.edit.int32 = i == 1,
            Message::ClientName(s) => self.edit.client_name = s.chars().filter(|c| !c.is_control()).take(63).collect(),
            Message::Defaults => {
                self.edit = Settings::default();
                self.refresh_labels();
            }
            Message::Revert => {
                self.edit = self.saved.clone();
                self.refresh_labels();
            }
            Message::Save => {
                let Some(p) = self.prefix().map(|p| p.path.clone()) else { return Task::none() };
                let mut s = self.edit.clone();
                if s.client_name.trim().is_empty() {
                    s.client_name = Settings::default().client_name;
                }
                self.edit = s.clone();
                let wine = prefix::expand_home(self.wine.trim());
                self.status = Status::Busy("Saving…".into());
                return cosmic::task::future(async move {
                    let (p2, w2, s2) = (p.clone(), wine.clone(), s.clone());
                    let r = tokio::task::spawn_blocking(move || prefix::save_settings(&w2, &p2, &s2))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                    Message::Saved(p, wine, s, r)
                });
            }
            Message::Saved(path, wine, written, r) => match r {
                Ok(()) => {
                    if self.prefix().is_some_and(|p| p.path == path) {
                        self.saved = written;
                    }
                    self.binaries.set(&path, &wine);
                    self.status = Status::Ok("Saved. Restart the application (or reopen its audio device) to apply.".into());
                }
                Err(e) => self.status = Status::Error(e),
            },
            Message::Register => {
                let Some(p) = self.prefix().map(|p| p.path.clone()) else { return Task::none() };
                let wine = prefix::expand_home(self.wine.trim());
                self.status = Status::Busy("Registering the driver…".into());
                return cosmic::task::future(async move {
                    let p2 = p.clone();
                    let r = tokio::task::spawn_blocking(move || prefix::register(&wine, &p2))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                    Message::Registered(p, r)
                });
            }
            Message::Registered(path, r) => {
                self.refresh_registration(&path);
                self.status = match r {
                    Ok(_) => Status::Ok("Driver registered.".into()),
                    Err(e) => Status::Error(format!("Registration failed: {}", e.lines().last().unwrap_or(""))),
                };
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let space = cosmic::theme::spacing();
        let mut children = vec![self.prefix_section()];
        if self.prefix().is_some() {
            children.extend(self.settings_sections());
        }
        let page = settings::view_column(children).padding([space.space_s, space.space_m]);
        let body = widget::scrollable(
            widget::container(widget::container(page).max_width(760)).width(Length::Fill).center_x(Length::Fill),
        )
        .height(Length::Fill);
        widget::column::with_children(vec![body.into(), widget::divider::horizontal::default().into(), self.footer_bar()])
            .height(Length::Fill)
            .into()
    }
}
