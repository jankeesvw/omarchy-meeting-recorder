//! The recorder window. It has one page per phase: recording (which can shrink
//! to a compact strip with only the waves and the clock), transcribing (the
//! animation, edge to edge) and done (the transcript and what to do with it).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::agent::{self, Agent};
use crate::animation::TranscribeAnimation;
use crate::audio::{HISTORY, Source, to_meter};
use crate::chapters::{self, Chapter};
use crate::export::{self, Format, export_audio, export_tracks};
use crate::ipc::{self, SharedStatus, Status};
use crate::meeting::{self, Manifest};
use crate::player::Player;
use crate::notes::Notes;
use crate::transcribe::{self, Abort, CANCELLED, Event, LANGUAGES};
use crate::{APP_ID, APP_NAME, settings};

const MIC_COLOR: (f64, f64, f64) = (0.21, 0.52, 0.89);
const SYSTEM_COLOR: (f64, f64, f64) = (0.90, 0.38, 0.0);
const FULL_SIZE: (i32, i32) = (480, 700);
const COMPACT_SIZE: (i32, i32) = (300, 84);
const DONE_SIZE: (i32, i32) = (1100, 760);
/// The recording page with the notes next to it: wider, just as high.
const NOTES_SIZE: (i32, i32) = (1100, FULL_SIZE.1);
/// The done page with the meeting's notes next to the transcript.
const DONE_NOTES_SIZE: (i32, i32) = (1560, DONE_SIZE.1);
/// How wide the notes are next to the transcript.
const DONE_NOTES_WIDTH: i32 = 520;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Idle,
    Recording,
    /// Writing the audio files.
    Stopping,
    Transcribing,
    Done,
}

impl State {
    fn key(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Recording => "recording",
            State::Stopping => "stopping",
            State::Transcribing => "transcribing",
            State::Done => "done",
        }
    }
}

/// Whether the machine should stay awake in this state: the stretch after
/// Stop, when the audio is finished and transcribed without anyone around.
fn keeps_awake(state: State) -> bool {
    matches!(state, State::Stopping | State::Transcribing)
}

/// Where a transcription reads its two tracks from.
enum Tracks {
    /// The staging files of a recording that just stopped.
    Raw(PathBuf),
    /// The `.tracks` directory kept in a meeting folder.
    Kept(PathBuf),
    /// One imported file, with the number of speakers asked for.
    Single(PathBuf, Option<usize>),
}

/// The imported source audio, kept for transcribing again.
fn source_track(meeting_dir: &std::path::Path) -> PathBuf {
    meeting_dir.join(export::TRACKS_DIR).join("source.ogg")
}

/// Choices in the import dialog: automatic, then a fixed number.
const SPEAKER_CHOICES: [&str; 7] = ["Automatic", "1", "2", "3", "4", "5", "6"];

/// What all windows share: the audio sources, the socket for the bar widget
/// and the keybindings, and the list of open windows. Every window can show
/// its own meeting; only one records at a time.
struct Hub {
    mic: Source,
    system: Source,
    statuses: ipc::Statuses,
    /// The open windows, oldest first. Held here: the window's own callbacks
    /// only hold weak references, so this is what keeps a window's recorder
    /// alive until the window is gone.
    windows: RefCell<Vec<Rc<Recorder>>>,
}

impl Hub {
    fn new(app: &adw::Application) -> Rc<Self> {
        let hub = Rc::new(Hub {
            mic: Source::spawn("@DEFAULT_SOURCE@"),
            system: Source::spawn("@DEFAULT_MONITOR@"),
            statuses: ipc::Statuses::default(),
            windows: RefCell::default(),
        });
        let (commands_tx, commands_rx) = async_channel::unbounded();
        ipc::serve(
            hub.statuses.clone(),
            hub.mic.clone(),
            hub.system.clone(),
            commands_tx,
        );
        let weak = Rc::downgrade(&hub);
        let app_for_commands = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(command) = commands_rx.recv().await {
                let Some(hub) = weak.upgrade() else {
                    break;
                };
                hub.command(&app_for_commands, command.0, &command.1);
            }
        });

        let new_window = gio::SimpleAction::new("new-window", None);
        let weak = Rc::downgrade(&hub);
        let app_for_new = app.clone();
        new_window.connect_activate(move |_, _| {
            if let Some(hub) = weak.upgrade() {
                hub.add_window(&app_for_new).window.present();
            }
        });
        app.add_action(&new_window);

        // Quitting closes every window; one in the middle of a meeting asks first.
        let quit = gio::SimpleAction::new("quit", None);
        let weak = Rc::downgrade(&hub);
        quit.connect_activate(move |_, _| {
            if let Some(hub) = weak.upgrade() {
                for recorder in hub.recorders() {
                    recorder.window.close();
                }
            }
        });
        app.add_action(&quit);
        hub
    }

    fn add_window(self: &Rc<Self>, app: &adw::Application) -> Rc<Recorder> {
        let recorder = Recorder::new(app, self);
        self.windows.borrow_mut().push(recorder.clone());
        recorder.learn_hypr_address();
        let weak = Rc::downgrade(self);
        let status = recorder.shared.clone();
        recorder.window.connect_destroy(move |_| {
            if let Some(hub) = weak.upgrade() {
                hub.statuses
                    .lock()
                    .unwrap()
                    .retain(|s| !Arc::ptr_eq(s, &status));
                hub.windows
                    .borrow_mut()
                    .retain(|r| !Arc::ptr_eq(&r.shared, &status));
            }
        });
        recorder
    }

    /// The open windows, oldest first.
    fn recorders(&self) -> Vec<Rc<Recorder>> {
        self.windows.borrow().clone()
    }

    /// The window that is recording, if any.
    fn recording(&self) -> Option<Rc<Recorder>> {
        self.recorders()
            .into_iter()
            .find(|r| matches!(r.state.get(), State::Recording | State::Stopping))
    }

    /// Where the bar widget and the keybindings act: the window recording,
    /// else the one transcribing, else the one in front, else a new one.
    fn target(self: &Rc<Self>, app: &adw::Application) -> Rc<Recorder> {
        let recorders = self.recorders();
        let active = app.active_window();
        self.recording()
            .or_else(|| {
                recorders
                    .iter()
                    .find(|r| r.state.get() == State::Transcribing)
                    .cloned()
            })
            .or_else(|| {
                recorders
                    .iter()
                    .find(|r| active.as_ref() == Some(r.window.upcast_ref()))
                    .cloned()
            })
            .or_else(|| recorders.last().cloned())
            .unwrap_or_else(|| self.add_window(app))
    }

    /// A window free to show a meeting: the one in front when it is not busy,
    /// else another free one, else a new one.
    fn free_window(self: &Rc<Self>, app: &adw::Application) -> Rc<Recorder> {
        let free = |r: &Rc<Recorder>| matches!(r.state.get(), State::Idle | State::Done);
        let recorders = self.recorders();
        let active = app.active_window();
        recorders
            .iter()
            .find(|r| active.as_ref() == Some(r.window.upcast_ref()) && free(r))
            .or_else(|| recorders.iter().rev().find(|r| free(r)))
            .cloned()
            .unwrap_or_else(|| self.add_window(app))
    }

    /// A command from the socket: `omarchy-meeting-recorder start` and friends.
    /// `title` is what came with it: the name for `start`, the notes file for
    /// `notes`, "" without one.
    fn command(self: &Rc<Self>, app: &adw::Application, command: &str, title: &str) {
        if command == "new-window" {
            self.add_window(app).window.present();
            return;
        }
        if command == "notes" || command == "name" {
            let r = self.recording().unwrap_or_else(|| self.free_window(app));
            // Notes are for reading while recording: a window showing a meeting
            // goes back to the recording page first.
            if r.state.get() == State::Done {
                r.ready();
            }
            if command == "notes" {
                r.open_notes(std::path::Path::new(title));
            } else if r.state.get() == State::Idle {
                set_name(&r.title_row, title);
            }
            r.window.present();
            return;
        }
        let r = match command {
            // Starting takes a window that is free, so a meeting being
            // transcribed stays on screen.
            "start" => self.recording().unwrap_or_else(|| self.free_window(app)),
            _ => self.target(app),
        };
        match command {
            // A name typed on the ready page is kept unless the command
            // brings one; from the done page it starts fresh.
            "start" if matches!(r.state.get(), State::Idle | State::Done) => {
                if r.state.get() == State::Done {
                    r.ready();
                }
                if !title.is_empty() {
                    set_name(&r.title_row, title);
                }
                r.start();
            }
            "stop" => r.stop(),
            "compact" => r.set_compact(!r.compact.get()),
            "pause" => r.toggle_pause(),
            _ => {}
        }
    }
}

/// Runs the app. `open` is a `.meeting-recorder` file or a meeting folder to show
/// instead of starting a new recording.
pub fn run(open: Option<&str>) -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|app| {
        load_css();
        crate::theme::follow(|| {
            for window in gtk::Window::list_toplevels() {
                redraw(&window);
            }
        });
        app.set_accels_for_action("win.compact", &["<Control>m"]);
        app.set_accels_for_action("win.find", &["<Control>f"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("app.new-window", &["<Control>n"]);
    });
    // Made on the first activation, once GTK is up: it starts the audio.
    let hub: Rc<RefCell<Option<Rc<Hub>>>> = Rc::default();
    let get_hub = {
        let hub = hub.clone();
        move |app: &adw::Application| {
            let existing = hub.borrow().clone();
            existing.unwrap_or_else(|| {
                let created = Hub::new(app);
                *hub.borrow_mut() = Some(created.clone());
                created
            })
        }
    };
    let hub_for_open = get_hub.clone();
    app.connect_open(move |app, files, _| {
        let hub = hub_for_open(app);
        // A window in the middle of a meeting keeps it; the file gets its own.
        let recorder = hub.free_window(app);
        if let Some(path) = files.first().and_then(|f| f.path()) {
            recorder.open_meeting(&path);
        }
        recorder.window.present();
    });
    app.connect_activate(move |app| {
        let hub = get_hub(app);
        let first = hub.recorders().is_empty();
        let recorder = hub.target(app);
        // Opened again while it was finishing in the background: stay open.
        recorder.quit_when_done.set(false);
        if first && recorder.state.get() == State::Idle {
            let recorder = recorder.clone();
            glib::idle_add_local_once(move || recorder.offer_recovery());
        }
        recorder.window.present();
    });
    match open {
        Some(path) => app.run_with_args(&[APP_NAME, path]),
        None => app.run_with_args::<&str>(&[]),
    }
}

struct Recorder {
    window: adw::ApplicationWindow,
    view: adw::ToolbarView,
    toasts: adw::ToastOverlay,
    layout: gtk::Stack,
    compact_action: gio::SimpleAction,
    compact_button: gtk::Button,
    title_row: adw::EntryRow,
    format_row: adw::ComboRow,
    language_row: adw::ComboRow,
    /// Whether to record the camera next to the audio.
    webcam_row: adw::SwitchRow,
    /// The camera the toggle records, when there is one.
    webcam_camera: Option<(PathBuf, String)>,
    webcam: crate::webcam::Webcam,
    /// Waiting for the camera's first frame before the audio starts or resumes.
    starting: Cell<bool>,
    /// The desktop's cookie for keeping the machine awake while the meeting
    /// is processed; 0 while it may idle.
    awake: Cell<u32>,
    /// What the camera sees, in the webcam row, before and while recording.
    webcam_preview: gtk::Picture,
    animation: TranscribeAnimation,
    meters: [gtk::DrawingArea; 2],
    compact_meters: [gtk::DrawingArea; 2],
    dot: gtk::Label,
    timer: gtk::Label,
    compact_dot: gtk::Label,
    compact_timer: gtk::Label,
    status_label: gtk::Label,
    button: gtk::Button,
    copy_button: gtk::Button,
    /// Your own scripts from config.toml, hidden when there are none.
    actions_button: gtk::MenuButton,
    /// In its place when there are none: opens the page on how to add them.
    add_actions_button: gtk::Button,
    again_button: gtk::Button,
    done_title_row: adw::EntryRow,
    done_group: adw::PreferencesGroup,
    /// One name row per speaker, rebuilt for every meeting.
    speaker_rows: RefCell<Vec<adw::EntryRow>>,
    again_language_row: adw::ComboRow,
    done_icon: gtk::Image,
    done_heading: gtk::Label,
    done_meta: gtk::Label,
    transcript_list: gtk::ListBox,
    transcript_scroll: gtk::ScrolledWindow,
    find_button: gtk::Button,
    find_bar: gtk::Box,
    find_entry: gtk::SearchEntry,
    find_count: gtk::Label,
    find_prev: gtk::Button,
    find_next: gtk::Button,
    /// Text of each transcript row, in list order, for Ctrl+F.
    search_rows: RefCell<Vec<String>>,
    /// Row indexes of the current query.
    find_hits: RefCell<Vec<i32>>,
    /// The query those hits were computed for. Enter before the search
    /// delay elapses still uses what is typed now.
    find_query: RefCell<String>,
    /// Which of `find_hits` is the one on screen.
    find_at: Cell<usize>,
    player: Player,
    /// Start of each transcript line in ms, by buffer line, for seeking and highlighting.
    segments: RefCell<Vec<(i32, i64)>>,
    /// The paragraphs on screen as (start ms, speaker, text), what the agent reads.
    lines: RefCell<Vec<(i64, String, String)>>,
    chapters_group: adw::PreferencesGroup,
    chapters_list: gtk::ListBox,
    /// Start of each row in the chapters list, in ms.
    chapter_starts: RefCell<Vec<i64>>,
    /// The size the window was last fitted to, per page.
    fitted: Cell<(i32, i32)>,
    chapters_button: gtk::Button,
    chapters_spinner: adw::Spinner,
    /// Looked up once: the default agent, if any.
    agent: std::cell::OnceCell<Option<Agent>>,
    generating: Cell<bool>,
    /// The meeting whose automatic actions are still to run: set when its
    /// transcript is done, taken once the chapters are too.
    auto_actions_due: RefCell<Option<PathBuf>>,
    current_line: Cell<i32>,

    mic: Source,
    system: Source,
    shared: SharedStatus,
    hub: std::rc::Weak<Hub>,
    /// This window's address in Hyprland, learnt once it is on screen, so a
    /// resize reaches this window and not another one of the app.
    hypr_address: RefCell<Option<String>>,

    state: Cell<State>,
    compact: Cell<bool>,
    full_size: Cell<(i32, i32)>,
    started_at: Cell<i64>,
    paused: Cell<bool>,
    frozen: [Frozen; 2],
    /// When the animation came on screen, to keep it there long enough.
    animation_since: Cell<Option<std::time::Instant>>,
    paused_secs: Cell<i64>,
    pause_began: Cell<i64>,
    pause_button: gtk::Button,
    import_button: gtk::Button,
    model_banner: adw::Banner,
    model_downloading: Cell<bool>,
    drop_hint: gtk::Label,
    staging: RefCell<Option<PathBuf>>,
    result_dir: RefCell<Option<PathBuf>>,
    abort: RefCell<Option<Abort>>,
    quit_when_done: Cell<bool>,
    /// Set while a meeting's own settings are shown, so they are not saved as defaults.
    loading: Cell<bool>,
    /// The settings of the meeting on the done page.
    manifest: RefCell<Option<Manifest>>,
    /// Notes to read from, next to the recording page when they are open.
    notes: Rc<Notes>,
    /// The [a] in the corner of the recording page that opens and closes it.
    notes_button: gtk::Button,
    /// The notes file read from while recording, to come back to for the next take.
    live_notes: RefCell<Option<PathBuf>>,
    /// The notes on screen are the copy in the meeting on the done page.
    showing_meeting_notes: Cell<bool>,
}

impl Recorder {
    fn new(app: &adw::Application, hub: &Rc<Hub>) -> Rc<Self> {
        let mic = hub.mic.clone();
        let system = hub.system.clone();
        let shared: SharedStatus = Arc::new(Mutex::new(Status {
            state: "idle",
            ..Default::default()
        }));
        hub.statuses.lock().unwrap().push(shared.clone());

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Meeting Recorder")
            .default_width(FULL_SIZE.0)
            .default_height(FULL_SIZE.1)
            .build();
        let view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let compact_button = gtk::Button::builder()
            .icon_name("view-restore-symbolic")
            .tooltip_text("Minimize (Ctrl+M)")
            .action_name("win.compact")
            .build();
        header.pack_start(&compact_button);
        let find_button = gtk::Button::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Find in the transcript (Ctrl+F)")
            .action_name("win.find")
            .visible(false)
            .build();
        header.pack_end(&find_button);
        view.add_top_bar(&header);
        // Under the header bar, full width, while the speech model still has
        // to be downloaded.
        let model_banner = adw::Banner::builder().revealed(false).build();
        view.add_top_bar(&model_banner);
        let toasts = adw::ToastOverlay::new();
        view.set_content(Some(&toasts));
        view.set_hexpand(true);
        let notes = Notes::new();
        let panes = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        panes.append(&view);
        panes.append(&notes.widget);
        window.set_content(Some(&panes));

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let group = adw::PreferencesGroup::new();
        let title_row = adw::EntryRow::builder()
            .title("Meeting name")
            .show_apply_button(true)
            .build();
        let format_labels: Vec<&str> = Format::ALL.iter().map(|f| f.label()).collect();
        let format_row = adw::ComboRow::builder()
            .title("Audio file")
            .subtitle("Can be changed during the call")
            .model(&gtk::StringList::new(&format_labels))
            .build();
        let saved = settings::load_format();
        format_row.set_selected(Format::ALL.iter().position(|f| *f == saved).unwrap_or(0) as u32);
        let language_labels: Vec<&str> =
            LANGUAGES.iter().map(|(_, label)| label.as_str()).collect();
        let language_row = adw::ComboRow::builder()
            .title("Language")
            .subtitle("Used for the transcript after the call")
            .model(&gtk::StringList::new(&language_labels))
            .build();
        let saved = settings::load_language();
        language_row.set_selected(
            LANGUAGES
                .iter()
                .position(|(code, _)| *code == saved)
                .unwrap_or(0) as u32,
        );
        let webcam_camera = crate::webcam::pick(&crate::webcam::devices(), &settings::load_webcam());
        let webcam_row = adw::SwitchRow::builder()
            .title("Webcam")
            .subtitle(match &webcam_camera {
                Some((_, name)) => format!("{name}, recorded as webcam.mp4"),
                None => "No camera found".to_owned(),
            })
            .active(webcam_camera.is_some() && settings::load_webcam_on())
            .sensitive(webcam_camera.is_some())
            .build();
        let webcam_preview = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .width_request(96)
            .height_request(54)
            .valign(gtk::Align::Center)
            .css_classes(["webcam-preview"])
            .visible(false)
            .build();
        webcam_preview.set_overflow(gtk::Overflow::Hidden);
        webcam_row.add_prefix(&webcam_preview);
        let (frames_tx, frames_rx) = async_channel::bounded::<Vec<u8>>(2);
        let picture = webcam_preview.clone();
        glib::spawn_future_local(async move {
            while let Ok(frame) = frames_rx.recv().await {
                if let Ok(texture) = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(frame)) {
                    picture.set_paintable(Some(&texture));
                }
            }
        });
        group.add(&title_row);
        group.add(&format_row);
        group.add(&language_row);
        group.add(&webcam_row);
        content.append(&group);

        let frozen: [Frozen; 2] = Default::default();
        let meters = [
            meter(&mic, ("blue", MIC_COLOR), 56, &frozen[0]),
            meter(&system, ("orange", SYSTEM_COLOR), 56, &frozen[1]),
        ];
        let meters_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        meters_box.append(&meter_block("You (microphone)", &meters[0]));
        meters_box.append(&meter_block("Computer audio", &meters[1]));

        content.append(&meters_box);

        let status_row = gtk::Box::builder()
            .spacing(10)
            .halign(gtk::Align::Center)
            .build();
        let dot = gtk::Label::builder()
            .label("●")
            .css_classes(["error", "title-2"])
            .build();
        let timer = gtk::Label::builder()
            .label("00:00")
            .css_classes(["title-1", "numeric"])
            .build();
        status_row.append(&dot);
        status_row.append(&timer);
        content.append(&status_row);

        let status_label = gtk::Label::builder()
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        content.append(&status_label);

        let button = gtk::Button::builder().css_classes(["pill"]).build();
        let pause_button = gtk::Button::builder()
            .label("Pause")
            .css_classes(["pill"])
            .visible(false)
            .build();
        let buttons = gtk::Box::builder()
            .spacing(10)
            .halign(gtk::Align::Center)
            .build();
        buttons.append(&pause_button);
        buttons.append(&button);
        content.append(&buttons);
        let import_button = gtk::Button::builder()
            .label("Import an audio file, or drop one here")
            .halign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        content.append(&import_button);

        // Transcribing: the animation fills the whole window.
        let animation = TranscribeAnimation::new();

        // Done: a wide page. Left the meeting and what to do with it, right the
        // player and the transcript.
        let left = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .build();
        let done_header = gtk::Box::builder().spacing(12).build();
        let done_icon = gtk::Image::builder()
            .icon_name("object-select-symbolic")
            .pixel_size(20)
            .css_classes(["done-icon"])
            .valign(gtk::Align::Center)
            .build();
        let done_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .valign(gtk::Align::Center)
            .build();
        let done_heading = gtk::Label::builder()
            .label("Meeting saved")
            .xalign(0.0)
            .css_classes(["title-3"])
            .build();
        let done_meta = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        done_text.append(&done_heading);
        done_text.append(&done_meta);
        done_header.append(&done_icon);
        done_header.append(&done_text);
        left.append(&done_header);

        let done_group = adw::PreferencesGroup::new();
        let done_title_row = adw::EntryRow::builder()
            .title("Meeting name")
            .show_apply_button(true)
            .build();
        done_group.add(&done_title_row);
        left.append(&done_group);

        // Chapters: a list to jump through, with the agent's button in the header.
        let chapters_spinner = adw::Spinner::builder().visible(false).build();
        let chapters_button = gtk::Button::builder()
            .label("Generate")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let chapters_suffix = gtk::Box::builder().spacing(6).build();
        chapters_suffix.append(&chapters_spinner);
        chapters_suffix.append(&chapters_button);
        let chapters_group = adw::PreferencesGroup::builder()
            .title("Chapters")
            .header_suffix(&chapters_suffix)
            .vexpand(true)
            .build();
        let chapters_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::Single)
            .valign(gtk::Align::Start)
            .build();
        chapters_list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No chapters yet")
                .css_classes(["dim-label"])
                .margin_top(14)
                .margin_bottom(14)
                .build(),
        ));
        let chapters_scroll = gtk::ScrolledWindow::builder()
            .child(&chapters_list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        chapters_group.add(&chapters_scroll);
        left.append(&chapters_group);

        let copy_button = gtk::Button::builder()
            .label("Copy transcript")
            .css_classes(["pill", "suggested-action"])
            .build();
        left.append(&copy_button);

        let actions_button = gtk::MenuButton::builder()
            .label("Actions")
            .always_show_arrow(true)
            .css_classes(["pill"])
            .visible(false)
            .build();
        left.append(&actions_button);
        let add_actions_button = gtk::Button::builder()
            .label("Add actions…")
            .tooltip_text("How to run your own scripts on a meeting")
            .css_classes(["pill"])
            .visible(false)
            .build();
        left.append(&add_actions_button);

        let actions = gtk::Box::builder().spacing(8).homogeneous(true).build();
        let open_button = gtk::Button::builder()
            .label("Open folder")
            .css_classes(["pill"])
            .build();
        let new_button = gtk::Button::builder()
            .label("New recording")
            .css_classes(["pill"])
            .build();
        actions.append(&open_button);
        actions.append(&new_button);
        left.append(&actions);

        let again_group = adw::PreferencesGroup::new();
        let again_language_row = adw::ComboRow::builder()
            .title("Language")
            .subtitle("Transcribe again")
            .model(&gtk::StringList::new(&language_labels))
            .selected(language_row.selected())
            .build();
        let again_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Transcribe again")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        again_language_row.add_suffix(&again_button);
        again_group.add(&again_language_row);
        left.append(&again_group);

        let right = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .hexpand(true)
            .build();
        let player = Player::new();
        right.append(player.widget());
        // One row per paragraph: time and speaker in fixed columns, the text
        // wrapping in its own column so every line starts at the same place.
        let transcript_list = gtk::ListBox::builder()
            .css_classes(["transcript"])
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .build();
        let transcript_scroll = gtk::ScrolledWindow::builder()
            .child(&transcript_list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .css_classes(["card"])
            .build();
        transcript_scroll.set_overflow(gtk::Overflow::Hidden);
        // Hidden until Ctrl+F on the done page. Sits above the transcript so
        // the lines it marks stay visible underneath.
        let find_entry = gtk::SearchEntry::builder()
            .placeholder_text("Find in the transcript")
            .activates_default(false)
            .hexpand(true)
            .build();
        let find_count = gtk::Label::builder()
            .width_chars(11)
            .xalign(1.0)
            .css_classes(["dim-label", "numeric"])
            .build();
        let find_nav = |icon: &str, tooltip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .sensitive(false)
                .css_classes(["flat", "circular"])
                .build()
        };
        let find_prev = find_nav("go-up-symbolic", "Previous match (Shift+Enter)");
        let find_next = find_nav("go-down-symbolic", "Next match (Enter)");
        let find_close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close (Escape)")
            .css_classes(["flat", "circular"])
            .build();
        let find_bar = gtk::Box::builder().spacing(6).visible(false).build();
        find_bar.append(&find_entry);
        find_bar.append(&find_count);
        find_bar.append(&find_prev);
        find_bar.append(&find_next);
        find_bar.append(&find_close);
        right.append(&find_bar);
        // The webcam, when the meeting has one, floats in the corner of the transcript.
        let transcript_layers = gtk::Overlay::builder()
            .child(&transcript_scroll)
            .vexpand(true)
            .build();
        right.append(&transcript_layers);
        let pip = player.video_widget();
        pip.set_halign(gtk::Align::End);
        pip.set_valign(gtk::Align::End);
        pip.set_margin_end(16);
        pip.set_margin_bottom(16);
        pip.add_css_class("webcam-pip");
        transcript_layers.add_overlay(pip);

        let done = gtk::Box::builder()
            .spacing(24)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        // A fixed, narrow column on the left; the transcript takes the rest.
        let left_column = adw::Clamp::builder()
            .child(&left)
            .maximum_size(320)
            .tightening_threshold(320)
            .width_request(320)
            .hexpand(false)
            .build();
        done.append(&left_column);
        done.append(&right);

        // Compact mode: only the two waves and the clock.
        let compact_meters = [
            meter(&mic, ("blue", MIC_COLOR), 26, &frozen[0]),
            meter(&system, ("orange", SYSTEM_COLOR), 26, &frozen[1]),
        ];
        let compact_dot = gtk::Label::builder()
            .label("●")
            .css_classes(["error"])
            .build();
        let compact_timer = gtk::Label::builder()
            .label("00:00")
            .css_classes(["numeric", "heading"])
            .build();
        let compact_clock = gtk::Box::builder()
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        compact_clock.append(&compact_dot);
        compact_clock.append(&compact_timer);
        let compact_waves = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        compact_waves.append(&compact_meters[0]);
        compact_waves.append(&compact_meters[1]);
        let compact_strip = gtk::Box::builder()
            .spacing(12)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(14)
            .margin_end(8)
            .build();
        let expand_button = gtk::Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text("Expand (Ctrl+M)")
            .action_name("win.compact")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        compact_strip.append(&compact_clock);
        compact_strip.append(&compact_waves);
        compact_strip.append(&expand_button);
        // The whole strip drags the window around, like a title bar.
        let compact = gtk::WindowHandle::builder()
            .child(&compact_strip)
            .tooltip_text("Drag to move, Ctrl+M to expand")
            .build();

        // Not homogeneous, so the window can shrink to the compact page.
        let layout = gtk::Stack::builder()
            .hhomogeneous(false)
            .vhomogeneous(false)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(250)
            .build();
        layout.add_named(&content, Some("record"));
        layout.add_named(animation.widget(), Some("transcribing"));
        layout.add_named(&done, Some("done"));
        layout.add_named(&compact, Some("compact"));
        let drop_hint = gtk::Label::builder()
            .label("Drop to import")
            .css_classes(["drop-hint", "title-2"])
            .visible(false)
            .can_target(false)
            .build();
        let overlay = gtk::Overlay::builder().child(&layout).build();
        overlay.add_overlay(&drop_hint);
        let notes_button = gtk::Button::builder()
            .label("a")
            .tooltip_text("Meeting notes")
            .css_classes(["flat", "notes-toggle"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_end(12)
            .margin_bottom(12)
            .build();
        overlay.add_overlay(&notes_button);
        toasts.set_child(Some(&overlay));

        let compact_action = gio::SimpleAction::new("compact", None);
        window.add_action(&compact_action);
        let find_action = gio::SimpleAction::new("find", None);
        window.add_action(&find_action);
        let run_action = gio::SimpleAction::new("run-action", Some(glib::VariantTy::INT32));
        window.add_action(&run_action);

        let recorder = Rc::new(Recorder {
            window,
            view,
            toasts,
            layout,
            compact_action,
            compact_button,
            title_row,
            format_row,
            language_row,
            webcam_row,
            webcam_camera,
            webcam: crate::webcam::Webcam::new(frames_tx),
            starting: Cell::new(false),
            awake: Cell::new(0),
            webcam_preview,
            animation,
            meters,
            compact_meters,
            dot,
            timer,
            compact_dot,
            compact_timer,
            status_label,
            button,
            copy_button,
            actions_button,
            add_actions_button,
            again_button,
            done_title_row,
            done_group,
            speaker_rows: RefCell::default(),
            again_language_row,
            done_icon,
            done_heading,
            done_meta,
            transcript_list,
            transcript_scroll,
            find_button,
            find_bar,
            find_entry,
            find_count,
            find_prev,
            find_next,
            search_rows: RefCell::default(),
            find_hits: RefCell::default(),
            find_query: RefCell::default(),
            find_at: Cell::new(0),
            player,
            segments: RefCell::default(),
            lines: RefCell::default(),
            chapters_group,
            chapters_list,
            chapter_starts: RefCell::default(),
            fitted: Cell::new(FULL_SIZE),
            chapters_button,
            chapters_spinner,
            agent: std::cell::OnceCell::new(),
            generating: Cell::new(false),
            auto_actions_due: RefCell::default(),
            current_line: Cell::new(-1),
            mic,
            system,
            shared,
            hub: Rc::downgrade(hub),
            hypr_address: RefCell::default(),
            state: Cell::new(State::Idle),
            compact: Cell::new(false),
            full_size: Cell::new(FULL_SIZE),
            started_at: Cell::new(0),
            paused: Cell::new(false),
            frozen,
            animation_since: Cell::new(None),
            paused_secs: Cell::new(0),
            pause_began: Cell::new(0),
            pause_button,
            import_button,
            model_banner,
            model_downloading: Cell::new(false),
            drop_hint,
            staging: RefCell::default(),
            result_dir: RefCell::default(),
            abort: RefCell::default(),
            quit_when_done: Cell::new(false),
            loading: Cell::new(false),
            manifest: RefCell::default(),
            notes,
            notes_button,
            live_notes: RefCell::default(),
            showing_meeting_notes: Cell::new(false),
        });
        recorder.connect_signals(&open_button, &new_button);
        recorder.connect_find(&find_close);
        recorder.render();
        recorder
    }

    fn connect_find(self: &Rc<Self>, find_close: &gtk::Button) {
        let weak = Rc::downgrade(self);
        self.find_entry.connect_search_changed(move |_| {
            if let Some(r) = weak.upgrade() {
                r.apply_find(true);
            }
        });
        let weak = Rc::downgrade(self);
        self.find_entry.connect_activate(move |_| {
            if let Some(r) = weak.upgrade() {
                r.find_again(1);
            }
        });
        let weak = Rc::downgrade(self);
        self.find_entry.connect_next_match(move |_| {
            if let Some(r) = weak.upgrade() {
                r.find_again(1);
            }
        });
        let weak = Rc::downgrade(self);
        self.find_entry.connect_previous_match(move |_| {
            if let Some(r) = weak.upgrade() {
                r.find_again(-1);
            }
        });
        let weak = Rc::downgrade(self);
        self.find_entry.connect_stop_search(move |_| {
            if let Some(r) = weak.upgrade() {
                r.close_find();
            }
        });
        // Shift+Enter is still Enter to the entry, which would move forward.
        // Catch it on the way in and go back instead.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
            if enter && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                if let Some(r) = weak.upgrade() {
                    r.find_again(-1);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.find_entry.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.find_prev.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.find_again(-1);
            }
        });
        let weak = Rc::downgrade(self);
        self.find_next.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.find_again(1);
            }
        });
        let weak = Rc::downgrade(self);
        find_close.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.close_find();
            }
        });
        if let Some(action) = self.window.lookup_action("find") {
            let weak = Rc::downgrade(self);
            action
                .downcast::<gio::SimpleAction>()
                .expect("find is a SimpleAction")
                .connect_activate(move |_, _| {
                    if let Some(r) = weak.upgrade() {
                        r.open_find();
                    }
                });
        }
    }

    fn connect_signals(self: &Rc<Self>, open_button: &gtk::Button, new_button: &gtk::Button) {
        let weak = Rc::downgrade(self);
        self.notes.close_button.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            r.notes.unload();
            r.show_page();
        });
        // ↓ and ↑ go to the next and previous paragraph, like the buttons, as
        // long as the cursor is not in the notes or in a field to type in.
        let arrows = gtk::EventControllerKey::new();
        arrows.set_propagation_phase(gtk::PropagationPhase::Capture);
        self.window.add_controller(arrows.clone());
        let weak = Rc::downgrade(self);
        arrows.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(r) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let forward = match key {
                gtk::gdk::Key::Down => true,
                gtk::gdk::Key::Up => false,
                _ => return glib::Propagation::Proceed,
            };
            let typing = gtk::prelude::GtkWindowExt::focus(&r.window)
                .is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::TextView>());
            if !r.notes.widget.is_visible()
                || typing
                || modifiers.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK
                        | gtk::gdk::ModifierType::ALT_MASK
                        | gtk::gdk::ModifierType::SHIFT_MASK
                        | gtk::gdk::ModifierType::SUPER_MASK,
                )
            {
                return glib::Propagation::Proceed;
            }
            r.notes.jump(forward);
            glib::Propagation::Stop
        });

        let weak = Rc::downgrade(self);
        self.notes_button.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            if r.notes.is_loaded() {
                r.notes.unload();
                r.show_page();
            } else {
                let path = r.notes.last_path().unwrap_or_else(crate::notes::fresh_notes);
                r.open_notes(&path);
            }
        });

        let weak = Rc::downgrade(self);
        self.button.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            match r.state.get() {
                State::Idle | State::Done => r.start(),
                State::Recording => r.stop(),
                _ => {}
            }
        });

        // Import: a file dropped anywhere on the window, or picked from a dialog.
        // File managers offer a file list (Nautilus), some a single file.
        let drop = gtk::DropTarget::new(glib::Type::INVALID, gtk::gdk::DragAction::COPY);
        drop.set_types(&[gtk::gdk::FileList::static_type(), gio::File::static_type()]);
        // GTK's own check turns the drag down when the compositor offers the
        // source's preferred action (move) rather than copy, so decide on
        // the content alone. The file is only read; it is always a copy.
        drop.connect_accept(|_, offer| {
            let formats = offer.formats();
            formats.contains_type(gtk::gdk::FileList::static_type())
                || formats.contains_type(gio::File::static_type())
        });
        drop.connect_motion(|_, _, _| gtk::gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, _, _| {
            let Some(r) = weak.upgrade() else {
                return false;
            };
            r.drop_hint.set_visible(false);
            let file = value
                .get::<gtk::gdk::FileList>()
                .ok()
                .and_then(|list| list.files().into_iter().next())
                .or_else(|| value.get::<gio::File>().ok());
            let Some(file) = file else {
                return false;
            };
            match file.path() {
                Some(path) => {
                    r.confirm_import(path);
                    true
                }
                None => false,
            }
        });
        // While a file hovers over the window, say what dropping it does.
        let weak = Rc::downgrade(self);
        drop.connect_enter(move |_, _, _| {
            if let Some(r) = weak.upgrade() {
                r.drop_hint.set_visible(true);
            }
            gtk::gdk::DragAction::COPY
        });
        let weak = Rc::downgrade(self);
        drop.connect_leave(move |_| {
            if let Some(r) = weak.upgrade() {
                r.drop_hint.set_visible(false);
            }
        });
        self.window.add_controller(drop);

        let weak = Rc::downgrade(self);
        self.import_button.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Audio and video"));
            filter.add_mime_type("audio/*");
            filter.add_mime_type("video/*");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let dialog = gtk::FileDialog::builder()
                .title("Import an audio file")
                .filters(&filters)
                .build();
            let this = r.clone();
            dialog.open(Some(&r.window), gio::Cancellable::NONE, move |result| {
                if let Some(path) = result.ok().and_then(|f| f.path()) {
                    this.confirm_import(path);
                }
            });
        });

        let weak = Rc::downgrade(self);
        self.model_banner.connect_button_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.download_model();
            }
        });
        self.update_model_banner();

        let weak = Rc::downgrade(self);
        self.pause_button.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.toggle_pause();
            }
        });

        let weak = Rc::downgrade(self);
        self.compact_action.connect_activate(move |_, _| {
            if let Some(r) = weak.upgrade() {
                r.set_compact(!r.compact.get());
            }
        });

        // Ctrl+Q goes through the same check as the close button.

        let weak = Rc::downgrade(self);
        self.webcam_row.connect_active_notify(move |row| {
            if let Some(r) = weak.upgrade() {
                settings::save_webcam_on(row.is_active());
                if let Some((_, name)) = &r.webcam_camera {
                    settings::save_webcam(name);
                }
                // During a recording the camera joins or leaves; black fills where it was off.
                if r.state.get() == State::Recording {
                    let device = r.selected_webcam().map(|(device, _)| device);
                    r.webcam.switch(device.as_deref());
                    if device.is_some() && !r.paused.get() {
                        let this = r.clone();
                        glib::spawn_future_local(async move {
                            this.webcam.first_frame(Duration::from_secs(5)).await;
                        });
                    }
                }
                r.update_preview();
            }
        });

        let weak = Rc::downgrade(self);
        self.format_row.connect_selected_notify(move |_| {
            if let Some(r) = weak.upgrade()
                && !r.loading.get()
            {
                settings::save_format(r.selected_format());
            }
        });

        // The two language rows (recording page, done page) are one setting.
        let weak = Rc::downgrade(self);
        self.language_row.connect_selected_notify(move |row| {
            if let Some(r) = weak.upgrade() {
                if !r.loading.get() {
                    settings::save_language(r.selected_language());
                }
                if r.again_language_row.selected() != row.selected() {
                    r.again_language_row.set_selected(row.selected());
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.again_language_row.connect_selected_notify(move |row| {
            if let Some(r) = weak.upgrade()
                && r.language_row.selected() != row.selected()
            {
                r.language_row.set_selected(row.selected());
            }
        });

        // A click on a transcript row plays the meeting from there.
        let weak = Rc::downgrade(self);
        self.transcript_list.connect_row_activated(move |_, row| {
            let Some(r) = weak.upgrade() else { return };
            let index = row.index();
            let start = r
                .segments
                .borrow()
                .iter()
                .find(|(i, _)| *i == index)
                .map(|(_, ms)| *ms);
            if let Some(ms) = start {
                r.player.play_from(ms);
            }
        });

        let weak = Rc::downgrade(self);
        self.player.connect_position(move |ms| {
            if let Some(r) = weak.upgrade() {
                r.highlight(ms);
            }
        });

        let weak = Rc::downgrade(self);
        self.chapters_list.connect_row_activated(move |_, row| {
            if let Some(r) = weak.upgrade() {
                let start = r.chapter_starts.borrow().get(row.index() as usize).copied();
                if let Some(ms) = start {
                    r.player.play_from(ms);
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.chapters_button.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.generate_chapters();
            }
        });

        let weak = Rc::downgrade(self);
        new_button.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.ready();
            }
        });

        // The name on the done page drives the same title and folder rename.
        let weak = Rc::downgrade(self);
        self.done_title_row.connect_changed(move |row| {
            if let Some(r) = weak.upgrade()
                && r.title_row.text() != row.text()
            {
                set_name(&r.title_row, &row.text());
            }
        });
        let weak = Rc::downgrade(self);
        self.done_title_row.connect_apply(move |_| {
            if let Some(r) = weak.upgrade() {
                r.apply_title();
            }
        });
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(r) = weak.upgrade() {
                r.apply_title();
            }
        });
        self.done_title_row.add_controller(focus);

        let weak = Rc::downgrade(self);
        self.title_row.connect_changed(move |row| {
            if let Some(r) = weak.upgrade() {
                if r.state.get() == State::Recording
                    && let Some(staging) = r.staging.borrow().as_ref()
                {
                    write_recording_note(
                        staging,
                        row.text().trim(),
                        r.started_at.get(),
                        r.selected_format(),
                        r.selected_language(),
                    );
                }
                r.shared.lock().unwrap().title = row.text().to_string();
            }
        });

        // A new name renames the meeting folder, on Enter or when the row loses focus.
        let weak = Rc::downgrade(self);
        self.title_row.connect_apply(move |_| {
            if let Some(r) = weak.upgrade() {
                r.apply_title();
            }
        });
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(r) = weak.upgrade() {
                r.apply_title();
            }
        });
        self.title_row.add_controller(focus);

        let weak = Rc::downgrade(self);
        open_button.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            if let Some(dir) = r.result_dir.borrow().as_ref() {
                let uri = gio::File::for_path(dir).uri();
                let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
            }
        });

        let weak = Rc::downgrade(self);
        self.copy_button.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            let Some(dir) = r.result_dir.borrow().clone() else {
                return;
            };
            match std::fs::read_to_string(dir.join("transcript.md")) {
                Ok(text) => {
                    r.window.clipboard().set_text(&text);
                    r.toast("Copied to clipboard");
                }
                Err(_) => r.toast("No transcript found"),
            }
        });

        // The menu is made when it opens, so an edit to config.toml shows up
        // without restarting the app.
        let weak = Rc::downgrade(self);
        self.actions_button.set_create_popup_func(move |_| {
            if let Some(r) = weak.upgrade() {
                r.refresh_actions();
            }
        });
        self.add_actions_button.connect_clicked(|_| {
            let _ = gio::AppInfo::launch_default_for_uri(
                crate::actions::DOCS,
                None::<&gio::AppLaunchContext>,
            );
        });
        if let Some(action) = self.window.lookup_action("run-action") {
            let weak = Rc::downgrade(self);
            action
                .downcast::<gio::SimpleAction>()
                .expect("run-action is a SimpleAction")
                .connect_activate(move |_, parameter| {
                    let index = parameter.and_then(|p| p.get::<i32>());
                    if let (Some(r), Some(index)) = (weak.upgrade(), index) {
                        r.run_action(index as usize);
                    }
                });
        }

        let weak = Rc::downgrade(self);
        self.again_button.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            let Some(dir) = r.result_dir.borrow().clone() else {
                return;
            };
            let tracks = match r.manifest.borrow().as_ref() {
                Some(m) if m.imported.is_some() => {
                    Tracks::Single(source_track(&dir), m.speaker_count)
                }
                _ => Tracks::Kept(dir),
            };
            let this = r.clone();
            glib::spawn_future_local(async move {
                let result = this
                    .run_transcription(tracks, this.selected_language())
                    .await;
                this.hold_animation(&result).await;
                this.finished(true, result);
            });
        });

        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            let Some(r) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match r.state.get() {
                State::Recording => {
                    r.confirm_close_recording();
                    glib::Propagation::Stop
                }
                State::Transcribing => {
                    r.confirm_close_transcribing();
                    glib::Propagation::Stop
                }
                // Saving the audio takes a moment; finish it, then quit. The
                // hidden window no longer keeps the machine awake for the
                // transcription that follows; see `keep_awake`.
                State::Stopping => {
                    r.close_when_done();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });

        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(33), move || match weak.upgrade() {
            Some(r) => {
                if r.window.is_visible() {
                    if r.compact.get() {
                        r.compact_meters.iter().for_each(|m| m.queue_draw());
                    } else if r.layout.visible_child_name().as_deref() == Some("record") {
                        r.meters.iter().for_each(|m| m.queue_draw());
                    }
                }
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });

        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(500), move || match weak.upgrade() {
            Some(r) => {
                r.tick();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    /// The Actions menu from config.toml, and the button only when there are any.
    fn refresh_actions(&self) {
        let actions = crate::actions::load();
        let menu = gio::Menu::new();
        for (index, action) in actions.iter().enumerate() {
            let item = gio::MenuItem::new(Some(&action.name), None);
            item.set_action_and_target_value(
                Some("win.run-action"),
                Some(&(index as i32).to_variant()),
            );
            menu.append_item(&item);
        }
        self.actions_button.set_menu_model(Some(&menu));
        self.actions_button.set_visible(!actions.is_empty());
        self.add_actions_button.set_visible(actions.is_empty());
    }

    /// Runs one of your actions on this meeting from the menu.
    fn run_action(self: &Rc<Self>, index: usize) {
        let Some(action) = crate::actions::load().into_iter().nth(index) else {
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            this.perform(action).await;
        });
    }

    /// Runs the actions marked `auto = true` on the meeting they are due for,
    /// one after the other in the order of the config file. They are due once
    /// its transcript is done, and its chapters when an agent writes them. A
    /// window that has moved on to another meeting meanwhile leaves them. A
    /// window closed while it was busy closes once they are done.
    fn run_automatic_actions(self: &Rc<Self>) {
        let Some(due) = self.auto_actions_due.borrow_mut().take() else {
            return;
        };
        let actions = crate::actions::automatic();
        if actions.is_empty() || !self.showing(&due) {
            self.close_if_asked();
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            for action in actions {
                if !this.showing(&due) {
                    break;
                }
                this.perform(action).await;
            }
            this.close_if_asked();
        });
    }

    /// Whether the done page shows `dir`: the same meeting, also after a
    /// rename, which keeps the timestamp the folder name starts with.
    fn showing(&self, dir: &std::path::Path) -> bool {
        let prefix = |p: &std::path::Path| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.chars().take(12).collect::<String>())
        };
        self.state.get() == State::Done
            && self
                .result_dir
                .borrow()
                .as_deref()
                .is_some_and(|current| prefix(current) == prefix(dir))
    }

    /// Runs `action` on this meeting off the main thread and says how it went;
    /// a link it printed gets an Open button. Returns once the action is done.
    async fn perform(self: &Rc<Self>, action: crate::actions::Action) {
        let (Some(dir), Some(manifest)) = (
            self.result_dir.borrow().clone(),
            self.manifest.borrow().clone(),
        ) else {
            return;
        };
        // Stays until the action is done, then makes way for how it went.
        let running = adw::Toast::new(&format!("{}…", action.name));
        running.set_use_markup(false);
        running.set_timeout(0);
        self.toasts.add_toast(running.clone());
        let before = crate::actions::fingerprint(&dir);
        let (tx, rx) = async_channel::bounded(1);
        let name = action.name.clone();
        let folder = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(crate::actions::run(&action, &folder, &manifest));
        });
        let result = rx.recv().await;
        running.dismiss();
        // The action may have edited the meeting: show what is on disk now.
        if crate::actions::fingerprint(&dir) != before {
            self.reload_meeting(&dir);
        }
        let Ok(result) = result else { return };
        let toast = match result {
            Ok(outcome) => {
                let toast = adw::Toast::new(&outcome.message);
                toast.set_use_markup(false);
                if let Some(url) = outcome.url {
                    toast.set_button_label(Some("Open"));
                    toast.set_timeout(15);
                    toast.connect_button_clicked(move |_| {
                        let _ = gio::AppInfo::launch_default_for_uri(
                            &url,
                            None::<&gio::AppLaunchContext>,
                        );
                    });
                }
                toast
            }
            Err(why) => {
                let toast = adw::Toast::new(&format!("{name} failed: {why}"));
                toast.set_use_markup(false);
                toast.set_timeout(10);
                toast
            }
        };
        self.toasts.add_toast(toast);
    }

    /// Reads the meeting on the done page again after something else changed
    /// it, keeping the player and the scroll position where they are.
    fn reload_meeting(self: &Rc<Self>, dir: &std::path::Path) {
        if self.state.get() != State::Done || self.result_dir.borrow().as_deref() != Some(dir) {
            return;
        }
        let Some((_, manifest)) = meeting::open(dir) else {
            return;
        };
        set_name(&self.done_title_row, &manifest.title);
        set_name(&self.title_row, &manifest.title);
        *self.manifest.borrow_mut() = Some(manifest);
        if let Ok(text) = std::fs::read_to_string(dir.join("transcript.md")) {
            self.redraw_transcript(&text);
        }
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    fn selected_format(&self) -> Format {
        Format::ALL
            .get(self.format_row.selected() as usize)
            .copied()
            .unwrap_or(Format::Mono)
    }

    fn selected_language(&self) -> &'static str {
        LANGUAGES
            .get(self.language_row.selected() as usize)
            .map(|(code, _)| *code)
            .unwrap_or("auto")
    }

    fn title(&self) -> String {
        let typed = self.title_row.text().trim().to_owned();
        if typed.is_empty() {
            "Meeting".to_owned()
        } else {
            typed
        }
    }

    /// The meeting's folder for the bar widget and `wait`, "" before it has one.
    fn folder_text(&self) -> String {
        self.result_dir
            .borrow()
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default()
    }

    /// Once the window is on screen: its own Hyprland window is the one of
    /// this process that no other window of the app has claimed yet.
    fn learn_hypr_address(self: &Rc<Self>) {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            return;
        }
        let weak = Rc::downgrade(self);
        self.window.connect_map(move |_| {
            let weak = weak.clone();
            glib::timeout_add_local_once(Duration::from_millis(300), move || {
                let Some(r) = weak.upgrade() else { return };
                let Some(hub) = r.hub.upgrade() else { return };
                let taken: Vec<String> = hub
                    .recorders()
                    .iter()
                    .filter_map(|other| other.hypr_address.borrow().clone())
                    .collect();
                let clients = hyprctl_json("clients");
                let unclaimed = clients.as_ref().and_then(|c| c.as_array()).and_then(|clients| {
                    unclaimed_window(clients, std::process::id(), &taken)
                });
                *r.hypr_address.borrow_mut() = unclaimed;
            });
        });
    }

    /// The preview shows the camera while the toggle is on and the recording
    /// page is on screen; a paused recording leaves its last frame, dimmed.
    /// Nowhere else is the camera on.
    fn update_preview(&self) {
        let on = self.webcam_camera.is_some() && self.webcam_row.is_active();
        let on_page = !self.compact.get()
            && self.layout.visible_child_name().as_deref() == Some("record");
        self.webcam_preview.set_visible(on);
        self.webcam_preview
            .set_opacity(if self.paused.get() { 0.4 } else { 1.0 });
        if !on {
            self.webcam_preview.set_paintable(None::<&gtk::gdk::Paintable>);
        }
        if matches!(self.state.get(), State::Idle | State::Done) {
            let device = self.webcam_camera.as_ref().map(|(device, _)| device.as_path());
            self.webcam.preview(device.filter(|_| on && on_page));
        }
    }

    /// The camera to record, as (device, name); None when the toggle is off.
    fn selected_webcam(&self) -> Option<(PathBuf, String)> {
        self.webcam_camera.clone().filter(|_| self.webcam_row.is_active())
    }

    /// Opens the notes in `path` next to the recording page.
    fn open_notes(&self, path: &std::path::Path) {
        if self.notes.load(path) {
            self.show_page();
        }
    }

    /// With notes open on the recording page they sit next to
    /// the recorder, which keeps its own width. Returns whether it is on screen.
    fn arrange_notes(&self) -> bool {
        let page = if self.compact.get() {
            None
        } else {
            self.layout.visible_child_name()
        };
        let recording_page = page.as_deref() == Some("record");
        let done_page = page.as_deref() == Some("done");
        let shown = (recording_page || done_page) && self.notes.is_loaded();
        self.notes_button.set_visible(recording_page);
        self.notes.widget.set_visible(shown);
        // Next to the recorder the notes take the room; next to a transcript, the transcript does.
        let beside_recorder = shown && recording_page;
        self.view.set_hexpand(!beside_recorder);
        self.view.set_size_request(if beside_recorder { FULL_SIZE.0 } else { -1 }, -1);
        self.notes.widget.set_hexpand(beside_recorder);
        self.notes.widget.set_size_request(if shown && done_page { DONE_NOTES_WIDTH } else { -1 }, -1);
        self.update_preview();
        shown
    }

    /// The done page shows the notes the meeting was recorded with, when it has any.
    fn show_meeting_notes(&self, dir: &std::path::Path) {
        let file = dir.join(crate::notes::NOTES_FILE);
        if file.exists() && self.notes.load(&file) {
            self.showing_meeting_notes.set(true);
        } else if self.showing_meeting_notes.replace(false) || self.notes.is_loaded() {
            self.notes.unload();
        }
        self.show_page();
    }

    fn set_state(&self, state: State) {
        // The bar lives on the done page. Leave it behind with the page.
        if state != State::Done {
            self.dismiss_find();
        }
        self.state.set(state);
        self.keep_awake(keeps_awake(state));
        let mut shared = self.shared.lock().unwrap();
        shared.state = if state == State::Recording && self.paused.get() {
            "paused"
        } else {
            state.key()
        };
        shared.title = self.title_row.text().to_string();
        shared.started_at = self.started_at.get();
        shared.paused_secs = self.paused_secs.get();
        shared.pause_began = if self.paused.get() {
            self.pause_began.get()
        } else {
            0
        };
        if state != State::Transcribing {
            shared.progress = 0.0;
        }
        shared.folder = self.folder_text();
        drop(shared);
        self.render();
    }

    /// Asks the desktop not to idle while the meeting is processed, and lets
    /// it again afterwards. Idle is what holds off the screensaver, the lock
    /// and suspending on idle, and under a compositor with the idle-inhibit
    /// protocol GTK grants it there; asking for suspend as well only has the
    /// portal refuse it and GTK log a warning. Compositors honour the
    /// inhibitor while the window is on screen, so a window hidden with
    /// "Close when done" or "Stop and close" no longer keeps the machine awake.
    fn keep_awake(&self, on: bool) {
        let Some(app) = self.window.application() else {
            return;
        };
        let cookie = self.awake.get();
        if on && cookie == 0 {
            let flags = gtk::ApplicationInhibitFlags::IDLE;
            self.awake
                .set(app.inhibit(Some(&self.window), flags, Some("Finishing a meeting")));
        } else if !on && cookie != 0 {
            app.uninhibit(cookie);
            self.awake.set(0);
        }
    }

    fn render(&self) {
        let state = self.state.get();
        let recording = state == State::Recording;
        self.webcam_row.set_sensitive(
            self.webcam_camera.is_some()
                && matches!(state, State::Idle | State::Done | State::Recording),
        );
        self.dot.set_visible(recording);
        self.compact_button.set_visible(recording);
        self.find_button.set_visible(state == State::Done);
        self.compact_action.set_enabled(recording);
        self.language_row
            .set_sensitive(!matches!(state, State::Stopping | State::Transcribing));
        self.button.set_sensitive(matches!(
            state,
            State::Idle | State::Recording | State::Done
        ));
        self.show_page();
        self.button.remove_css_class("suggested-action");
        self.button.remove_css_class("destructive-action");
        self.pause_button.set_visible(recording);
        self.import_button.set_visible(state == State::Idle);
        self.pause_button
            .set_label(if self.paused.get() { "Resume" } else { "Pause" });
        if recording {
            self.button.set_label("Stop recording");
            self.button.add_css_class("destructive-action");
        } else {
            self.button.set_label(match state {
                State::Stopping => "Saving…",
                State::Transcribing => "Transcribing…",
                _ => "Start recording",
            });
            self.button.add_css_class("suggested-action");
        }

        let tracks_kept = self.result_dir.borrow().as_ref().is_some_and(|dir| {
            let (mic, computer) = export::tracks(dir);
            (mic.is_file() && computer.is_file()) || source_track(dir).is_file()
        });
        self.again_button.set_sensitive(tracks_kept);
        self.again_button.set_tooltip_text(Some(if tracks_kept {
            "Transcribe the meeting again with the selected language"
        } else {
            "The separate tracks of this meeting are gone, so it cannot be transcribed again"
        }));

        let text = match state {
            State::Idle => "Ready. Press Start recording when the meeting begins.".to_owned(),
            State::Recording if self.paused.get() => {
                "Paused. Nothing is recorded until you resume.".to_owned()
            }
            State::Recording => {
                "Recording. Name, audio file and language can still be changed.".to_owned()
            }
            State::Stopping => "Saving the audio…".to_owned(),
            State::Transcribing => "Transcribing the meeting on this computer…".to_owned(),
            State::Done => String::new(),
        };
        self.status_label.set_label(&text);
    }

    /// Shows the page for the current state, edge to edge while transcribing.
    fn show_page(&self) {
        if self.compact.get() {
            return;
        }
        let state = self.state.get();
        let page = match state {
            State::Stopping | State::Transcribing => "transcribing",
            State::Done => "done",
            _ => "record",
        };
        self.layout.set_visible_child_name(page);
        let prompting = self.arrange_notes();
        // Recording and transcribing share one size, so stopping does not jump.
        self.fit_window(match (page, prompting) {
            ("done", true) => DONE_NOTES_SIZE,
            ("done", false) => DONE_SIZE,
            ("record", true) => NOTES_SIZE,
            _ => FULL_SIZE,
        });
        let immersive = matches!(state, State::Stopping | State::Transcribing);
        self.view.set_extend_content_to_top_edge(immersive);
        if immersive {
            self.window.add_css_class("immersive");
        } else {
            self.window.remove_css_class("immersive");
        }
    }

    /// Grows the window for the wide done page and shrinks it back for the
    /// others. Only when the page asks for another size than last time, so a
    /// window the user resized is left alone otherwise.
    fn fit_window(&self, size: (i32, i32)) {
        if self.fitted.replace(size) == size {
            return;
        }
        let minimum = if size == DONE_SIZE {
            (820, 560)
        } else if size == DONE_NOTES_SIZE {
            (820 + DONE_NOTES_WIDTH, 560)
        } else if size == NOTES_SIZE {
            (900, 600)
        } else {
            (360, 200)
        };
        let current = (self.window.width(), self.window.height());
        self.window.set_size_request(minimum.0, minimum.1);
        self.window.set_default_size(size.0, size.1);
        self.window.queue_resize();
        let address = self.hypr_address.borrow().clone();
        glib::timeout_add_local_once(Duration::from_millis(50), move || {
            hyprland_resize(size, current, address);
        });
    }

    fn tick(&self) {
        if self.state.get() == State::Recording {
            let elapsed = self.elapsed();
            let clock = format_elapsed(elapsed);
            let opacity = if self.paused.get() {
                0.35
            } else if elapsed % 2 == 0 {
                1.0
            } else {
                0.3
            };
            self.timer.set_label(&clock);
            self.compact_timer.set_label(&clock);
            self.dot.set_opacity(opacity);
            self.compact_dot.set_opacity(opacity);
        }
    }

    /// Switches between the full layout and the compact one with only the waves.
    fn set_compact(&self, compact: bool) {
        if compact == self.compact.get() || (compact && self.state.get() != State::Recording) {
            return;
        }
        self.compact.set(compact);
        let size = if compact {
            let (w, h) = (self.window.width(), self.window.height());
            if w > COMPACT_SIZE.0 && h > COMPACT_SIZE.1 {
                self.full_size.set((w, h));
            }
            self.layout.set_visible_child_name("compact");
            self.arrange_notes();
            self.view.set_reveal_top_bars(false);
            // libadwaita keeps every window at least 360x200 unless told otherwise.
            self.window.set_size_request(COMPACT_SIZE.0, COMPACT_SIZE.1);
            COMPACT_SIZE
        } else {
            self.view.set_reveal_top_bars(true);
            self.show_page();
            self.window.set_size_request(360, 200);
            self.full_size.get()
        };
        let current = (self.window.width(), self.window.height());
        self.window.set_default_size(size.0, size.1);
        self.window.queue_resize();
        // A compositor may ignore a client asking to change the size of a
        // window that is already on screen; on Hyprland, ask it directly.
        let address = self.hypr_address.borrow().clone();
        glib::timeout_add_local_once(Duration::from_millis(50), move || {
            hyprland_resize(size, current, address);
        });
    }

    /// Recorded time so far, without the pauses.
    fn elapsed(&self) -> i64 {
        let until = if self.paused.get() {
            self.pause_began.get()
        } else {
            ipc::now()
        };
        (until - self.started_at.get() - self.paused_secs.get()).max(0)
    }

    fn toggle_pause(self: &Rc<Self>) {
        if self.state.get() != State::Recording || self.starting.get() {
            return;
        }
        if !self.paused.get() {
            self.webcam.audio_paused();
            self.pause_began.set(ipc::now());
            self.paused.set(true);
            self.mic.set_paused(true);
            self.system.set_paused(true);
            self.after_pause_change();
            return;
        }
        // Resuming: with the camera on, the audio waits for its first frame again.
        self.webcam.resume();
        self.starting.set(true);
        let this = self.clone();
        glib::spawn_future_local(async move {
            this.webcam.first_frame(Duration::from_secs(3)).await;
            this.starting.set(false);
            // Stopped while the camera came back: nothing to resume.
            if this.state.get() != State::Recording || !this.paused.get() {
                return;
            }
            this.paused_secs
                .set(this.paused_secs.get() + ipc::now() - this.pause_began.get());
            this.paused.set(false);
            this.mic.set_paused(false);
            this.system.set_paused(false);
            this.webcam.audio_started();
            this.after_pause_change();
        });
    }

    fn after_pause_change(&self) {
        self.update_preview();
        self.freeze_meters(self.paused.get());
        self.set_state(State::Recording);
        self.tick();
    }

    fn freeze_meters(&self, frozen: bool) {
        for (cell, source) in self.frozen.iter().zip([&self.mic, &self.system]) {
            *cell.borrow_mut() = frozen.then(|| source.levels());
        }
        for meter in self.meters.iter().chain(self.compact_meters.iter()) {
            meter.queue_draw();
        }
    }

    /// Asks for the language and the number of speakers, then imports.
    fn confirm_import(self: &Rc<Self>, path: PathBuf) {
        if matches!(
            self.state.get(),
            State::Recording | State::Stopping | State::Transcribing
        ) {
            self.toast("Finish the current recording first");
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some("Import audio"), Some(&name));
        let group = adw::PreferencesGroup::new();
        let language_labels: Vec<&str> =
            LANGUAGES.iter().map(|(_, label)| label.as_str()).collect();
        let language = adw::ComboRow::builder()
            .title("Language")
            .model(&gtk::StringList::new(&language_labels))
            .selected(self.language_row.selected())
            .build();
        let speakers = adw::ComboRow::builder()
            .title("Speakers")
            .subtitle("Recognized by their voices")
            .model(&gtk::StringList::new(&SPEAKER_CHOICES))
            .build();
        group.add(&language);
        group.add(&speakers);
        dialog.set_extra_child(Some(&group));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("import", "Import");
        dialog.set_response_appearance("import", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("import"));
        dialog.set_close_response("cancel");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "import" {
                return;
            }
            let code = LANGUAGES
                .get(language.selected() as usize)
                .map(|(code, _)| *code)
                .unwrap_or("auto");
            let count = match speakers.selected() {
                0 => None,
                n => Some(n as usize),
            };
            this.import_file(path.clone(), code, count);
        });
        dialog.present(Some(&self.window));
    }

    /// Makes a meeting of an audio file: a folder with the levelled audio and
    /// the source kept for transcribing again, then the transcript.
    fn import_file(
        self: &Rc<Self>,
        path: PathBuf,
        language: &'static str,
        speakers: Option<usize>,
    ) {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Imported audio".to_owned());
        // The file's own date says when the meeting was, better than now.
        let started_at = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or_else(ipc::now, |d| d.as_secs() as i64);
        let mut out = output_dir(started_at, &title);
        let mut n = 2;
        while out.exists() {
            out = output_dir(started_at, &format!("{title} {n}"));
            n += 1;
        }

        self.player.unload();
        set_name(&self.title_row, &title);
        self.started_at.set(started_at);
        *self.result_dir.borrow_mut() = Some(out.clone());
        *self.manifest.borrow_mut() = Some(Manifest {
            title: title.clone(),
            started_at,
            duration_secs: 0,
            format: Format::Mono,
            language: language.to_owned(),
            speakers: Vec::new(),
            labels: Vec::new(),
            imported: path.file_name().map(|n| n.to_string_lossy().into_owned()),
            speaker_count: speakers,
            model: None,
            chapters: Vec::new(),
            chapters_by: None,
        });
        self.animation_since.set(Some(std::time::Instant::now()));
        self.animation.reset();
        self.animation.set_stage("Importing audio");
        self.animation.set_running(true);
        self.set_state(State::Stopping);

        let this = self.clone();
        glib::spawn_future_local(async move {
            let (source, target) = (path.clone(), out.clone());
            let staging = glib::user_cache_dir()
                .join(APP_NAME)
                .join(format!("import-{}", ipc::now()));
            let converted = gio::spawn_blocking(move || import_audio(&source, &target, &staging))
                .await
                .unwrap_or_else(|_| Err("the import stopped unexpectedly".into()));
            let result = match converted {
                Ok(duration) => {
                    if let Some(m) = this.manifest.borrow_mut().as_mut() {
                        m.duration_secs = duration;
                        let _ = meeting::write(&out, m);
                    }
                    this.run_transcription(Tracks::Single(source_track(&out), speakers), language)
                        .await
                }
                Err(message) => Err(message),
            };
            this.hold_animation(&result).await;
            this.finished(result.is_ok(), result);
        });
    }

    /// After a crash: offers to save recordings that were never stopped.
    fn offer_recovery(self: &Rc<Self>) {
        let current = self.staging.borrow().clone();
        let Some(staging) = unfinished_recordings()
            .into_iter()
            .find(|dir| Some(dir) != current.as_ref())
        else {
            // One question at a time: the bar widget waits for a start without a recovery.
            self.offer_bar_widget();
            return;
        };
        let note = read_recording_note(&staging);
        let started_at = note.as_ref().map_or(0, |n| n.started_at);
        let when = glib::DateTime::from_unix_local(started_at)
            .and_then(|t| t.format("%A %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let length = format_elapsed(raw_duration(&staging));
        let dialog = adw::AlertDialog::new(
            Some("Unfinished recording found"),
            Some(&format!(
                "A recording from {when} ({length}) was not stopped properly, probably because the app quit. Save it as a meeting?"
            )),
        );
        dialog.add_response("discard", "Discard");
        dialog.add_response("later", "Later");
        dialog.add_response("save", "Save");
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("later");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| match response {
            "discard" => {
                let _ = std::fs::remove_dir_all(&staging);
            }
            "save" => this.recover(staging.clone(), note.clone()),
            _ => {}
        });
        dialog.present(Some(&self.window));
    }

    /// Asks once whether the widget may go in the Omarchy bar.
    fn offer_bar_widget(self: &Rc<Self>) {
        if !crate::bar_widget::should_offer() {
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Add Meeting Recorder to your bar?"),
            Some(
                "The bar then shows a live waveform while you record, and nothing the rest of the time. Clicking it opens the app.",
            ),
        );
        dialog.add_response("no", "No Thanks");
        dialog.add_response("add", "Add to Bar");
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("no");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "add" {
                settings::set_bar_widget_offered();
                return;
            }
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(crate::bar_widget::add());
            });
            let this = this.clone();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(())) => {
                        settings::set_bar_widget_offered();
                        this.toast("Added to the bar");
                    }
                    Ok(Err(message)) => {
                        this.toast(&format!("Could not add it to the bar: {message}"))
                    }
                    Err(_) => {}
                }
            });
        });
        dialog.present(Some(&self.window));
    }

    /// Picks an unfinished recording up where it stopped and finishes it like Stop does.
    fn recover(self: &Rc<Self>, staging: PathBuf, note: Option<RecordingNote>) {
        if self.state.get() != State::Idle {
            self.toast("Finish the current recording first");
            return;
        }
        let note = note.unwrap_or_else(|| RecordingNote {
            title: "Recovered recording".to_owned(),
            started_at: ipc::now() - raw_duration(&staging),
            format: settings::load_format(),
            language: settings::load_language().to_owned(),
        });
        set_name(&self.title_row, &note.title);
        if let Some(i) = Format::ALL.iter().position(|f| *f == note.format) {
            self.format_row.set_selected(i as u32);
        }
        if let Some(i) = LANGUAGES
            .iter()
            .position(|(code, _)| *code == note.language)
        {
            self.language_row.set_selected(i as u32);
        }
        self.started_at.set(note.started_at);
        self.paused.set(false);
        self.paused_secs.set(0);
        *self.staging.borrow_mut() = Some(staging);
        *self.result_dir.borrow_mut() = None;
        // Straight into what Stop does from here.
        self.set_state(State::Recording);
        self.stop();
    }

    /// Says so when the speech model is not on disk yet, with a button to get it.
    fn update_model_banner(&self) {
        if self.model_downloading.get() {
            return;
        }
        match crate::models::missing() {
            Some((name, size_mb)) => {
                let size = if size_mb >= 1000 {
                    format!("{:.1} GB", f64::from(size_mb) / 1000.0)
                } else {
                    format!("{size_mb} MB")
                };
                self.model_banner.set_title(&format!(
                    "The speech model ({name}, {size}) is needed to transcribe"
                ));
                self.model_banner.set_button_label(Some("Download"));
                self.model_banner.set_revealed(true);
            }
            None => self.model_banner.set_revealed(false),
        }
    }

    /// Fetches the speech model in the background; recording can go on.
    fn download_model(self: &Rc<Self>) {
        if self.model_downloading.replace(true) {
            return;
        }
        self.model_banner.set_button_label(None);
        self.model_banner.set_title("Downloading the speech model…");
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        let (done_tx, done_rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = crate::models::ensure(&events_tx, &Abort::default());
            let _ = done_tx.send_blocking(result);
            let _ = events_tx.send_blocking(Event::Finished);
        });
        let this = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = events_rx.recv().await {
                match event {
                    Event::Progress(progress) => this.model_banner.set_title(&format!(
                        "Downloading the speech model… {:.0}%",
                        progress * 100.0
                    )),
                    Event::Finished => break,
                    _ => {}
                }
            }
            let result = done_rx
                .recv()
                .await
                .unwrap_or_else(|_| Err("the download stopped".into()));
            this.model_downloading.set(false);
            match result {
                Ok(_) => this.toast("Speech model ready"),
                Err(message) => this.toast(&format!("Could not download the model: {message}")),
            }
            this.update_model_banner();
        });
    }

    /// Back to the recording page, ready for the next meeting.
    fn ready(&self) {
        self.player.unload();
        *self.result_dir.borrow_mut() = None;
        *self.manifest.borrow_mut() = None;
        set_name(&self.title_row, "");
        self.timer.set_label("00:00");
        self.compact_timer.set_label("00:00");
        // The meeting's notes stay with the meeting; the ones recorded with come back.
        if self.showing_meeting_notes.replace(false) {
            self.notes.unload();
            if let Some(live) = self.live_notes.borrow().clone() {
                self.notes.load(&live);
            }
        }
        self.set_state(State::Idle);
        self.update_model_banner();
    }

    fn start(self: &Rc<Self>) {
        // One recording at a time: the audio sources are shared by all windows.
        if let Some(other) = self.hub.upgrade().and_then(|hub| hub.recording())
            && !Rc::ptr_eq(&other, self)
        {
            self.toast("Another window is recording");
            return;
        }
        if self.starting.get() {
            return;
        }
        // A name typed before starting is kept; otherwise one from the time.
        if self.title_row.text().trim().is_empty() {
            let title = glib::DateTime::now_local()
                .and_then(|now| now.format("Meeting %H:%M"))
                .map(|s| s.to_string())
                .unwrap_or_else(|_| "Meeting".to_owned());
            set_name(&self.title_row, &title);
        }
        self.paused.set(false);
        self.paused_secs.set(0);
        self.pause_began.set(0);
        let staging = glib::user_cache_dir()
            .join(APP_NAME)
            .join(ipc::now().to_string());
        if let Err(e) = std::fs::create_dir_all(&staging) {
            self.status_label
                .set_label(&format!("Could not start recording: {e}"));
            return;
        }
        let camera = self.selected_webcam().map(|(device, _)| device);
        self.webcam.begin(&staging, camera.as_deref());
        if camera.is_none() {
            self.start_audio(staging);
            return;
        }
        // The audio waits for the camera's first frame, so the two start together.
        self.starting.set(true);
        self.button.set_sensitive(false);
        self.status_label.set_label("Starting the camera…");
        let this = self.clone();
        glib::spawn_future_local(async move {
            this.webcam.first_frame(Duration::from_secs(3)).await;
            this.starting.set(false);
            this.button.set_sensitive(true);
            this.start_audio(staging);
        });
    }

    fn start_audio(self: &Rc<Self>, staging: PathBuf) {
        let started_at = ipc::now();
        if let Err(e) = self
            .mic
            .start_recording(&staging.join("mic.raw"))
            .and_then(|_| self.system.start_recording(&staging.join("system.raw")))
        {
            self.mic.stop_recording();
            self.system.stop_recording();
            let _ = self.webcam.end();
            self.status_label
                .set_label(&format!("Could not start recording: {e}"));
            return;
        }
        self.webcam.audio_started();
        write_recording_note(
            &staging,
            &self.title(),
            started_at,
            self.selected_format(),
            self.selected_language(),
        );
        *self.staging.borrow_mut() = Some(staging);
        *self.result_dir.borrow_mut() = None;
        self.player.unload();
        self.started_at.set(started_at);
        self.timer.set_label("00:00");
        self.compact_timer.set_label("00:00");
        self.set_state(State::Recording);
    }

    fn stop(self: &Rc<Self>) {
        if self.state.get() != State::Recording {
            return;
        }
        if self.paused.get() {
            self.paused_secs
                .set(self.paused_secs.get() + ipc::now() - self.pause_began.get());
            self.paused.set(false);
        }
        self.freeze_meters(false);
        self.animation_since.set(Some(std::time::Instant::now()));
        self.mic.stop_recording();
        self.system.stop_recording();
        let webcam = self.webcam.end();
        let Some(staging) = self.staging.borrow().clone() else {
            return;
        };
        self.set_compact(false);
        // Straight to the animation: the waves would suggest it is still recording.
        self.animation.reset();
        self.animation.set_stage("Saving audio");
        self.animation.set_running(true);
        self.set_state(State::Stopping);

        let format = self.selected_format();
        let language = self.selected_language();
        let out = output_dir(self.started_at.get(), &self.title());
        // The notes read from, as they stood when the recording stopped.
        let script = self.notes.text();
        if !self.showing_meeting_notes.get() {
            *self.live_notes.borrow_mut() = self.notes.path();
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let (audio_out, audio_staging) = (out.clone(), staging.clone());
            let saved = gio::spawn_blocking(move || {
                let _ = std::fs::create_dir_all(&audio_out);
                let (mic, system) = (
                    audio_staging.join("mic.raw"),
                    audio_staging.join("system.raw"),
                );
                let audio = export_audio(&mic, &system, &audio_out, format);
                let tracks = export_tracks(&mic, &system, &audio_out);
                crate::webcam::export(&audio_staging, &audio_out, webcam);
                (audio, tracks)
            })
            .await
            .unwrap_or((false, false));
            let manifest = Manifest {
                title: this.title(),
                started_at: this.started_at.get(),
                duration_secs: raw_duration(&staging),
                format,
                language: language.to_owned(),
                speakers: vec![
                    settings::load_your_name(),
                    meeting::DEFAULT_REMOTE.to_owned(),
                ],
                labels: Vec::new(),
                imported: None,
                speaker_count: None,
                model: None,
                chapters: Vec::new(),
                chapters_by: None,
            };
            let _ = meeting::write(&out, &manifest);
            if let Some(script) = script {
                let _ = std::fs::write(out.join(crate::notes::NOTES_FILE), script);
            }
            *this.manifest.borrow_mut() = Some(manifest);
            *this.result_dir.borrow_mut() = Some(out);

            let result = this
                .run_transcription(Tracks::Raw(staging.clone()), language)
                .await;
            // The kept tracks are enough to transcribe again; the raw files can go.
            if saved == (true, true) {
                let _ = std::fs::remove_dir_all(&staging);
            }
            this.hold_animation(&result).await;
            this.finished(saved.0, result);
        });
    }

    /// Transcribes into `result_dir/transcript.md`, driving the animation.
    async fn run_transcription(
        &self,
        tracks: Tracks,
        language: &'static str,
    ) -> Result<(), String> {
        let Some(out) = self.result_dir.borrow().clone() else {
            return Err("no meeting folder".into());
        };
        self.set_compact(false);
        if self.animation_since.get().is_none() {
            self.animation_since.set(Some(std::time::Instant::now()));
        }
        self.set_state(State::Transcribing);
        self.animation.reset();
        self.animation.set_stage("Loading audio");
        self.animation.set_progress(0.0);
        self.animation.set_running(true);

        let abort = Abort::default();
        *self.abort.borrow_mut() = Some(abort.clone());
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        let (done_tx, done_rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = match tracks {
                Tracks::Single(path, speakers) => transcribe::load_track(&path).and_then(|track| {
                    transcribe::transcribe_single(&track, language, speakers, &events_tx, &abort)
                }),
                Tracks::Raw(dir) | Tracks::Kept(dir) => {
                    let (mic_path, computer_path) = if dir.join("mic.raw").exists() {
                        (dir.join("mic.raw"), dir.join("system.raw"))
                    } else {
                        export::tracks(&dir)
                    };
                    transcribe::load_track(&mic_path).and_then(|mic| {
                        let computer = transcribe::load_track(&computer_path)?;
                        transcribe::transcribe(&mic, &computer, language, &events_tx, &abort)
                    })
                }
            };
            let _ = done_tx.send_blocking(result);
            let _ = events_tx.send_blocking(Event::Finished);
        });

        while let Ok(event) = events_rx.recv().await {
            match event {
                Event::Stage(stage) => self.animation.set_stage(&stage),
                Event::Progress(progress) => {
                    self.animation.set_progress(progress);
                    self.shared.lock().unwrap().progress = progress;
                }
                Event::Segment(text) => {
                    // The transcription labels speakers You/Remote or Speaker N;
                    // show this meeting's names.
                    let text = match (self.manifest.borrow().as_ref(), text.split_once(": ")) {
                        (Some(m), Some((label, rest))) => {
                            let labels = m.default_labels();
                            match labels.iter().position(|l| l == label) {
                                Some(i) if i < m.speakers.len() => {
                                    format!("{}: {rest}", m.speakers[i])
                                }
                                _ => text.clone(),
                            }
                        }
                        _ => text.clone(),
                    };
                    self.animation.push_text(&text);
                }
                Event::Finished => break,
            }
        }
        let result = done_rx
            .recv()
            .await
            .unwrap_or_else(|_| Err("transcription stopped unexpectedly".into()));
        *self.abort.borrow_mut() = None;

        let transcript = result?;
        // The name as it is now; it may have been edited while transcribing.
        let out = self.result_dir.borrow().clone().unwrap_or(out);
        let date = glib::DateTime::from_unix_local(self.started_at.get())
            .and_then(|t| t.format("%Y-%m-%d %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let mut markdown = transcribe::to_markdown(&self.title(), &date, &transcript);
        if let Some(manifest) = self.manifest.borrow_mut().as_mut() {
            // An import finds its own number of speakers; keep names already
            // given and number the rest.
            if manifest.imported.is_some() {
                let found = speakers_in(&markdown);
                let mut names = manifest.speakers.clone();
                names.resize_with(found.len(), String::new);
                for (i, name) in names.iter_mut().enumerate() {
                    if name.is_empty() {
                        *name = format!("Speaker {}", i + 1);
                    }
                }
                manifest.speakers = names;
            } else {
                // A recording: You, or You 1, You 2, ... when several people
                // share the mic, then Remote or Remote 1, Remote 2, ... Keep
                // the names already given to a label; a new one gets a default.
                let mut labels: Vec<String> = speakers_in(&markdown)
                    .into_iter()
                    .filter(|l| meeting::side_of(l).is_some())
                    .collect();
                labels.sort_by_key(|l| {
                    let (side, n) = meeting::side_of(l).unwrap_or(("", 0));
                    (side != meeting::DEFAULT_YOU, n)
                });
                if !labels.is_empty() {
                    let known: std::collections::HashMap<String, String> = manifest
                        .default_labels()
                        .into_iter()
                        .zip(manifest.speakers.iter().cloned())
                        .collect();
                    let names = labels
                        .iter()
                        .map(|label| {
                            if let Some(name) = known.get(label) {
                                return name.clone();
                            }
                            // One voice where there were several: the name of the first.
                            if let Some(name) = known.get(&format!("{label} 1")) {
                                return name.clone();
                            }
                            match meeting::side_of(label) {
                                // The first voice on the mic is you.
                                Some((side, 1)) => {
                                    known.get(side).cloned().unwrap_or_else(|| label.clone())
                                }
                                Some((side, n)) if side == meeting::DEFAULT_YOU => {
                                    format!("Room {n}")
                                }
                                _ => label.clone(),
                            }
                        })
                        .collect();
                    manifest.labels = labels;
                    manifest.speakers = names;
                }
            }
            // The transcription labels speakers You/Remote or Speaker N; use
            // the names of this meeting.
            let renames: Vec<(String, String)> = manifest
                .default_labels()
                .into_iter()
                .zip(manifest.speakers.iter().cloned())
                .filter(|(label, name)| label != name)
                .collect();
            markdown = meeting::relabel_all(&markdown, &renames);
            manifest.language = language.to_owned();
            manifest.model = Some(crate::models::configured());
            manifest.title = self.title();
            // Chapters of a previous transcript would point at lines that are gone.
            manifest.chapters.clear();
            manifest.chapters_by = None;
            let _ = meeting::write(&out, manifest);
        }
        std::fs::write(out.join("transcript.md"), markdown)
            .map_err(|e| format!("could not write the transcript: {e}"))
    }

    /// Keeps the animation on screen for at least ten seconds, also after a
    /// short recording, so it reads as a step rather than a flicker. Skipped
    /// when it was cancelled or the window is already gone.
    async fn hold_animation(&self, result: &Result<(), String>) {
        const MINIMUM: Duration = Duration::from_secs(10);
        if let Some(since) = self.animation_since.take() {
            let shown = since.elapsed();
            let cancelled = matches!(result, Err(message) if message == CANCELLED);
            if shown < MINIMUM && !cancelled && self.window.is_visible() {
                self.animation.set_progress(1.0);
                self.animation.set_stage("Done");
                glib::timeout_future(MINIMUM - shown).await;
            }
        }
        self.animation.set_running(false);
    }

    fn finished(self: &Rc<Self>, audio_ok: bool, transcript: Result<(), String>) {
        set_name(&self.done_title_row, &self.title_row.text());
        self.set_state(State::Done);
        if let Some(dir) = self.result_dir.borrow().clone() {
            self.show_meeting_notes(&dir);
        }
        // Follow a name that was edited while the transcription ran.
        self.apply_title();

        let problem = match (&transcript, audio_ok) {
            (Err(message), _) if message == CANCELLED => {
                Some("Transcription cancelled.".to_owned())
            }
            (Err(message), _) => Some(format!("Transcription failed: {message}.")),
            (Ok(()), false) => Some("Could not save the audio.".to_owned()),
            _ => None,
        };
        let text = self
            .result_dir
            .borrow()
            .as_ref()
            .and_then(|dir| std::fs::read_to_string(dir.join("transcript.md")).ok());
        if let Some(dir) = self.result_dir.borrow().as_ref() {
            self.player.load(dir);
        }
        self.show_transcript(text.as_deref(), problem.as_deref());
        self.transcript_scroll.vadjustment().set_value(0.0);
        let actions_follow = problem.is_none() && !crate::actions::automatic().is_empty();
        if let Some(problem) = problem {
            eprintln!("{APP_NAME}: {problem}");
        } else {
            self.window.set_default_widget(Some(&self.copy_button));
            self.copy_button.grab_focus();
            // Your automatic actions run on a fresh transcript, after the
            // chapters when there are any, so they see the whole meeting.
            if actions_follow {
                *self.auto_actions_due.borrow_mut() = self.result_dir.borrow().clone();
            }
            // A fresh transcript gets chapters when an agent is around.
            if self.can_have_chapters() {
                self.generate_chapters();
            } else {
                self.run_automatic_actions();
            }
        }
        // A window closed while busy goes now, or once its automatic actions
        // are done: quitting first would kill them halfway or before they start.
        if !actions_follow {
            self.close_if_asked();
        }
    }

    /// Closes a window that was closed while it was busy, now that it is done.
    fn close_if_asked(self: &Rc<Self>) {
        if self.quit_when_done.get()
            && let Some(app) = self.window.application()
        {
            // Closed while busy: now it is done, it goes. The app goes with it
            // unless another window is still open.
            let others = self
                .hub
                .upgrade()
                .map(|hub| {
                    hub.recorders()
                        .into_iter()
                        .filter(|r| !Rc::ptr_eq(r, self))
                        .count()
                })
                .unwrap_or(0);
            if others > 0 {
                self.window.destroy();
            } else {
                app.quit();
            }
        }
    }

    /// Shows a saved meeting on the done page, with the settings it was made with.
    fn open_meeting(self: &Rc<Self>, path: &std::path::Path) {
        if matches!(
            self.state.get(),
            State::Recording | State::Stopping | State::Transcribing
        ) {
            // This window is busy with its own meeting: open it in another.
            if let (Some(hub), Some(app)) = (self.hub.upgrade(), self.window.application()) {
                let app = app
                    .downcast::<adw::Application>()
                    .expect("an adw::Application");
                let other = hub.free_window(&app);
                if !Rc::ptr_eq(&other, self) {
                    other.open_meeting(path);
                    other.window.present();
                    return;
                }
            }
            self.toast("Finish the current recording first");
            return;
        }
        let Some((dir, manifest)) = meeting::open(path) else {
            self.toast("This is not a meeting the recorder can open");
            return;
        };
        // Folders from before manifests existed get one now.
        if meeting::find(&dir).is_none() {
            let _ = meeting::write(&dir, &manifest);
        }

        self.loading.set(true);
        if let Some(i) = Format::ALL.iter().position(|f| *f == manifest.format) {
            self.format_row.set_selected(i as u32);
        }
        if let Some(i) = LANGUAGES
            .iter()
            .position(|(code, _)| *code == manifest.language)
        {
            self.language_row.set_selected(i as u32);
        }
        self.loading.set(false);

        self.started_at.set(manifest.started_at);
        set_name(&self.title_row, &manifest.title);
        set_name(&self.done_title_row, &manifest.title);
        *self.manifest.borrow_mut() = Some(manifest);
        *self.result_dir.borrow_mut() = Some(dir.clone());
        self.set_state(State::Done);
        self.show_meeting_notes(&dir);
        let text = std::fs::read_to_string(dir.join("transcript.md")).ok();
        let problem = text
            .is_none()
            .then_some("This meeting has no transcript yet.");
        self.player.load(&dir);
        self.show_transcript(text.as_deref(), problem);
        self.transcript_scroll.vadjustment().set_value(0.0);
        // Never leave the focus in the name: typing would rename the meeting.
        self.window.set_default_widget(Some(&self.copy_button));
        self.copy_button.grab_focus();
    }

    /// Fills the done page from transcript.md: the meta line and the readable transcript.
    fn show_transcript(self: &Rc<Self>, markdown: Option<&str>, problem: Option<&str>) {
        let ok = problem.is_none() && markdown.is_some();
        self.done_heading.set_label(if ok {
            "Transcript ready"
        } else {
            "Meeting saved"
        });
        self.done_icon.set_icon_name(Some(if ok {
            "object-select-symbolic"
        } else {
            "dialog-warning-symbolic"
        }));
        self.copy_button.set_sensitive(markdown.is_some());
        self.actions_button.set_sensitive(markdown.is_some());
        self.refresh_actions();

        while let Some(row) = self.transcript_list.row_at_index(0) {
            self.transcript_list.remove(&row);
        }
        let mut meta = Vec::new();
        let mut segments = Vec::new();
        let mut notes = Vec::new();
        self.current_line.set(-1);
        if let Some(problem) = problem {
            notes.push(problem.to_owned());
        }
        let mut paragraphs = Paragraphs::default();
        for (index, line) in markdown.unwrap_or("").lines().enumerate() {
            if let Some(value) = line.strip_prefix("- **Duration:** ") {
                meta.push(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("- **Language:** ") {
                // Nothing was said, so nothing was detected.
                if !value.trim().eq_ignore_ascii_case("unknown") {
                    meta.push(value.trim().to_owned());
                }
            } else if let Some((time, speaker, text)) = parse_segment(line) {
                paragraphs.add(time, speaker, text, index);
            } else if let Some(note) = line.strip_prefix('_').and_then(|l| l.strip_suffix('_')) {
                notes.push(note.to_owned());
            }
        }
        // Colours follow the speaker order of the meeting, then first appearance.
        let mut order: Vec<String> = self
            .manifest
            .borrow()
            .as_ref()
            .map(|m| m.speakers.clone())
            .unwrap_or_default();
        for paragraph in &paragraphs.list {
            if !order.contains(&paragraph.speaker) {
                order.push(paragraph.speaker.clone());
            }
        }
        let chapters = self
            .manifest
            .borrow()
            .as_ref()
            .map(|m| m.chapters.clone())
            .unwrap_or_default();
        // One string per row, in the same order as the list, so a hit is a row.
        let mut search_rows = Vec::new();
        for note in &notes {
            self.transcript_list.append(&note_row(note));
            search_rows.push(note.clone());
        }
        // The speaker column is as wide as the longest name.
        let speaker_width = paragraphs
            .list
            .iter()
            .map(|p| text_width(&self.transcript_list, &p.speaker))
            .max()
            .unwrap_or(0);
        let long = paragraphs
            .list
            .last()
            .is_some_and(|p| p.start_ms >= 3_600_000);
        let time_width = text_width(
            &self.transcript_list,
            if long { "0:00:00" } else { "00:00" },
        );
        let mut next_chapter = chapters.iter().peekable();
        let mut lines = Vec::new();
        for paragraph in paragraphs.list {
            while let Some(chapter) = next_chapter.next_if(|c| c.start_ms <= paragraph.start_ms) {
                let row = chapter_row(&chapter.title, chapter.start_ms);
                segments.push((row_count(&self.transcript_list), chapter.start_ms));
                self.transcript_list.append(&row);
                search_rows.push(chapter.title.clone());
            }
            let index = order
                .iter()
                .position(|s| *s == paragraph.speaker)
                .unwrap_or(0);
            let row = self.paragraph_row(&paragraph, index, time_width, speaker_width);
            segments.push((row_count(&self.transcript_list), paragraph.start_ms));
            self.transcript_list.append(&row);
            search_rows.push(format!("{} {}", paragraph.speaker, paragraph.text));
            lines.push((paragraph.start_ms, paragraph.speaker, paragraph.text));
        }
        if let Some(manifest) = self.manifest.borrow().as_ref() {
            meta.push(manifest.format.short_label().to_owned());
        }
        *self.segments.borrow_mut() = segments;
        *self.lines.borrow_mut() = lines;
        *self.search_rows.borrow_mut() = search_rows;
        // An edit rebuilds the list. Keep the bar on the same line if it can.
        if self.find_bar.is_visible() {
            self.apply_find(false);
        }
        self.player.set_chapters(
            chapters
                .iter()
                .map(|c| (c.start_ms, c.title.clone()))
                .collect(),
        );
        self.show_chapters(&chapters);
        self.show_speakers();
        let meta = meta.join("  ·  ");
        self.done_meta.set_label(&meta);
        self.done_meta.set_visible(!meta.is_empty());
    }

    fn default_agent(&self) -> Option<Agent> {
        self.agent.get_or_init(agent::default_agent).clone()
    }

    /// Chapters need an agent and a meeting long enough to divide.
    fn can_have_chapters(&self) -> bool {
        let long_enough = self
            .lines
            .borrow()
            .last()
            .is_some_and(|(start, _, _)| *start >= chapters::MIN_DURATION_MS);
        long_enough && self.default_agent().is_some()
    }

    /// Fills the chapters list and its header. The list shows whatever
    /// chapters the meeting has; the button only exists with an agent.
    fn show_chapters(&self, list: &[Chapter]) {
        while let Some(row) = self.chapters_list.row_at_index(0) {
            self.chapters_list.remove(&row);
        }
        for chapter in list {
            let row = adw::ActionRow::builder()
                .title(&chapter.title)
                .title_lines(2)
                .activatable(true)
                .build();
            row.add_prefix(
                &gtk::Label::builder()
                    .label(chapters::clock(chapter.start_ms))
                    .css_classes(["numeric", "dim-label"])
                    .build(),
            );
            self.chapters_list.append(&row);
        }
        *self.chapter_starts.borrow_mut() = list.iter().map(|c| c.start_ms).collect();
        self.update_chapters_header(!list.is_empty());
    }

    fn update_chapters_header(&self, has_chapters: bool) {
        let agent = self.default_agent();
        self.chapters_group
            .set_visible(agent.is_some() || has_chapters);
        self.chapters_button.set_visible(agent.is_some());
        self.chapters_spinner.set_visible(self.generating.get());
        self.chapters_button
            .set_sensitive(!self.generating.get() && self.can_have_chapters());
        self.chapters_button
            .set_label(if has_chapters { "Redo" } else { "Generate" });
        let description = match &agent {
            Some(agent) if self.generating.get() => {
                format!("Writing chapters with {}…", agent.name)
            }
            Some(agent) if has_chapters => format!("Made with {}", agent.name),
            Some(agent) if self.can_have_chapters() => {
                format!("Let {} divide the meeting into chapters", agent.name)
            }
            Some(_) => "For meetings of three minutes or more".to_owned(),
            None => String::new(),
        };
        self.chapters_group
            .set_description((!description.is_empty()).then_some(description.as_str()));
    }

    /// Asks the default agent for chapters in the background. The meeting
    /// works the same without them; a failure only shows up in the header.
    fn generate_chapters(self: &Rc<Self>) {
        if self.generating.get() {
            self.run_automatic_actions();
            return;
        }
        let (Some(agent), Some(dir)) = (self.default_agent(), self.result_dir.borrow().clone())
        else {
            self.run_automatic_actions();
            return;
        };
        let lines = self.lines.borrow().clone();
        self.generating.set(true);
        self.update_chapters_header(!self.chapter_starts.borrow().is_empty());
        let this = self.clone();
        glib::spawn_future_local(async move {
            let asked = agent.clone();
            let result = gio::spawn_blocking(move || {
                let lines: Vec<chapters::Line> = lines
                    .iter()
                    .map(|(start_ms, speaker, text)| chapters::Line {
                        start_ms: *start_ms,
                        speaker,
                        text,
                    })
                    .collect();
                chapters::generate(&asked, &lines)
            })
            .await
            .unwrap_or_else(|_| Err("the agent stopped unexpectedly".into()));
            this.generating.set(false);
            // The folder may have been renamed meanwhile (same timestamp
            // prefix); a different meeting is left alone.
            let prefix = |p: &std::path::Path| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.chars().take(12).collect::<String>())
            };
            let current = this.result_dir.borrow().clone();
            let Some(dir) = current.filter(|d| prefix(d) == prefix(&dir)) else {
                this.run_automatic_actions();
                return;
            };
            match result {
                Ok(list) => this.store_chapters(&dir, &list, &agent),
                Err(message) => {
                    eprintln!("{APP_NAME}: chapters: {message}");
                    this.update_chapters_header(!this.chapter_starts.borrow().is_empty());
                    this.chapters_group
                        .set_description(Some(&format!("{} could not make chapters", agent.name)));
                }
            }
            // With or without chapters, the meeting is now as complete as it
            // gets: the automatic actions can have it.
            this.run_automatic_actions();
        });
    }

    fn store_chapters(self: &Rc<Self>, dir: &std::path::Path, list: &[Chapter], agent: &Agent) {
        if let Some(manifest) = self.manifest.borrow_mut().as_mut() {
            manifest.chapters = list.to_vec();
            manifest.chapters_by = Some(agent.id.clone());
            let _ = meeting::write(dir, manifest);
        }
        let transcript = dir.join("transcript.md");
        if let Ok(text) = std::fs::read_to_string(&transcript) {
            let _ = std::fs::write(&transcript, chapters::apply_to_markdown(&text, list));
        }
        let text = std::fs::read_to_string(&transcript).ok();
        self.show_transcript(text.as_deref(), None);
        self.toast(&format!("{} chapters added", list.len()));
    }

    /// One paragraph of the transcript: time, speaker and text in columns, with
    /// edit, swap-speaker and delete buttons that show on hover.
    fn paragraph_row(
        self: &Rc<Self>,
        paragraph: &Paragraph,
        speaker_index: usize,
        time_width: i32,
        speaker_width: i32,
    ) -> gtk::ListBoxRow {
        let time = gtk::Label::builder()
            .label(&paragraph.time)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .width_request(time_width)
            .css_classes(["numeric", "dim-label"])
            .build();
        let speaker = gtk::Label::builder()
            .label(&paragraph.speaker)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .width_request(speaker_width)
            .css_classes([format!("speaker-{}", speaker_index % 6)])
            .build();
        let text = gtk::Label::builder()
            .label(&paragraph.text)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .natural_wrap_mode(gtk::NaturalWrapMode::None)
            .build();
        let action = |icon: &str, tooltip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .valign(gtk::Align::Start)
                .css_classes(["flat", "circular"])
                .build()
        };
        let edit = action("document-edit-symbolic", "Edit this line");
        let swap = action("object-flip-horizontal-symbolic", "Next speaker");
        let delete = action("user-trash-symbolic", "Delete this line");
        let actions = gtk::Box::builder()
            .spacing(2)
            .valign(gtk::Align::Start)
            .css_classes(["row-actions"])
            .build();
        actions.append(&edit);
        actions.append(&swap);
        actions.append(&delete);

        let content = gtk::Box::builder().spacing(14).build();
        content.append(&time);
        content.append(&speaker);
        content.append(&text);
        content.append(&actions);
        let row = gtk::ListBoxRow::builder()
            .child(&content)
            .tooltip_text(format!("Play from {}", paragraph.time))
            .build();
        row.set_cursor_from_name(Some("pointer"));

        let (sources, stamp, who) = (
            paragraph.sources.clone(),
            paragraph.time.clone(),
            paragraph.speaker.clone(),
        );
        let weak = Rc::downgrade(self);
        let delete_sources = sources.clone();
        delete.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.delete_paragraph(&delete_sources);
            }
        });
        let weak = Rc::downgrade(self);
        let (swap_sources, swap_who) = (sources.clone(), who.clone());
        swap.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.swap_speaker(&swap_sources, &swap_who);
            }
        });
        let weak = Rc::downgrade(self);
        let original = paragraph.text.clone();
        let (edit_content, edit_speaker, edit_row) =
            (content.clone(), speaker.clone(), row.clone());
        edit.connect_clicked(move |button| {
            let Some(r) = weak.upgrade() else { return };
            r.start_editing(
                &edit_row,
                &edit_content,
                &edit_speaker,
                &text,
                button,
                &sources,
                &stamp,
                &who,
                &original,
            );
        });
        row
    }

    /// Swaps the text of a row for an editor: Enter saves, Escape cancels,
    /// leaving the field saves too.
    #[allow(clippy::too_many_arguments)]
    fn start_editing(
        self: &Rc<Self>,
        row: &gtk::ListBoxRow,
        content: &gtk::Box,
        speaker: &gtk::Label,
        label: &gtk::Label,
        edit_button: &gtk::Button,
        sources: &[usize],
        time: &str,
        who: &str,
        original: &str,
    ) {
        row.set_activatable(false);
        row.add_css_class("editing");
        // Start at the size of the text it replaces: without a width the first
        // frame wraps every letter and the field flashes up very tall.
        let (width, height) = (label.width().max(120), label.height().max(1));
        label.set_visible(false);
        edit_button.set_sensitive(false);
        let editor = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .hexpand(true)
            .valign(gtk::Align::Start)
            .width_request(width)
            .height_request(height)
            .css_classes(["transcript-editor"])
            .build();
        editor.buffer().set_text(original);
        content.insert_child_after(&editor, Some(speaker));
        editor.grab_focus();

        let done = Rc::new(Cell::new(false));
        let finish = {
            let (weak, editor, label, row, button) = (
                Rc::downgrade(self),
                editor.clone(),
                label.clone(),
                row.clone(),
                edit_button.clone(),
            );
            let (sources, time, who, original) = (
                sources.to_vec(),
                time.to_owned(),
                who.to_owned(),
                original.to_owned(),
            );
            let done = done.clone();
            move |save: bool| {
                if done.replace(true) {
                    return;
                }
                let buffer = editor.buffer();
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if let Some(parent) = editor.parent().and_downcast::<gtk::Box>() {
                    parent.remove(&editor);
                }
                label.set_visible(true);
                row.set_activatable(true);
                row.remove_css_class("editing");
                button.set_sensitive(true);
                if save
                    && text != original
                    && let Some(r) = weak.upgrade()
                {
                    // Deferred: saving rebuilds the list this row lives in.
                    let (sources, time, who) = (sources.clone(), time.clone(), who.clone());
                    glib::idle_add_local_once(move || {
                        if text.is_empty() {
                            r.delete_paragraph(&sources);
                        } else {
                            r.edit_paragraph(&sources, &time, &who, &text);
                        }
                    });
                }
            }
        };
        let finish = Rc::new(finish);
        let keys = gtk::EventControllerKey::new();
        let on_key = finish.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| match key {
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter
                if !modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) =>
            {
                on_key(true);
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Escape => {
                on_key(false);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        editor.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let on_leave = finish.clone();
        focus.connect_leave(move |_| on_leave(true));
        editor.add_controller(focus);
    }

    /// Changes transcript.md line by line and shows the result, keeping the
    /// scroll position. Returns the text as it was, for undo.
    fn rewrite_transcript(
        self: &Rc<Self>,
        change: impl FnOnce(&mut Vec<String>),
    ) -> Option<String> {
        let dir = self.result_dir.borrow().clone()?;
        let path = dir.join("transcript.md");
        let before = std::fs::read_to_string(&path).ok()?;
        let mut lines: Vec<String> = before.lines().map(str::to_owned).collect();
        change(&mut lines);
        // Removing lines leaves their blank separators behind; keep one.
        let mut tidy: Vec<String> = Vec::new();
        for line in lines {
            if line.trim().is_empty() && tidy.last().is_some_and(|l| l.trim().is_empty()) {
                continue;
            }
            tidy.push(line);
        }
        let after = tidy.join("\n") + "\n";
        if std::fs::write(&path, &after).is_err() {
            self.toast("Could not save the transcript");
            return None;
        }
        self.redraw_transcript(&after);
        Some(before)
    }

    fn redraw_transcript(self: &Rc<Self>, markdown: &str) {
        let adjustment = self.transcript_scroll.vadjustment();
        let scroll = adjustment.value();
        self.show_transcript(Some(markdown), None);
        glib::idle_add_local_once(move || adjustment.set_value(scroll));
    }

    fn edit_paragraph(self: &Rc<Self>, sources: &[usize], time: &str, who: &str, text: &str) {
        let (first, rest) = match sources.split_first() {
            Some(split) => split,
            None => return,
        };
        let (first, rest) = (*first, rest.to_vec());
        let line = format!("**[{time}] {who}:** {text}");
        if self
            .rewrite_transcript(move |lines| {
                if let Some(slot) = lines.get_mut(first) {
                    *slot = line;
                }
                for index in rest.into_iter().rev() {
                    if index < lines.len() {
                        lines.remove(index);
                    }
                }
            })
            .is_some()
        {
            self.toast("Saved");
        }
    }

    fn delete_paragraph(self: &Rc<Self>, sources: &[usize]) {
        let mut sources = sources.to_vec();
        sources.sort_unstable();
        let Some(before) = self.rewrite_transcript(move |lines| {
            for index in sources.into_iter().rev() {
                if index < lines.len() {
                    lines.remove(index);
                }
            }
        }) else {
            return;
        };
        let toast = adw::Toast::builder()
            .title("Line deleted")
            .button_label("Undo")
            .build();
        let weak = Rc::downgrade(self);
        toast.connect_button_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            if let Some(dir) = r.result_dir.borrow().clone()
                && std::fs::write(dir.join("transcript.md"), &before).is_ok()
            {
                r.redraw_transcript(&before);
            }
        });
        self.toasts.add_toast(toast);
    }

    /// Gives a paragraph to the other speaker, for when whisper guessed wrong.
    fn swap_speaker(self: &Rc<Self>, sources: &[usize], who: &str) {
        let speakers = self
            .manifest
            .borrow()
            .as_ref()
            .map(|m| m.people())
            .unwrap_or_default();
        if speakers.len() < 2 {
            return;
        }
        let next = speakers
            .iter()
            .position(|s| s == who)
            .map_or(0, |i| (i + 1) % speakers.len());
        let other = speakers[next].clone();
        let sources = sources.to_vec();
        self.rewrite_transcript(move |lines| {
            for index in sources {
                if let Some(line) = lines.get_mut(index)
                    && let Some((time, _, text)) = parse_segment(line)
                {
                    *line = format!("**[{time}] {other}:** {text}");
                }
            }
        });
    }

    /// Ctrl+F on the done page: show the bar and select whatever was typed last.
    fn open_find(&self) {
        if self.state.get() != State::Done {
            return;
        }
        let opening = !self.find_bar.is_visible();
        self.find_bar.set_visible(true);
        self.find_entry.grab_focus();
        self.find_entry.select_region(0, -1);
        if opening {
            self.apply_find(false);
        }
    }

    /// Hides the bar and the marks. The query stays, so the next Ctrl+F continues it.
    fn close_find(&self) {
        self.dismiss_find();
        if self.state.get() == State::Done {
            // Back to the copy button: focus in the name field would rename the meeting.
            self.copy_button.grab_focus();
        }
    }

    fn dismiss_find(&self) {
        if !self.find_bar.is_visible() {
            return;
        }
        self.find_bar.set_visible(false);
        self.clear_find_marks();
    }

    /// Marks every row that contains the query. `jump_to_first` is for a new
    /// query; a rebuilt transcript keeps the row it was on.
    fn apply_find(&self, jump_to_first: bool) {
        let keep = self.find_hits.borrow().get(self.find_at.get()).copied();
        self.clear_find_marks();
        let query = self.find_entry.text();
        let hits = crate::find::hits(self.search_rows.borrow().as_slice(), query.as_str());
        let count = hits.len();
        let at = if count == 0 || jump_to_first {
            0
        } else if let Some(row) = keep {
            hits.iter().position(|hit| *hit == row).unwrap_or(0)
        } else {
            0
        };
        for index in &hits {
            if let Some(row) = self.transcript_list.row_at_index(*index) {
                row.add_css_class("match");
            }
        }
        *self.find_hits.borrow_mut() = hits;
        *self.find_query.borrow_mut() = query.trim().to_owned();
        self.find_at.set(at);
        let any = count > 0;
        self.find_prev.set_sensitive(any);
        self.find_next.set_sensitive(any);
        if query.trim().is_empty() {
            self.find_count.set_label("");
            return;
        }
        if count == 0 {
            self.find_count.set_label("No matches");
            return;
        }
        self.show_hit(at);
    }

    /// Next or previous match. A query that has not been applied yet jumps
    /// to its first match instead of stepping past it.
    fn find_again(&self, delta: i32) {
        let query = self.find_entry.text();
        if query.trim() != self.find_query.borrow().as_str() {
            self.apply_find(true);
            return;
        }
        self.find_step(delta);
    }

    /// Moves to another match. `delta` is 1 or -1, and wraps around.
    fn find_step(&self, delta: i32) {
        let count = self.find_hits.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let at = (self.find_at.get() as i32 + delta).rem_euclid(count) as usize;
        self.show_hit(at);
    }

    fn show_hit(&self, at: usize) {
        let hits = self.find_hits.borrow().clone();
        if let Some(row) = hits
            .get(self.find_at.get())
            .and_then(|index| self.transcript_list.row_at_index(*index))
        {
            row.remove_css_class("found");
        }
        self.find_at.set(at);
        let Some(&index) = hits.get(at) else {
            return;
        };
        self.find_count
            .set_label(&format!("{} of {}", at + 1, hits.len()));
        if let Some(row) = self.transcript_list.row_at_index(index) {
            row.add_css_class("found");
            self.scroll_row_into_view(&row);
        }
    }

    fn clear_find_marks(&self) {
        for index in self.find_hits.borrow().clone() {
            if let Some(row) = self.transcript_list.row_at_index(index) {
                row.remove_css_class("match");
                row.remove_css_class("found");
            }
        }
    }

    /// Brings `row` into the transcript, a third of the way down, once it has a size.
    fn scroll_row_into_view(&self, row: &gtk::ListBoxRow) {
        let row = row.clone();
        let list = self.transcript_list.clone();
        let scroll = self.transcript_scroll.clone();
        glib::idle_add_local_once(move || {
            let Some(bounds) = row.compute_bounds(&list) else {
                return;
            };
            let adjustment = scroll.vadjustment();
            let (top, page) = (adjustment.value(), adjustment.page_size());
            let (y, height) = (f64::from(bounds.y()), f64::from(bounds.height()));
            if y < top || y + height > top + page {
                adjustment.set_value((y - page * 0.3).max(0.0));
            }
        });
    }

    /// Marks the transcript line that is playing at `ms`, and its chapter.
    fn highlight(&self, ms: i64) {
        let chapter = self
            .chapter_starts
            .borrow()
            .iter()
            .rposition(|start| *start <= ms);
        let selected = self
            .chapters_list
            .selected_row()
            .map(|r| r.index() as usize);
        if chapter != selected {
            match chapter.and_then(|i| self.chapters_list.row_at_index(i as i32)) {
                Some(row) => self.chapters_list.select_row(Some(&row)),
                None => self.chapters_list.unselect_all(),
            }
        }
        let line = self
            .segments
            .borrow()
            .iter()
            .rev()
            .find(|(_, start)| *start <= ms)
            .map(|(line, _)| *line)
            .unwrap_or(-1);
        let previous = self.current_line.replace(line);
        if line == previous {
            return;
        }
        if let Some(row) = self.transcript_list.row_at_index(previous) {
            row.remove_css_class("current");
        }
        let Some(row) = self.transcript_list.row_at_index(line) else {
            return;
        };
        row.add_css_class("current");
        // Follow along while playing, keeping the row a third from the top.
        if self.player.is_playing()
            && let Some(bounds) = row.compute_bounds(&self.transcript_list)
        {
            let adjustment = self.transcript_scroll.vadjustment();
            let (top, page) = (adjustment.value(), adjustment.page_size());
            let (y, h) = (f64::from(bounds.y()), f64::from(bounds.height()));
            if y < top || y + h > top + page {
                adjustment.set_value((y - page * 0.3).max(0.0));
            }
        }
    }

    /// One name row per person under the meeting name. Speakers given the same
    /// name share a row: that is how two voices that were one person are merged.
    fn show_speakers(self: &Rc<Self>) {
        for row in self.speaker_rows.borrow_mut().drain(..) {
            self.done_group.remove(&row);
        }
        let Some(manifest) = self.manifest.borrow().clone() else {
            return;
        };
        let mut rows = Vec::new();
        let labels = manifest.default_labels();
        for name in manifest.people() {
            let i = manifest
                .speakers
                .iter()
                .position(|s| *s == name)
                .unwrap_or(0);
            let side = labels.get(i).and_then(|l| meeting::side_of(l));
            let title = match (manifest.imported.is_some(), side) {
                (true, _) | (false, None) => format!("Speaker {}", i + 1),
                (false, Some((side, n))) => {
                    let place = if side == meeting::DEFAULT_YOU {
                        "the microphone"
                    } else {
                        "the computer audio"
                    };
                    if n == 0 {
                        format!("Speaker on {place}")
                    } else {
                        format!("Speaker {n} on {place}")
                    }
                }
            };
            let row = adw::EntryRow::builder()
                .title(title)
                .text(&name)
                .show_apply_button(true)
                .build();
            let weak = Rc::downgrade(self);
            row.connect_apply(move |_| {
                if let Some(r) = weak.upgrade() {
                    r.apply_speakers();
                }
            });
            let focus = gtk::EventControllerFocus::new();
            let weak = Rc::downgrade(self);
            focus.connect_leave(move |_| {
                if let Some(r) = weak.upgrade() {
                    // Deferred: saving rebuilds these rows.
                    glib::idle_add_local_once(move || r.apply_speakers());
                }
            });
            row.add_controller(focus);
            self.done_group.add(&row);
            rows.push(row);
        }
        *self.speaker_rows.borrow_mut() = rows;
    }

    /// Renames the speakers in transcript.md and the manifest. A name that
    /// another speaker already has merges the two. Your own name is also
    /// remembered for the next recordings.
    fn apply_speakers(self: &Rc<Self>) {
        if self.state.get() != State::Done {
            return;
        }
        let Some(dir) = self.result_dir.borrow().clone() else {
            return;
        };
        let Some(manifest) = self.manifest.borrow().clone() else {
            return;
        };
        let labels = manifest.default_labels();
        let people = manifest.people();
        let typed: Vec<String> = self
            .speaker_rows
            .borrow()
            .iter()
            .map(|row| row.text().trim().replace(['*', '[', ']', ':', '\n'], ""))
            .collect();
        if typed.len() != people.len() {
            return;
        }
        let names: Vec<String> = manifest
            .speakers
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let row = people.iter().position(|p| p == name).unwrap_or(0);
                if typed[row].is_empty() {
                    labels
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| format!("Speaker {}", i + 1))
                } else {
                    typed[row].clone()
                }
            })
            .collect();
        if names.is_empty() || names == manifest.speakers {
            return;
        }
        let merged = names.iter().collect::<std::collections::HashSet<_>>().len() < people.len();
        let mut renames: Vec<(String, String)> = Vec::new();
        for (from, to) in manifest.speakers.iter().zip(&names) {
            if from != to && !renames.iter().any(|(f, _)| f == from) {
                renames.push((from.clone(), to.clone()));
            }
        }
        let transcript = dir.join("transcript.md");
        if let Ok(text) = std::fs::read_to_string(&transcript) {
            let _ = std::fs::write(&transcript, meeting::relabel_all(&text, &renames));
        }
        let you_changed = manifest.imported.is_none() && names.first() != manifest.speakers.first();
        if let Some(m) = self.manifest.borrow_mut().as_mut() {
            m.speakers = names.clone();
            let _ = meeting::write(&dir, m);
        }
        if you_changed && let Some(you) = names.first() {
            settings::save_your_name(you);
        }
        let text = std::fs::read_to_string(&transcript).ok();
        self.redraw_transcript(text.as_deref().unwrap_or(""));
        if merged {
            self.toast("Merged into one speaker");
        } else {
            self.toast("Saved");
        }
    }

    /// Renames the meeting folder after the name and rewrites the transcript heading.
    fn apply_title(&self) {
        if self.state.get() != State::Done {
            return;
        }
        let Some(current) = self.result_dir.borrow().clone() else {
            return;
        };
        let title = self.title();
        let target = output_dir(self.started_at.get(), &title);
        // "202609241400 Weekly 2" is still the folder of "Weekly": an import
        // got a number when the name was taken. Only a new name renames.
        let renamed = !folder_is_for(&current, &target);
        if renamed {
            if target.exists() {
                self.toast("A meeting folder with that name already exists");
                return;
            }
            if let Err(e) = std::fs::rename(&current, &target) {
                self.toast(&format!("Could not rename the folder: {e}"));
                return;
            }
            self.player.relocate(&current, &target);
            *self.result_dir.borrow_mut() = Some(target.clone());
            self.shared.lock().unwrap().folder = self.folder_text();
            self.render();
        }
        // The folder the meeting is in now: renamed, or the numbered one it had.
        let target = if renamed { target } else { current };
        let mut retitled = false;
        if let Some(manifest) = self.manifest.borrow_mut().as_mut()
            && manifest.title != title
        {
            manifest.title = title.clone();
            let _ = meeting::write(&target, manifest);
            retitled = true;
        }
        if !renamed && !retitled {
            return;
        }
        let transcript = target.join("transcript.md");
        if let Ok(text) = std::fs::read_to_string(&transcript)
            && let Some(rest) = text.strip_prefix("# ")
        {
            let body = rest.split_once('\n').map(|(_, body)| body).unwrap_or("");
            let _ = std::fs::write(&transcript, format!("# {title}\n{body}"));
        }
        self.toast("Saved");
    }

    /// Hidden, the window no longer keeps the machine awake; see `keep_awake`.
    fn close_when_done(&self) {
        self.quit_when_done.set(true);
        self.window.set_visible(false);
    }

    fn confirm_close_recording(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Still recording"),
            Some(
                "Closing stops the meeting. The audio is saved and transcribed in the background, \
                 then the window closes. Hidden, the window no longer keeps the machine awake \
                 while the transcript finishes.",
            ),
        );
        dialog.add_response("keep", "Keep recording");
        dialog.add_response("stop", "Stop and close");
        dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("keep");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "stop" {
                this.stop();
                this.close_when_done();
            }
        });
        dialog.present(Some(&self.window));
    }

    fn confirm_close_transcribing(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Still transcribing"),
            Some(
                "The audio is already saved. The transcript is not finished yet. \
                 Hidden, the window no longer keeps the machine awake while the transcript finishes.",
            ),
        );
        dialog.add_response("keep", "Keep open");
        dialog.add_response("later", "Close when done");
        dialog.add_response("cancel", "Cancel transcription");
        dialog.set_response_appearance("cancel", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("keep");
        let this = self.clone();
        dialog.connect_response(None, move |_, response| match response {
            "later" => this.close_when_done(),
            "cancel" => {
                if let Some(abort) = this.abort.borrow().as_ref() {
                    abort.store(true, Ordering::Relaxed);
                }
                this.close_when_done();
            }
            _ => {}
        });
        dialog.present(Some(&self.window));
    }
}

/// Whether `folder` is `expected`, or `expected` with a number after it.
fn folder_is_for(folder: &std::path::Path, expected: &std::path::Path) -> bool {
    let (Some(name), Some(want)) = (
        folder.file_name().and_then(|n| n.to_str()),
        expected.file_name().and_then(|n| n.to_str()),
    ) else {
        return folder == expected;
    };
    folder.parent() == expected.parent()
        && name.strip_prefix(want).is_some_and(|rest| {
            rest.is_empty()
                || rest
                    .strip_prefix(' ')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

fn output_dir(started_at: i64, title: &str) -> PathBuf {
    let stamp = glib::DateTime::from_unix_local(started_at)
        .and_then(|t| t.format("%Y%m%d%H%M"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    glib::home_dir()
        .join("Documents/Meetings")
        .join(format!("{stamp} {}", safe_name(title)))
}

fn row_count(list: &gtk::ListBox) -> i32 {
    let mut count = 0;
    while list.row_at_index(count).is_some() {
        count += 1;
    }
    count
}

/// Width in pixels of `text` in the font of `widget`.
fn text_width(widget: &impl IsA<gtk::Widget>, text: &str) -> i32 {
    widget.create_pango_layout(Some(text)).pixel_size().0
}

fn chapter_row(title: &str, start_ms: i64) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["chapter-heading"])
        .build();
    let row = gtk::ListBoxRow::builder()
        .child(&label)
        .tooltip_text(format!("Play from {}", chapters::clock(start_ms)))
        .css_classes(["chapter"])
        .build();
    row.set_cursor_from_name(Some("pointer"));
    row
}

fn note_row(text: &str) -> gtk::ListBoxRow {
    gtk::ListBoxRow::builder()
        .child(
            &gtk::Label::builder()
                .label(text)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label"])
                .build(),
        )
        .activatable(false)
        .build()
}

/// Transcript lines grouped into one paragraph per turn for reading. Older
/// transcripts have a line per sentence; a line by the same speaker that
/// follows closely on the previous one joins it.
#[derive(Default)]
struct Paragraphs {
    list: Vec<Paragraph>,
}

struct Paragraph {
    /// The lines of transcript.md this paragraph was made of.
    sources: Vec<usize>,
    time: String,
    start_ms: i64,
    speaker: String,
    text: String,
    /// Start and length of the last line added, to estimate when it ended.
    last_start_ms: i64,
    last_chars: usize,
}

impl Paragraphs {
    const MAX_MS: i64 = 90_000;

    fn add(&mut self, time: &str, speaker: &str, text: &str, source: usize) {
        let start_ms = clock_to_ms(time);
        if let Some(last) = self.list.last_mut() {
            // About 15 characters a second, plus a pause of up to 3 seconds.
            let ended = last.last_start_ms + last.last_chars as i64 * 1000 / 15;
            if last.speaker == speaker
                && start_ms - ended.max(last.last_start_ms + 3000) <= 3000
                && start_ms - last.start_ms < Self::MAX_MS
            {
                last.text.push(' ');
                last.text.push_str(text);
                last.last_start_ms = start_ms;
                last.last_chars = text.chars().count();
                last.sources.push(source);
                return;
            }
        }
        self.list.push(Paragraph {
            sources: vec![source],
            time: time.to_owned(),
            start_ms,
            speaker: speaker.to_owned(),
            text: text.to_owned(),
            last_start_ms: start_ms,
            last_chars: text.chars().count(),
        });
    }
}

/// Resizes this app's window through Hyprland, keeping it where it is.
/// Does nothing outside Hyprland.
/// Resizes this window: the one at `address` when it is known, else the one
/// that is `current` in size now. With more windows open, the others keep theirs.
fn hyprland_resize((width, height): (i32, i32), current: (i32, i32), address: Option<String>) {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    let pid = std::process::id();
    std::thread::spawn(move || {
        // Right after start the window may not be mapped yet; wait for it a little.
        let mut window = None;
        for _ in 0..30 {
            window = match &address {
                Some(address) => client_at(address),
                None => own_window(pid, current),
            };
            if window.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let Some(window) = window else { return };
        let Some(address) = window["address"].as_str().map(str::to_owned) else {
            return;
        };
        hyprctl_dispatch(&format!(
            "hl.dsp.window.resize({{ x = {width}, y = {height}, window = \"address:{address}\" }})"
        ));
        // Hyprland grows a floating window around its centre; a strip that was
        // dragged into a corner would then stick out. Bring it back on screen.
        std::thread::sleep(std::time::Duration::from_millis(150));
        if let Some((x, y)) = client_at(&address).and_then(|w| on_screen(&w)) {
            hyprctl_dispatch(&format!(
                "hl.dsp.window.move({{ x = {x}, y = {y}, window = \"address:{address}\" }})"
            ));
        }
    });
}

fn hyprctl_json(what: &str) -> Option<serde_json::Value> {
    let out = std::process::Command::new("hyprctl")
        .args([what, "-j"])
        .output()
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

fn hyprctl_dispatch(call: &str) {
    let _ = std::process::Command::new("hyprctl")
        .arg("dispatch")
        .arg(call)
        .output();
}

/// This process's floating window that is `size` now, as `hyprctl clients -j`
/// describes it.
fn own_window(pid: u32, size: (i32, i32)) -> Option<serde_json::Value> {
    pick_window(hyprctl_json("clients")?.as_array()?, pid, size).cloned()
}

/// Of this process's floating windows, the one of `size`; with more of that
/// size, or none (a window not on screen yet), the one focused last.
fn pick_window(clients: &[serde_json::Value], pid: u32, size: (i32, i32)) -> Option<&serde_json::Value> {
    let mut own: Vec<&serde_json::Value> = clients
        .iter()
        .filter(|c| c["pid"].as_u64() == Some(u64::from(pid)) && c["floating"] == true)
        .collect();
    own.sort_by_key(|c| c["focusHistoryID"].as_i64().unwrap_or(i64::MAX));
    let sized = |c: &&&serde_json::Value| {
        c["size"][0].as_i64() == Some(i64::from(size.0)) && c["size"][1].as_i64() == Some(i64::from(size.1))
    };
    own.iter().find(sized).or(own.first()).copied()
}

/// The first of this process's floating windows, most recently focused
/// first, whose address is not in `taken`.
fn unclaimed_window(clients: &[serde_json::Value], pid: u32, taken: &[String]) -> Option<String> {
    let mut own: Vec<&serde_json::Value> = clients
        .iter()
        .filter(|c| c["pid"].as_u64() == Some(u64::from(pid)) && c["floating"] == true)
        .collect();
    own.sort_by_key(|c| c["focusHistoryID"].as_i64().unwrap_or(i64::MAX));
    own.iter()
        .filter_map(|c| c["address"].as_str())
        .find(|address| !taken.iter().any(|t| t == address))
        .map(str::to_owned)
}

/// A window by its Hyprland address.
fn client_at(address: &str) -> Option<serde_json::Value> {
    hyprctl_json("clients")?
        .as_array()?
        .iter()
        .find(|c| c["address"].as_str() == Some(address))
        .cloned()
}

/// Where the window has to go to be fully visible on its monitor, outside the
/// bar and with a small margin; None when it already is.
fn on_screen(window: &serde_json::Value) -> Option<(i64, i64)> {
    const MARGIN: i64 = 12;
    let (x, y) = (window["at"][0].as_i64()?, window["at"][1].as_i64()?);
    let (w, h) = (window["size"][0].as_i64()?, window["size"][1].as_i64()?);
    let monitors = hyprctl_json("monitors")?;
    let monitor = monitors
        .as_array()?
        .iter()
        .find(|m| m["id"].as_i64() == window["monitor"].as_i64())?;
    let scale = monitor["scale"].as_f64().unwrap_or(1.0).max(0.1);
    let reserved = |i: usize| monitor["reserved"][i].as_i64().unwrap_or(0);
    let left = monitor["x"].as_i64()? + reserved(0) + MARGIN;
    let top = monitor["y"].as_i64()? + reserved(1) + MARGIN;
    let right =
        monitor["x"].as_i64()? + (monitor["width"].as_f64()? / scale) as i64 - reserved(2) - MARGIN;
    let bottom = monitor["y"].as_i64()? + (monitor["height"].as_f64()? / scale) as i64
        - reserved(3)
        - MARGIN;
    let nx = x.min(right - w).max(left);
    let ny = y.min(bottom - h).max(top);
    (nx != x || ny != y).then_some((nx, ny))
}

/// Fills in a meeting name that nobody typed: without the apply button,
/// which would ask to confirm a change that was never made.
fn set_name(row: &adw::EntryRow, text: &str) {
    row.set_show_apply_button(false);
    row.set_text(text);
    row.set_show_apply_button(true);
}

/// Repaints a widget and everything in it, for custom drawing after a theme switch.
fn redraw(widget: &impl IsA<gtk::Widget>) {
    widget.queue_draw();
    let mut child = widget.first_child();
    while let Some(current) = child {
        redraw(&current);
        child = current.next_sibling();
    }
}

/// Decodes any audio or video file with ffmpeg into the meeting folder: the
/// levelled `audio.ogg` to listen to and `.tracks/source.ogg` to transcribe
/// again. Returns the duration in seconds.
fn import_audio(
    source: &std::path::Path,
    out: &std::path::Path,
    staging: &std::path::Path,
) -> Result<i64, String> {
    std::fs::create_dir_all(out.join(export::TRACKS_DIR)).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(staging).map_err(|e| e.to_string())?;
    let raw = staging.join("mic.raw");
    let silence = staging.join("system.raw");
    let decoded = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-nostdin", "-i"])
        .arg(source)
        .args(["-vn", "-f", "s16le", "-ar", "48000", "-ac", "2"])
        .arg(&raw)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let bytes = std::fs::metadata(&raw).map(|m| m.len()).unwrap_or(0);
    if !decoded || bytes == 0 {
        let _ = std::fs::remove_dir_all(staging);
        return Err("this file has no audio ffmpeg can read".into());
    }
    let _ = std::fs::write(&silence, []);
    let listened = export_audio(&raw, &silence, out, Format::Mono);
    let kept = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-y", "-nostdin", "-f", "s16le", "-ar", "48000", "-ac", "2", "-i",
        ])
        .arg(&raw)
        .args(["-ac", "1", "-c:a", "libopus", "-b:a", "48k"])
        .arg(source_track(out))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(staging);
    if !listened || !kept {
        return Err("could not convert the audio".into());
    }
    Ok((bytes / (48_000 * 2 * 2)) as i64)
}

/// What is known about a recording in progress, next to its audio, so it can
/// be finished after a crash.
#[derive(Clone)]
struct RecordingNote {
    title: String,
    started_at: i64,
    format: Format,
    language: String,
}

fn write_recording_note(
    staging: &std::path::Path,
    title: &str,
    started_at: i64,
    format: Format,
    language: &str,
) {
    let note = serde_json::json!({
        "title": title,
        "started_at": started_at,
        "format": format.key(),
        "language": language,
    });
    let _ = std::fs::write(staging.join("recording.json"), note.to_string());
}

fn read_recording_note(staging: &std::path::Path) -> Option<RecordingNote> {
    let text = std::fs::read_to_string(staging.join("recording.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(RecordingNote {
        title: value["title"]
            .as_str()
            .filter(|t| !t.is_empty())?
            .to_owned(),
        started_at: value["started_at"].as_i64()?,
        format: Format::from_key(value["format"].as_str().unwrap_or("mono")),
        language: value["language"].as_str().unwrap_or("auto").to_owned(),
    })
}

/// Recording staging folders left behind, with some audio in them.
fn unfinished_recordings() -> Vec<PathBuf> {
    let root = glib::user_cache_dir().join(APP_NAME);
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        // Either track counts: a machine without a microphone still records
        // the computer audio.
        .filter(|dir| raw_duration(dir) > 0)
        .collect();
    found.sort();
    found
}

/// Recorded seconds in a staging folder, from the size of the longer raw track.
fn raw_duration(staging: &std::path::Path) -> i64 {
    let bytes = ["mic.raw", "system.raw"]
        .iter()
        .map(|name| std::fs::metadata(staging.join(name)).map_or(0, |m| m.len()))
        .max()
        .unwrap_or(0);
    (bytes / (48_000 * 2 * 2)) as i64
}

/// The speaker labels in transcript Markdown, in order of first appearance.
fn speakers_in(markdown: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in markdown.lines() {
        if let Some((_, speaker, _)) = parse_segment(line)
            && !found.iter().any(|f| f == speaker)
        {
            found.push(speaker.to_owned());
        }
    }
    found
}

/// `01:23` or `1:02:03` to milliseconds.
fn clock_to_ms(clock: &str) -> i64 {
    clock
        .split(':')
        .filter_map(|part| part.parse::<i64>().ok())
        .fold(0, |total, part| total * 60 + part)
        * 1000
}

/// Splits `**[01:23] You:** text` into its time, speaker and text.
fn parse_segment(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix("**[")?;
    let (time, rest) = rest.split_once("] ")?;
    let (speaker, text) = rest.split_once(":** ")?;
    Some((time, speaker, text.trim()))
}

/// The few styles libadwaita does not have: a see-through header bar over the
/// animation, and the transcript card.
fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "window.immersive { background: #0d0826; }
         window.immersive headerbar { background: transparent; box-shadow: none; color: #f8f5f2; }
         window.immersive headerbar button { color: #f8f5f2; }
         .transcript { background: transparent; padding: 8px; }
         .transcript row { padding: 7px 10px; border-radius: 8px; }
         .transcript row:hover { background: alpha(currentColor, 0.06); }
         .transcript row.current { background: alpha(@accent_bg_color, 0.25); }
         .transcript row.match { background: alpha(@accent_bg_color, 0.14); }
         .transcript row.found { background: alpha(@accent_bg_color, 0.36);
                                 box-shadow: inset 3px 0 0 @accent_color; }
         .transcript row.chapter { padding-top: 16px; }
         .transcript row.chapter:first-child { padding-top: 7px; }
         .chapter-heading { font-weight: 800; font-size: 1.08em; }
         .speaker-0, .speaker-1, .speaker-2, .speaker-3, .speaker-4, .speaker-5 { font-weight: 700; }
         .speaker-0 { color: #5a9cf0; } .speaker-1 { color: #f08a3a; } .speaker-2 { color: #57c27a; }
         .speaker-3 { color: #d066c8; } .speaker-4 { color: #3fc4cf; } .speaker-5 { color: #d9b53a; }
         .drop-hint { margin: 10px; border: 3px dashed @accent_color; border-radius: 14px;
                      background: alpha(@window_bg_color, 0.88); color: @accent_color; }
         .row-actions { opacity: 0; transition: opacity 120ms; }
         .transcript row:hover .row-actions, .transcript row.editing .row-actions { opacity: 1; }
         .row-actions button { min-height: 24px; min-width: 24px; padding: 2px; }
         .transcript-editor { background: alpha(currentColor, 0.06); border-radius: 6px; padding: 4px 6px; }
         .transcript-editor text { background: transparent; }
         .done-icon { color: @accent_color; }
         .player { padding: 6px 14px 6px 6px; }
         .notes { background: @window_bg_color; border-left: 1px solid alpha(currentColor, 0.1); }
         .notes-toggle { min-height: 24px; min-width: 24px; padding: 0 6px; border-radius: 6px;
                                border: 1px solid alpha(currentColor, 0.25); font-family: \"IBM Plex Mono\", \"JetBrainsMono Nerd Font\", monospace;
                                font-size: 13px; color: alpha(currentColor, 0.6); }
         .notes-toggle:hover { color: currentColor; }
         .webcam-preview { border-radius: 6px; background: black; margin: 6px 4px 6px 0; }
         .player-video { border-radius: 10px; background: black; }
         .webcam-pip { border-radius: 10px; box-shadow: 0 10px 30px alpha(black, 0.45); }
         .notes-text { font-family: \"IBM Plex Mono\", \"JetBrainsMono Nerd Font\", \"JetBrains Mono\", monospace;
                              font-size: 21px; line-height: 1.6; background: transparent; caret-color: currentColor; }
         .notes-text text { background: transparent; }
         .notes-footer { font-family: \"IBM Plex Mono\", \"JetBrainsMono Nerd Font\", \"JetBrains Mono\", monospace;
                                font-size: 11px; color: alpha(currentColor, 0.5); }
         .notes-button { min-height: 26px; min-width: 26px; padding: 2px; color: alpha(currentColor, 0.5); }
         .notes-button:hover { color: currentColor; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn safe_name(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() {
        "Meeting".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn format_elapsed(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// What a meter shows while the recording is paused: the levels at the
/// moment of pausing, dimmed and still.
type Frozen = Rc<RefCell<Option<Vec<f32>>>>;

fn meter(
    source: &Source,
    (key, fallback): (&'static str, (f64, f64, f64)),
    height: i32,
    frozen: &Frozen,
) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_height(height)
        .hexpand(true)
        .build();
    let source = source.clone();
    let frozen = frozen.clone();
    area.set_draw_func(move |area, cr, width, height| {
        let (r, g, b) = crate::theme::color(key, fallback);
        let (width, height) = (f64::from(width), f64::from(height));
        let paused = frozen.borrow().is_some();
        let levels = frozen.borrow().clone().unwrap_or_else(|| source.levels());
        let dim = if paused { 0.3 } else { 1.0 };
        let mid = height / 2.0;
        let step = width / HISTORY as f64;
        let bar = (step * 0.6).max(1.0);
        cr.set_source_rgba(r, g, b, 0.15);
        cr.rectangle(0.0, mid - 0.5, width, 1.0);
        let _ = cr.fill();
        for (i, peak) in levels.into_iter().enumerate() {
            let h = (to_meter(peak) * (height - 4.0)).max(1.0);
            cr.set_source_rgba(r, g, b, (0.35 + 0.65 * i as f64 / HISTORY as f64) * dim);
            cr.rectangle(i as f64 * step, mid - h / 2.0, bar, h);
            let _ = cr.fill();
        }
        // A pause sign in the middle of the big meters, so it is obvious.
        if paused && height >= 40.0 {
            let ink = area.color();
            let (ir, ig, ib) = (
                f64::from(ink.red()),
                f64::from(ink.green()),
                f64::from(ink.blue()),
            );
            let (cx, cy) = (width / 2.0, mid);
            cr.set_source_rgba(ir, ig, ib, 0.9);
            cr.rectangle(cx - 42.0, cy - 8.0, 5.0, 16.0);
            cr.rectangle(cx - 33.0, cy - 8.0, 5.0, 16.0);
            let _ = cr.fill();
            cr.select_font_face(
                "sans-serif",
                gtk::cairo::FontSlant::Normal,
                gtk::cairo::FontWeight::Bold,
            );
            cr.set_font_size(13.0);
            cr.move_to(cx - 20.0, cy + 5.0);
            let _ = cr.show_text("PAUSED");
        }
    });
    area
}

fn meter_block(name: &str, meter: &gtk::DrawingArea) -> gtk::Box {
    let block = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    block.append(
        &gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    block.append(
        &gtk::Frame::builder()
            .child(meter)
            .css_classes(["card"])
            .build(),
    );
    block
}

#[cfg(test)]
mod tests {
    use super::{State, keeps_awake, pick_window};

    #[test]
    fn the_machine_stays_awake_only_while_the_meeting_is_processed() {
        assert!(keeps_awake(State::Stopping));
        assert!(keeps_awake(State::Transcribing));
        assert!(!keeps_awake(State::Idle));
        assert!(!keeps_awake(State::Recording));
        assert!(!keeps_awake(State::Done));
    }

    #[test]
    fn a_resize_goes_to_the_window_it_is_for() {
        let window = |address: &str, w: i64, focus: i64| {
            serde_json::json!({ "address": address, "pid": 7, "floating": true, "size": [w, 700], "focusHistoryID": focus })
        };
        let clients = vec![window("0xa", 480, 1), window("0xb", 1100, 0), window("0xc", 480, 2)];
        let pick = |size| pick_window(&clients, 7, size).map(|c| c["address"].clone());
        assert_eq!(pick((1100, 700)), Some("0xb".into()));
        // Two of that size: the one focused last.
        assert_eq!(pick((480, 700)), Some("0xa".into()));
        // Not on screen yet: the focused one.
        assert_eq!(pick((0, 0)), Some("0xb".into()));
        assert_eq!(pick_window(&clients, 8, (480, 700)), None);
    }

    #[test]
    fn a_new_window_claims_the_address_no_other_window_has() {
        let window = |address: &str, focus: i64| {
            serde_json::json!({ "address": address, "pid": 7, "floating": true, "focusHistoryID": focus })
        };
        let clients = vec![window("0xa", 1), window("0xb", 0)];
        assert_eq!(super::unclaimed_window(&clients, 7, &[]), Some("0xb".into()));
        assert_eq!(super::unclaimed_window(&clients, 7, &["0xb".into()]), Some("0xa".into()));
        assert_eq!(super::unclaimed_window(&clients, 7, &["0xa".into(), "0xb".into()]), None);
    }
}
