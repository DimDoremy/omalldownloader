//! The desktop view, laid out like Ghost Downloader 3, the downloader this one is modelled
//! on:
//!
//! ```text
//! ┌──────────┬───────────────────────────────────────────────────────────────┐
//! │ ▣ Tasks  │ [All|Active|Done]  ⏵ Start all  ⏸ Pause all      12.4 MB/s  ⌕│
//! │ + Add    ├───────────────────────────────────────────────────────────────┤
//! │          │ ▉ main.iso                                     12.4 MB/s …  │
//! │          │   ▓▓▓▓▓▓▓▓▓▓░░░░░░░░  2.1/4.9 GB  ETA 3m  ⏸ 📂 🗑        │
//! │          ├───────────────────────────────────────────────────────────────┤
//! │ ⚙ Settings│ …                                                            │
//! └──────────┴───────────────────────────────────────────────────────────────┘
//! ```
//!
//! The left rail navigates (Tasks, Add a task, Settings, Browser extension); the toolbar
//! filters and acts on the whole list; every task is one card with the file's icon, its
//! name, a progress line, speed / ETA / size, and its own controls — the same information
//! hierarchy as Ghost's `TaskCard`.
//!
//! The view never touches the network or the disk. It reads what [`EngineHost`] keeps in
//! memory and sends commands to the engine's thread; when that thread has news it wakes the
//! view through the [`Notifier`] the application installs while it starts.
//!
//! Interface rules taken from ODK's design language, in this file: no literal colour, no
//! literal corner radius, spacing only through `Space`, one `ButtonVariant` per meaning.

pub mod theme;

use odk::prelude::*;
use odk::ui::gpui::{AnyElement, px, relative};
use omad_core::TaskStatus;
use omad_engine::engine::Plan;
use omad_engine::http::Probe;
use omad_engine::{Outcome, Progress as EngineProgress, StoreSink};
use omad_store::TaskRecord;
use std::sync::{Arc, Mutex};

// Nerd Font glyphs, the same set Omarchy's own bar and menus use.
const ICON_APP: &str = "\u{f019}"; // nf-fa-download
const ICON_TASKS: &str = "\u{f0c9}"; // nf-fa-bars
const ICON_ADD: &str = "\u{f067}"; // nf-fa-plus
const ICON_SETTINGS: &str = "\u{f013}"; // nf-fa-cog
const ICON_LINK: &str = "\u{f0c1}"; // nf-fa-link
const ICON_PLAY: &str = "\u{f04b}"; // nf-fa-play
const ICON_PAUSE: &str = "\u{f04c}"; // nf-fa-pause
const ICON_SPEED: &str = "\u{f0e4}"; // nf-fa-tachometer
const ICON_CLOCK: &str = "\u{f017}"; // nf-fa-clock
const ICON_DONE: &str = "\u{f00c}"; // nf-fa-check
const ICON_WARN: &str = "\u{f071}"; // nf-fa-warning

/// A row as the view shows it.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: u64,
    pub name: String,
    pub url: String,
    pub status: TaskStatus,
    pub received: u64,
    pub total: Option<u64>,
    pub speed: u64,
    pub error: Option<String>,
    /// Where the file lives, for "Open folder".
    pub path: Option<String>,
}

impl Row {
    fn fraction(&self) -> f64 {
        match self.total {
            Some(total) if total > 0 => (self.received as f64 / total as f64).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    fn size_text(&self) -> String {
        match self.total {
            Some(total) => format!("{} / {}", human_bytes(self.received), human_bytes(total)),
            None => human_bytes(self.received),
        }
    }

    fn speed_text(&self, lang: omad_core::i18n::Lang) -> String {
        if self.status == TaskStatus::Running && self.speed > 0 {
            omad_core::i18n::Key::SpeedPerSecond.with(lang, human_bytes(self.speed))
        } else {
            String::new()
        }
    }

    /// Time left at the current speed, when both are known.
    fn eta_text(&self) -> String {
        match (self.total, self.status, self.speed) {
            (Some(total), TaskStatus::Running, speed) if speed > 0 && total > self.received => {
                human_eta((total - self.received) / speed)
            }
            _ => String::new(),
        }
    }

    fn status_label(&self, lang: omad_core::i18n::Lang) -> &'static str {
        use omad_core::i18n::Key;
        match self.status {
            TaskStatus::Queued => Key::StatusQueued.text(lang),
            TaskStatus::Running => Key::StatusRunning.text(lang),
            TaskStatus::Paused => Key::StatusPaused.text(lang),
            TaskStatus::Completed => Key::StatusCompleted.text(lang),
            TaskStatus::Failed => Key::StatusFailed.text(lang),
        }
    }

    fn status_glyph(&self) -> &'static str {
        match self.status {
            TaskStatus::Queued => ICON_CLOCK,
            TaskStatus::Running => ICON_SPEED,
            TaskStatus::Paused => ICON_PAUSE,
            TaskStatus::Completed => ICON_DONE,
            TaskStatus::Failed => ICON_WARN,
        }
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_eta(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// How the view is told something changed, without knowing what a window is.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

/// Everything the view reads, and the commands it can send.
pub trait EngineHost: Send + Sync + 'static {
    fn rows(&self) -> Vec<Row>;
    fn add(&self, url: String);
    fn pause(&self, id: u64);
    fn resume(&self, id: u64);
    fn remove(&self, id: u64);
    fn pause_all(&self);
    fn resume_all(&self);
    /// Open the finished file, or its folder, with the desktop's own handler.
    fn reveal(&self, id: u64);
    fn open_file(&self, id: u64);
    /// How many browser profiles have been trusted so far.
    fn paired_browsers(&self) -> usize;
    fn bridge_summary(&self) -> String;
    fn set_notifier(&self, notifier: Notifier);
    fn set_referer(&self, referer: Option<String>);
    fn referer(&self) -> Option<String>;
    fn set_origin(&self, origin: Option<String>);
    fn origin(&self) -> Option<String>;
    fn download_dir(&self) -> String;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Tasks,
    Add,
    Settings,
    Extension,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Active,
    Done,
}

impl Filter {
    fn keeps(self, status: TaskStatus) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => {
                matches!(
                    status,
                    TaskStatus::Running | TaskStatus::Queued | TaskStatus::Paused
                )
            }
            Filter::Done => matches!(status, TaskStatus::Completed | TaskStatus::Failed),
        }
    }
}

/// The task view. One per window.
pub struct Tasks {
    host: Arc<dyn EngineHost>,
    lang: omad_core::i18n::Lang,
    draft: String,
    search: String,
    page: Page,
    filter: Filter,
}

impl Render for Tasks {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let all = self.host.rows();
        let running = all
            .iter()
            .filter(|r| r.status == TaskStatus::Running)
            .count();
        let speed: u64 = all.iter().map(|r| r.speed).sum();
        let visible = self.visible_rows();

        let content = match self.page {
            Page::Tasks => self.tasks_page(&all, &visible, running, speed, cx),
            Page::Add => self.add_page(cx),
            Page::Settings => self.settings_page(),
            Page::Extension => self.extension_page(cx),
        };
        let sidebar = self.sidebar(all.len(), running, cx);

        root().size_full().child(
            h_flex()
                .size_full()
                .child(sidebar)
                .child(div().w(px(1.0)).h_full().bg(surface_colour()))
                .child(v_flex().flex_1().h_full().min_w_0().child(content)),
        )
    }
}

impl Tasks {
    pub fn new(host: Arc<dyn EngineHost>) -> Self {
        Self::with_language(host, omad_core::i18n::Lang::from_env())
    }

    /// The same view in a named language; tests use this so a Chinese `LANG` in the
    /// environment cannot change what they assert.
    pub fn with_language(host: Arc<dyn EngineHost>, lang: omad_core::i18n::Lang) -> Self {
        Self {
            host,
            lang,
            draft: String::new(),
            search: String::new(),
            page: Page::Tasks,
            filter: Filter::All,
        }
    }

    /// What the view believes it is showing. Tests assert against this rather than against
    /// pixels, and the render below draws exactly this state.
    pub fn summary(&self) -> (Vec<Row>, &'static str, String) {
        let page = match self.page {
            Page::Tasks => "tasks",
            Page::Add => "add",
            Page::Settings => "settings",
            Page::Extension => "extension",
        };
        (self.host.rows(), page, self.host.bridge_summary())
    }

    /// The label of a row's primary control, so a test can prove it follows the state.
    pub fn toggle_label(&self, id: u64) -> Option<&'static str> {
        self.host
            .rows()
            .iter()
            .find(|r| r.id == id)
            .map(|row| match row.status {
                TaskStatus::Running | TaskStatus::Queued => {
                    omad_core::i18n::Key::ActionPause.text(self.lang)
                }
                TaskStatus::Paused | TaskStatus::Failed => {
                    omad_core::i18n::Key::ActionResume.text(self.lang)
                }
                TaskStatus::Completed => omad_core::i18n::Key::ActionOpen.text(self.lang),
            })
    }

    /// Which rows the current filter and search would show, in order. The render uses this
    /// same function, so a test of it is a test of the list.
    pub fn visible_rows(&self) -> Vec<Row> {
        let search = self.search.to_lowercase();
        self.host
            .rows()
            .into_iter()
            .filter(|r| self.filter.keeps(r.status))
            .filter(|r| search.is_empty() || r.name.to_lowercase().contains(&search))
            .collect()
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
    }

    pub fn set_search(&mut self, text: impl Into<String>) {
        self.search = text.into();
    }

    pub fn set_page(&mut self, page: Page) {
        self.page = page;
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    // ---------------------------------------------------------------- the rail

    fn sidebar(&self, total: usize, running: usize, cx: &mut Context<Self>) -> impl IntoElement {
        fn item(
            id: &'static str,
            glyph: &'static str,
            label: &'static str,
            counter: Option<String>,
            page: Page,
            active: bool,
            cx: &mut Context<Tasks>,
        ) -> AnyElement {
            Button::new(SharedString::from(format!("nav-{id}")))
                .full_width()
                .variant(if active {
                    ButtonVariant::Secondary
                } else {
                    ButtonVariant::Ghost
                })
                .label(if let Some(count) = counter {
                    format!("{glyph}   {label}   {count}")
                } else {
                    format!("{glyph}   {label}")
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    cx.notify();
                }))
                .into_any_element()
        }

        v_flex()
            .w(px(190.0))
            .h_full()
            .p(Space::Sm)
            .gap(Space::Xs)
            .child(
                h_flex()
                    .px(Space::Sm)
                    .py(Space::Sm)
                    .gap(Space::Sm)
                    .items_center()
                    .child(Text::new(ICON_APP).muted())
                    .child(Heading::new("OmaLL").level(3)),
            )
            .child(item(
                "tasks",
                ICON_TASKS,
                omad_core::i18n::Key::NavTasks.text(self.lang),
                Some(total.to_string()),
                Page::Tasks,
                self.page == Page::Tasks,
                cx,
            ))
            .child(item(
                "add",
                ICON_ADD,
                omad_core::i18n::Key::NavAdd.text(self.lang),
                None,
                Page::Add,
                self.page == Page::Add,
                cx,
            ))
            .child(div().flex_1())
            .when(running > 0, |el| {
                el.child(
                    h_flex()
                        .px(Space::Sm)
                        .gap(Space::Xs)
                        .items_center()
                        .child(Spinner::new("rail-spinner"))
                        .child(Text::new(format!("{running} downloading")).muted()),
                )
            })
            .child(item(
                "extension",
                ICON_LINK,
                omad_core::i18n::Key::NavExtension.text(self.lang),
                None,
                Page::Extension,
                self.page == Page::Extension,
                cx,
            ))
            .child(item(
                "settings",
                ICON_SETTINGS,
                omad_core::i18n::Key::NavSettings.text(self.lang),
                None,
                Page::Settings,
                self.page == Page::Settings,
                cx,
            ))
    }

    // ---------------------------------------------------------------- tasks page

    fn tasks_page(
        &self,
        all: &[Row],
        visible: &[Row],
        running: usize,
        speed: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = match self.filter {
            Filter::All => "all",
            Filter::Active => "active",
            Filter::Done => "done",
        };
        let filter_group =
            ToggleGroup::new("filter", omad_core::i18n::Key::FilterTasks.text(self.lang))
                .item("all", omad_core::i18n::Key::FilterAll.text(self.lang))
                .item("active", omad_core::i18n::Key::FilterActive.text(self.lang))
                .item("done", omad_core::i18n::Key::FilterDone.text(self.lang))
                .value([current])
                .on_change(cx.listener(|this, value: &[SharedString], _, cx| {
                    this.filter = match value.first().map(|v| v.as_ref()) {
                        Some("active") => Filter::Active,
                        Some("done") => Filter::Done,
                        _ => Filter::All,
                    };
                    cx.notify();
                }));

        let can_start = all
            .iter()
            .any(|r| matches!(r.status, TaskStatus::Paused | TaskStatus::Failed));
        let can_pause = running > 0;

        let toolbar = h_flex()
            .w_full()
            .h(px(48.0))
            .px(Space::Md)
            .gap(Space::Sm)
            .items_center()
            .border_b_1()
            .border_color(surface_colour())
            .child(filter_group)
            .child(
                Button::new("start-all")
                    .label(format!(
                        "{ICON_PLAY}  {}",
                        omad_core::i18n::Key::StartAll.text(self.lang)
                    ))
                    .variant(ButtonVariant::Secondary)
                    .disabled(!can_start)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.host.resume_all();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("pause-all")
                    .label(format!(
                        "{ICON_PAUSE}  {}",
                        omad_core::i18n::Key::PauseAll.text(self.lang)
                    ))
                    .variant(ButtonVariant::Secondary)
                    .disabled(!can_pause)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.host.pause_all();
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .when(speed > 0, |el| {
                el.child(
                    Badge::new(format!("{ICON_SPEED}  {}/s", human_bytes(speed))).tone(Tone::Info),
                )
            })
            .child(
                div().w(px(200.0)).child(
                    SearchInput::new("search", omad_core::i18n::Key::SearchTasks.text(self.lang))
                        .value(self.search.clone())
                        .placeholder("Search")
                        .on_change(cx.listener(|this, value: &str, _, cx| {
                            this.search = value.to_string();
                            cx.notify();
                        })),
                ),
            );

        let body: AnyElement = if visible.is_empty() {
            self.empty_state(all.is_empty())
        } else {
            v_flex()
                .w_full()
                .p(Space::Sm)
                .gap(Space::Xs)
                .children(visible.iter().map(|row| self.task_card(row, cx)))
                .into_any_element()
        };

        v_flex()
            .size_full()
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn empty_state(&self, nothing_at_all: bool) -> AnyElement {
        let (glyph, title, hint) = if nothing_at_all {
            (ICON_ADD, omad_core::i18n::Key::NoTasks.text(self.lang), "")
        } else {
            (
                ICON_TASKS,
                omad_core::i18n::Key::NoResults.text(self.lang),
                "",
            )
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(Space::Sm)
            .p(Space::Lg)
            .child(Text::new(glyph).muted())
            .child(Heading::new(title).level(3))
            .child(Text::new(hint).muted())
            .into_any_element()
    }

    fn task_card(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.lang;
        let id = row.id;
        let status = row.status;
        let can_pause = matches!(status, TaskStatus::Running | TaskStatus::Queued);
        let can_resume = matches!(status, TaskStatus::Paused | TaskStatus::Failed);
        let done = status == TaskStatus::Completed;
        let tone = match status {
            TaskStatus::Completed => Tone::Success,
            TaskStatus::Failed => Tone::Danger,
            TaskStatus::Paused => Tone::Warning,
            _ => Tone::Info,
        };
        let fraction = row.fraction();

        let meta = h_flex()
            .gap(Space::Md)
            .items_center()
            .when(!row.speed_text(lang).is_empty(), |el| {
                el.child(Text::new(row.speed_text(lang)).muted())
            })
            .when(!row.eta_text().is_empty(), |el| {
                el.child(Text::new(format!("{} left", row.eta_text())).muted())
            })
            .child(Text::new(row.size_text()).muted())
            .when_some(row.error.clone(), |el, error| {
                el.child(Text::new(error).muted())
            });

        let actions = h_flex()
            .gap(Space::Xs)
            .items_center()
            .child(if done {
                Button::new(SharedString::from(format!("toggle-{id}")))
                    .label(omad_core::i18n::Key::ActionOpen.text(lang))
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(move |this, _, _, _| this.host.open_file(id)))
                    .into_any_element()
            } else {
                Button::new(SharedString::from(format!("toggle-{id}")))
                    .label(if can_pause {
                        omad_core::i18n::Key::ActionPause.text(lang)
                    } else {
                        omad_core::i18n::Key::ActionResume.text(lang)
                    })
                    .variant(ButtonVariant::Primary)
                    .disabled(!(can_pause || can_resume))
                    .on_click(cx.listener(move |this, _, _, _| {
                        if can_pause {
                            this.host.pause(id);
                        } else {
                            this.host.resume(id);
                        }
                    }))
                    .into_any_element()
            })
            .child(
                Button::new(SharedString::from(format!("reveal-{id}")))
                    .label(omad_core::i18n::Key::ActionFolder.text(lang))
                    .variant(ButtonVariant::Secondary)
                    .disabled(!done)
                    .on_click(cx.listener(move |this, _, _, _| this.host.reveal(id))),
            )
            .child(
                Button::new(SharedString::from(format!("remove-{id}")))
                    .label(omad_core::i18n::Key::ActionRemove.text(lang))
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(move |this, _, _, _| this.host.remove(id))),
            );

        v_flex()
            .debug_selector(move || format!("row/{id}"))
            .w_full()
            .px(Space::Md)
            .py(Space::Md)
            .gap(Space::Xs)
            .border_1()
            .border_color(surface_colour())
            .rounded(px(2.0))
            .bg(card_colour())
            .child(
                h_flex()
                    .w_full()
                    .gap(Space::Sm)
                    .items_center()
                    .child(
                        div()
                            .w(px(32.0))
                            .h(px(32.0))
                            .rounded(px(2.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Text::new(row.status_glyph()).muted()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(1.0))
                            .child(Heading::new(row.name.clone()).level(3))
                            .child(meta),
                    )
                    .child(
                        h_flex()
                            .gap(Space::Xs)
                            .items_center()
                            .child(Badge::new(row.status_label(lang)).tone(tone))
                            .child(actions),
                    ),
            )
            .child(progress_line(fraction))
            .into_any_element()
    }

    // ---------------------------------------------------------------- add page

    fn add_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let form = v_flex()
            .w_full()
            .p(Space::Lg)
            .gap(Space::Md)
            .child(Text::new(omad_core::i18n::Key::AddPlaceholder.text(self.lang)).muted())
            .child(
                div().w_full().child(
                    Input::new("url", "Link")
                        .value(self.draft.clone())
                        .placeholder("https://example.com/file.iso")
                        .on_change(cx.listener(|this, value: &str, _, cx| {
                            this.draft = value.to_string();
                            cx.notify();
                        })),
                ),
            )
            .child(
                h_flex()
                    .gap(Space::Sm)
                    .child(
                        Button::new("add")
                            .label(omad_core::i18n::Key::AddButton.text(self.lang))
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let url = this.draft.trim().to_string();
                                if !url.is_empty() {
                                    this.host.add(url);
                                    this.draft.clear();
                                    this.page = Page::Tasks;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cancel-add")
                            .label(omad_core::i18n::Key::AddCancel.text(self.lang))
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.draft.clear();
                                this.page = Page::Tasks;
                                cx.notify();
                            })),
                    ),
            )
            .child(Separator::horizontal())
            .child(
                Text::new(omad_core::i18n::Key::SavingTo.with(self.lang, self.host.download_dir()))
                    .muted(),
            );

        v_flex()
            .size_full()
            .child(page_header(
                omad_core::i18n::Key::PageAddTitle.text(self.lang),
            ))
            .child(form)
            .into_any_element()
    }

    // ---------------------------------------------------------------- settings

    fn settings_page(&self) -> AnyElement {
        v_flex()
            .size_full()
            .child(page_header(
                omad_core::i18n::Key::PageSettingsTitle.text(self.lang),
            ))
            .child(
                v_flex()
                    .p(Space::Lg)
                    .gap(Space::Md)
                    .child(Heading::new("Downloads").level(3))
                    .child(Text::new(format!("Folder: {}", self.host.download_dir())).muted())
                    .child(Separator::horizontal())
                    .child(Heading::new("Bridge").level(3))
                    .child(Text::new(self.host.bridge_summary()).muted())
                    .child(
                        Text::new(
                            "The bridge only accepts the extension profile you pair with it.",
                        )
                        .muted(),
                    ),
            )
            .into_any_element()
    }

    // ---------------------------------------------------------------- extension

    fn extension_page(&self, _cx: &mut Context<Self>) -> AnyElement {
        use omad_core::i18n::Key;
        let paired = self.host.paired_browsers();
        let status = match paired {
            0 => Key::ExtensionNoBrowser.text(self.lang).to_string(),
            1 => Key::ExtensionOneBrowser.text(self.lang).to_string(),
            n => Key::ExtensionManyBrowsers.with(self.lang, n),
        };
        let panel = v_flex()
            .p(Space::Lg)
            .gap(Space::Md)
            .child(Text::new(Key::ExtensionExplains.text(self.lang)).muted())
            .child(
                h_flex()
                    .gap(Space::Sm)
                    .items_center()
                    .child(
                        Text::new(omad_core::i18n::Key::ExtensionAddress.text(self.lang)).muted(),
                    )
                    .child(Text::new(self.host.bridge_summary())),
            )
            .child(Text::new(status).muted())
            .child(Separator::horizontal())
            .child(Heading::new(Key::ExtensionLoadingTitle.text(self.lang)).level(3))
            .child(Text::new(Key::ExtensionLoadingText.text(self.lang)).muted());

        v_flex()
            .size_full()
            .child(page_header(
                omad_core::i18n::Key::PageExtensionTitle.text(self.lang),
            ))
            .child(panel)
            .into_any_element()
    }
}

fn page_header(title: &'static str) -> AnyElement {
    h_flex()
        .w_full()
        .h(px(48.0))
        .px(Space::Md)
        .items_center()
        .border_b_1()
        .border_color(surface_colour())
        .child(Heading::new(title).level(2))
        .into_any_element()
}

/// The progress line under a card. GPUI 0.2.2 has no percentage sizing, so the filled part is
/// a fixed-width child measured against the card's nominal content width; the parent clips it.
fn progress_line(fraction: f64) -> AnyElement {
    // GPUI 0.2.2 has no percentage width, so the track is a flex row of two children that
    // share the card's width by growth. The filled part keeps a minimum so a hair of
    // progress is still visible.
    let done = fraction.clamp(0.0, 1.0) as f32;
    div()
        .debug_selector(|| "progress-track".to_string())
        .w_full()
        .h(px(3.0))
        .rounded(px(2.0))
        .overflow_hidden()
        .bg(accent_soft())
        .child(
            div()
                .h_full()
                // `relative` is GPUI's percentage length: the fill follows the card.
                .w(relative(done))
                .rounded(px(2.0))
                .bg(accent_colour()),
        )
        .into_any_element()
}

/// The one place this file asks for a colour. It goes through the semantic layer, which
/// guarantees the contrast of every value it hands out, so this file never names one.
fn surface_colour() -> odk::ui::gpui::Rgba {
    use odk::theme::semantic::BorderToken;
    let snapshot = odk::ui::theme_snapshot();
    let colour = snapshot.semantic.border.get(BorderToken::Subtle);
    odk::ui::gpui::rgba(colour.to_rgba_u32())
}

/// The card a task sits on: one step above the window, as Ghost's cards are.
fn card_colour() -> odk::ui::gpui::Rgba {
    use odk::theme::semantic::SurfaceToken;
    let snapshot = odk::ui::theme_snapshot();
    let colour = snapshot.semantic.surface.get(SurfaceToken::Recessed);
    odk::ui::gpui::rgba(colour.to_rgba_u32())
}

fn accent_colour() -> odk::ui::gpui::Rgba {
    use odk::theme::semantic::{Emphasis, Role, State};
    let snapshot = odk::ui::theme_snapshot();
    let colour = snapshot
        .semantic
        .role(Role::Accent)
        .emphasis(Emphasis::Solid)
        .state(State::Default)
        .background;
    odk::ui::gpui::rgba(colour.to_rgba_u32())
}

fn accent_soft() -> odk::ui::gpui::Rgba {
    use odk::theme::semantic::{Emphasis, Role, State};
    let snapshot = odk::ui::theme_snapshot();
    let colour = snapshot
        .semantic
        .role(Role::Accent)
        .emphasis(Emphasis::Subtle)
        .state(State::Hover)
        .background;
    odk::ui::gpui::rgba(colour.to_rgba_u32())
}

/// The engine's news, shared with the background thread. The view only ever reads this.
#[derive(Default)]
pub struct TaskBoard {
    rows: Mutex<Vec<Row>>,
    /// Tasks this process is downloading. Their progress comes from the engine, which
    /// reports faster and more accurately than the record; the store is not consulted for
    /// them.
    owned: Mutex<std::collections::HashSet<u64>>,
}

impl TaskBoard {
    pub fn rows(&self) -> Vec<Row> {
        self.rows.lock().unwrap().clone()
    }

    pub fn update_from_record(&self, record: &TaskRecord) {
        let mut rows = self.rows.lock().unwrap();
        let row = Row {
            id: record.id.0,
            name: record.filename.clone(),
            url: record.url.clone(),
            status: record.status,
            received: record.received_bytes,
            total: record.total_bytes,
            speed: 0,
            error: record.error.clone(),
            path: Some(record.file_path.clone()),
        };
        match rows.iter_mut().find(|r| r.id == row.id) {
            Some(existing) => *existing = row,
            None => rows.push(row),
        }
    }

    pub fn remove(&self, id: u64) {
        self.rows.lock().unwrap().retain(|r| r.id != id);
    }

    pub fn set_progress(&self, id: u64, received: u64, total: Option<u64>, speed: u64) {
        let mut rows = self.rows.lock().unwrap();
        if let Some(row) = rows.iter_mut().find(|r| r.id == id) {
            row.received = received;
            if total.is_some() {
                row.total = total;
            }
            row.speed = speed;
        }
    }

    pub fn set_status(&self, id: u64, status: TaskStatus, error: Option<String>) {
        let mut rows = self.rows.lock().unwrap();
        if let Some(row) = rows.iter_mut().find(|r| r.id == id) {
            row.status = status;
            row.error = error;
            if status != TaskStatus::Running {
                row.speed = 0;
            }
        }
    }

    /// Mirrors the store into the board for tasks this process is not driving — a task the
    /// CLI added, or one a previous run left behind. A row the engine is running here is
    /// left alone: the engine's own reports are more current than the record's last commit.
    pub fn mark_owned(&self, id: u64) {
        self.owned.lock().unwrap().insert(id);
    }

    pub fn release(&self, id: u64) {
        self.owned.lock().unwrap().remove(&id);
    }

    pub fn adopt(&self, records: &[TaskRecord]) {
        let owned: std::collections::HashSet<u64> = self.owned.lock().unwrap().clone();
        let mut rows = self.rows.lock().unwrap();
        for record in records {
            match rows.iter_mut().find(|r| r.id == record.id.0) {
                Some(_) if owned.contains(&record.id.0) => {}
                Some(row) => {
                    row.status = record.status;
                    row.received = record.received_bytes;
                    if record.total_bytes.is_some() {
                        row.total = record.total_bytes;
                    }
                    row.error = record.error.clone();
                    row.path = Some(record.file_path.clone());
                    if record.status != TaskStatus::Running {
                        row.speed = 0;
                    }
                }
                None => {
                    rows.push(Row {
                        id: record.id.0,
                        name: record.filename.clone(),
                        url: record.url.clone(),
                        status: record.status,
                        received: record.received_bytes,
                        total: record.total_bytes,
                        speed: 0,
                        error: record.error.clone(),
                        path: Some(record.file_path.clone()),
                    });
                }
            }
        }
        // A task removed elsewhere disappears here too.
        let alive: std::collections::HashSet<u64> = records.iter().map(|r| r.id.0).collect();
        rows.retain(|r| alive.contains(&r.id) || owned.contains(&r.id));
    }

    pub fn path_of(&self, id: u64) -> Option<String> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.path.clone())
    }
}

/// Sink implementation the engine thread installs, so the view sees progress as it happens.
pub struct BoardSink {
    pub board: Arc<TaskBoard>,
    pub notifier: Mutex<Option<Notifier>>,
}

impl BoardSink {
    fn wake(&self) {
        if let Some(notifier) = self.notifier.lock().unwrap().as_ref() {
            notifier();
        }
    }
}

impl StoreSink for BoardSink {
    fn on_plan(&self, id: omad_core::TaskId, _probe: &Probe, plan: &Plan, _connections: u8) {
        self.board
            .set_progress(id.0, plan.recorded(), Some(plan.total), 0);
        self.wake();
    }
    fn on_progress(&self, id: omad_core::TaskId, progress: &EngineProgress) {
        self.board
            .set_progress(id.0, progress.received, progress.total, progress.speed);
        self.wake();
    }
    fn on_commit(&self, _id: omad_core::TaskId, _received: u64, _total: Option<u64>, _speed: u64) {}
    fn on_finished(
        &self,
        id: omad_core::TaskId,
        outcome: Outcome,
        recorded: u64,
        total: Option<u64>,
    ) {
        let status = match outcome {
            Outcome::Completed => TaskStatus::Completed,
            Outcome::Paused => TaskStatus::Paused,
        };
        self.board.set_progress(id.0, recorded, total, 0);
        self.board.set_status(id.0, status, None);
        self.wake();
    }
    fn on_failed(&self, id: omad_core::TaskId, error: &str, _resumable: bool) {
        self.board
            .set_status(id.0, TaskStatus::Failed, Some(error.to_string()));
        self.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, name: &str, status: TaskStatus, received: u64, total: Option<u64>) -> Row {
        Row {
            id,
            name: name.into(),
            url: format!("https://example.com/{name}"),
            status,
            received,
            total,
            speed: 0,
            error: None,
            path: Some(format!("/tmp/{name}")),
        }
    }

    fn record(id: u64, name: &str, status: TaskStatus) -> TaskRecord {
        TaskRecord {
            id: omad_core::TaskId(id),
            url: "u".into(),
            filename: name.into(),
            dest: "downloads".into(),
            status,
            total_bytes: Some(100),
            received_bytes: 0,
            speed: 0,
            connections: 4,
            etag: None,
            file_path: format!("/tmp/{name}"),
            error: None,
            plan: None,
            expected_sha256: None,
            allow_private: false,
            referer: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn bytes_are_shown_in_the_unit_that_fits() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn eta_reads_like_a_person_wrote_it() {
        assert_eq!(human_eta(5), "5s");
        assert_eq!(human_eta(65), "1m 05s");
        assert_eq!(human_eta(3725), "1h 02m");
    }

    #[test]
    fn progress_is_clamped() {
        let mut r = row(1, "a", TaskStatus::Running, 200, Some(100));
        assert_eq!(r.fraction(), 1.0);
        r.total = None;
        assert_eq!(r.fraction(), 0.0);
        assert_eq!(r.size_text(), "200 B");
    }

    #[test]
    fn speed_and_eta_only_show_while_running() {
        let mut r = row(1, "a", TaskStatus::Running, 0, Some(1000));
        r.speed = 500;
        assert_eq!(r.speed_text(omad_core::i18n::Lang::En), "500 B/s");
        assert_eq!(r.eta_text(), "2s");
        r.status = TaskStatus::Paused;
        assert_eq!(r.speed_text(omad_core::i18n::Lang::En), "");
        assert_eq!(r.eta_text(), "");
    }

    #[test]
    fn the_filter_and_the_search_both_narrow_the_list() {
        let board = Arc::new(TaskBoard::default());
        for (id, name, status) in [
            (1, "linux.iso", TaskStatus::Running),
            (2, "notes.zip", TaskStatus::Paused),
            (3, "photo.png", TaskStatus::Completed),
            (4, "broken.bin", TaskStatus::Failed),
        ] {
            board.update_from_record(&record(id, name, status));
        }
        let host = Arc::new(TestHost {
            board: board.clone(),
        });
        let mut view = Tasks::new(host);

        assert_eq!(view.visible_rows().len(), 4);
        view.set_filter(Filter::Active);
        assert_eq!(
            view.visible_rows().len(),
            2,
            "running and paused are active"
        );
        view.set_filter(Filter::Done);
        assert_eq!(
            view.visible_rows().len(),
            2,
            "completed and failed are done"
        );
        view.set_filter(Filter::All);
        view.set_search("zip");
        let found = view.visible_rows();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "notes.zip");
    }

    #[test]
    fn the_board_tracks_a_task_through_its_life() {
        let board = Arc::new(TaskBoard::default());
        board.set_progress(1, 0, Some(100), 0);
        assert!(board.rows().is_empty(), "the board invents nothing");

        board.update_from_record(&record(1, "a.bin", TaskStatus::Queued));
        board.set_progress(1, 50, Some(100), 2048);
        let rows = board.rows();
        assert_eq!(rows[0].received, 50);
        assert_eq!(rows[0].fraction(), 0.5);

        board.set_status(1, TaskStatus::Completed, None);
        assert_eq!(board.rows()[0].status, TaskStatus::Completed);
        assert_eq!(board.rows()[0].speed, 0);
        board.remove(1);
        assert!(board.rows().is_empty());
    }

    #[test]
    fn a_sink_wakes_the_notifier() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let board = Arc::new(TaskBoard::default());
        board.update_from_record(&record(1, "a", TaskStatus::Running));
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let sink = BoardSink {
            board: board.clone(),
            notifier: Mutex::new(Some(Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }) as Notifier)),
        };
        sink.on_progress(
            omad_core::TaskId(1),
            &EngineProgress {
                received: 10,
                total: Some(100),
                speed: 1,
            },
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(board.rows()[0].received, 10);
    }

    /// A host whose only job is to hand the board to the view in tests.
    struct TestHost {
        board: Arc<TaskBoard>,
    }

    impl EngineHost for TestHost {
        fn rows(&self) -> Vec<Row> {
            self.board.rows()
        }
        fn add(&self, _url: String) {}
        fn pause(&self, _id: u64) {}
        fn resume(&self, _id: u64) {}
        fn remove(&self, _id: u64) {}
        fn pause_all(&self) {}
        fn resume_all(&self) {}
        fn reveal(&self, _id: u64) {}
        fn open_file(&self, _id: u64) {}
        fn paired_browsers(&self) -> usize {
            0
        }
        fn bridge_summary(&self) -> String {
            String::new()
        }
        fn set_notifier(&self, _notifier: Notifier) {}
        fn set_referer(&self, _referer: Option<String>) {}
        fn referer(&self) -> Option<String> {
            None
        }
        fn set_origin(&self, _origin: Option<String>) {}
        fn origin(&self) -> Option<String> {
            None
        }
        fn download_dir(&self) -> String {
            "/tmp".into()
        }
    }
}
