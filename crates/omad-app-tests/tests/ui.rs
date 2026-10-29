//! The view on GPUI's headless test platform.
//!
//! No compositor on this machine will let GPUI's X11 backend map a window, so the window
//! cannot be photographed here. These tests do what a screenshot cannot: they build, lay
//! out and paint the view and then interrogate the frame — a row a user can see is in the
//! frame at a real size, and a click on its control reaches the host.

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use omad_app::{EngineHost, Filter, Notifier, Row, Tasks};
use omad_core::TaskStatus;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct FakeHost {
    rows: Mutex<Vec<Row>>,
    added: Mutex<Vec<String>>,
    paused: Mutex<Vec<u64>>,
    resumed: Mutex<Vec<u64>>,
    removed: Mutex<Vec<u64>>,
    opened: Mutex<Vec<u64>>,
    revealed: Mutex<Vec<u64>>,
    pairing_started: Mutex<u32>,
    notifier: Mutex<Option<Notifier>>,
    referer: Mutex<Option<String>>,
    origin: Mutex<Option<String>>,
}

impl FakeHost {
    fn with_rows(rows: Vec<Row>) -> Arc<Self> {
        let host = Self::default();
        *host.rows.lock().unwrap() = rows;
        Arc::new(host)
    }
}

impl EngineHost for FakeHost {
    fn rows(&self) -> Vec<Row> {
        self.rows.lock().unwrap().clone()
    }
    fn add(&self, url: String) {
        self.added.lock().unwrap().push(url);
    }
    fn pause(&self, id: u64) {
        self.paused.lock().unwrap().push(id);
    }
    fn resume(&self, id: u64) {
        self.resumed.lock().unwrap().push(id);
    }
    fn remove(&self, id: u64) {
        self.removed.lock().unwrap().push(id);
    }
    fn pause_all(&self) {
        for row in self.rows.lock().unwrap().iter() {
            if matches!(row.status, TaskStatus::Running | TaskStatus::Queued) {
                self.paused.lock().unwrap().push(row.id);
            }
        }
    }
    fn resume_all(&self) {
        for row in self.rows.lock().unwrap().iter() {
            if matches!(row.status, TaskStatus::Paused | TaskStatus::Failed) {
                self.resumed.lock().unwrap().push(row.id);
            }
        }
    }
    fn reveal(&self, id: u64) {
        self.revealed.lock().unwrap().push(id);
    }
    fn open_file(&self, id: u64) {
        self.opened.lock().unwrap().push(id);
    }
    fn paired_browsers(&self) -> usize {
        *self.pairing_started.lock().unwrap() as usize
    }
    fn bridge_summary(&self) -> String {
        "ws://127.0.0.1:54321".into()
    }
    fn set_notifier(&self, notifier: Notifier) {
        *self.notifier.lock().unwrap() = Some(notifier);
    }
    fn set_referer(&self, referer: Option<String>) {
        *self.referer.lock().unwrap() = referer;
    }
    fn referer(&self) -> Option<String> {
        self.referer.lock().unwrap().clone()
    }
    fn set_origin(&self, origin: Option<String>) {
        *self.origin.lock().unwrap() = origin;
    }
    fn origin(&self) -> Option<String> {
        self.origin.lock().unwrap().clone()
    }
    fn download_dir(&self) -> String {
        "/tmp/downloads".into()
    }
}

fn row(id: u64, name: &str, status: TaskStatus, received: u64, total: Option<u64>) -> Row {
    Row {
        id,
        name: name.into(),
        url: format!("https://example.com/{name}"),
        status,
        received,
        total,
        speed: 1024 * 512,
        error: None,
        path: Some(format!("/tmp/{name}")),
    }
}

fn open(cx: &mut TestAppContext, host: Arc<FakeHost>) -> (&mut VisualTestContext, Entity<Tasks>) {
    let (entity, visual) = odk::ui::test_support::open_view(cx, move |_, cx| {
        // Pinned to English: a Chinese `LANG` in the environment must not change what
        // these tests assert.
        cx.new(|_| Tasks::with_language(host, omad_core::i18n::Lang::En))
    });
    visual.run_until_parked();
    (visual, entity)
}

fn bounds(cx: &mut VisualTestContext, selector: &str) -> Option<gpui::Bounds<gpui::Pixels>> {
    let leaked: &'static str = Box::leak(selector.to_string().into_boxed_str());
    cx.debug_bounds(leaked)
}

fn click(cx: &mut VisualTestContext, selector: &str) {
    let target =
        bounds(cx, selector).unwrap_or_else(|| panic!("{selector} should be in the frame"));
    cx.simulate_click(target.center(), Default::default());
    cx.run_until_parked();
}

#[test]
fn the_window_paints_the_rail_the_toolbar_and_the_controls() {
    let host = FakeHost::with_rows(vec![]);
    let mut cx = TestAppContext::single();
    let (visual, entity) = open(&mut cx, host);

    // The frame was painted: the parts a user interacts with have real bounds.
    assert!(
        bounds(visual, "nav-tasks").is_some(),
        "the rail has a Tasks entry"
    );
    assert!(
        bounds(visual, "nav-add").is_some(),
        "the rail has an Add entry"
    );
    assert!(
        bounds(visual, "nav-settings").is_some(),
        "the rail has a Settings entry"
    );
    assert!(
        bounds(visual, "start-all/button").is_some(),
        "the toolbar has Start all"
    );
    assert!(
        bounds(visual, "pause-all/button").is_some(),
        "the toolbar has Pause all"
    );
    assert!(
        bounds(visual, "search/box").is_some(),
        "the toolbar has a search field"
    );

    let (rows, page, _) = entity.read_with(visual, |view, _| view.summary());
    assert!(rows.is_empty());
    assert_eq!(page, "tasks");
}

#[test]
fn a_row_is_drawn_with_its_control_for_every_state() {
    let host = FakeHost::with_rows(vec![
        row(1, "linux.iso", TaskStatus::Running, 5 << 20, Some(10 << 20)),
        row(2, "notes.zip", TaskStatus::Paused, 1024, Some(4096)),
        row(3, "photo.png", TaskStatus::Completed, 2048, Some(2048)),
    ]);
    let mut cx = TestAppContext::single();
    let (visual, entity) = open(&mut cx, host);

    for id in 1..=3 {
        let card = bounds(visual, &format!("row/{id}"))
            .unwrap_or_else(|| panic!("row {id} should be in the frame"));
        assert!(
            f32::from(card.size.height) > 40.0,
            "a card has height: {card:?}"
        );
    }
    let first = bounds(visual, "row/1").unwrap();
    let third = bounds(visual, "row/3").unwrap();
    assert!(
        third.origin.y > first.origin.y + first.size.height - gpui::px(1.0),
        "rows stack instead of overlapping"
    );

    let labels: Vec<&str> = (1..=3)
        .map(|id| entity.read_with(visual, |view, _| view.toggle_label(id).unwrap()))
        .collect();
    assert_eq!(labels, vec!["Pause", "Resume", "Open"]);
}

#[test]
fn the_filter_narrows_the_list() {
    let host = FakeHost::with_rows(vec![
        row(1, "linux.iso", TaskStatus::Running, 1, Some(2)),
        row(2, "notes.zip", TaskStatus::Paused, 1, Some(2)),
        row(3, "photo.png", TaskStatus::Completed, 1, Some(2)),
    ]);
    let mut cx = TestAppContext::single();
    let (visual, entity) = open(&mut cx, host);
    assert_eq!(entity.read_with(visual, |v, _| v.visible_rows().len()), 3);

    entity.update(visual, |view, _| view.set_filter(Filter::Active));
    visual.run_until_parked();
    assert_eq!(entity.read_with(visual, |v, _| v.visible_rows().len()), 2);

    entity.update(visual, |view, _| view.set_filter(Filter::Done));
    visual.run_until_parked();
    assert_eq!(entity.read_with(visual, |v, _| v.visible_rows().len()), 1);
}

#[test]
fn a_running_task_pauses_when_its_control_is_clicked() {
    let host = FakeHost::with_rows(vec![row(7, "a.bin", TaskStatus::Running, 1, Some(2))]);
    let mut cx = TestAppContext::single();
    let (visual, _entity) = open(&mut cx, host.clone());

    click(visual, "toggle-7/button");

    assert_eq!(
        host.paused.lock().unwrap().as_slice(),
        [7],
        "clicking Pause on a running task must reach the host with that task's id"
    );
    assert!(host.resumed.lock().unwrap().is_empty());
}

#[test]
fn a_paused_task_resumes_when_its_control_is_clicked() {
    let host = FakeHost::with_rows(vec![row(9, "b.bin", TaskStatus::Paused, 1, Some(2))]);
    let mut cx = TestAppContext::single();
    let (visual, _entity) = open(&mut cx, host.clone());

    click(visual, "toggle-9/button");

    assert_eq!(host.resumed.lock().unwrap().as_slice(), [9]);
    assert!(host.paused.lock().unwrap().is_empty());
}

#[test]
fn a_completed_task_opens_its_file() {
    let host = FakeHost::with_rows(vec![row(4, "done.bin", TaskStatus::Completed, 2, Some(2))]);
    let mut cx = TestAppContext::single();
    let (visual, _entity) = open(&mut cx, host.clone());

    click(visual, "toggle-4/button");
    assert_eq!(
        host.opened.lock().unwrap().as_slice(),
        [4],
        "Done means open the file"
    );

    click(visual, "reveal-4/button");
    assert_eq!(
        host.revealed.lock().unwrap().as_slice(),
        [4],
        "the folder button works"
    );
}

#[test]
fn removing_a_task_reaches_the_host() {
    let host = FakeHost::with_rows(vec![row(5, "x.bin", TaskStatus::Paused, 1, Some(2))]);
    let mut cx = TestAppContext::single();
    let (visual, _entity) = open(&mut cx, host.clone());

    click(visual, "remove-5/button");
    assert_eq!(host.removed.lock().unwrap().as_slice(), [5]);
}

#[test]
fn start_all_and_pause_all_reach_the_host() {
    let host = FakeHost::with_rows(vec![
        row(1, "a.bin", TaskStatus::Paused, 1, Some(2)),
        row(2, "b.bin", TaskStatus::Running, 1, Some(2)),
    ]);
    let mut cx = TestAppContext::single();
    let (visual, _entity) = open(&mut cx, host.clone());

    click(visual, "pause-all/button");
    assert_eq!(
        host.paused.lock().unwrap().as_slice(),
        [2],
        "only the running task pauses"
    );

    click(visual, "start-all/button");
    assert_eq!(
        host.resumed.lock().unwrap().as_slice(),
        [1],
        "only the paused task starts"
    );
}

#[test]
fn the_add_page_takes_a_link_and_hands_it_over() {
    let host = FakeHost::with_rows(vec![]);
    let mut cx = TestAppContext::single();
    let (visual, entity) = open(&mut cx, host.clone());

    click(visual, "nav-add");
    assert_eq!(entity.read_with(visual, |v, _| v.summary().1), "add");

    click(visual, "url/box");
    visual.simulate_input("https://example.com/file.zip");
    visual.run_until_parked();
    assert_eq!(
        entity.read_with(visual, |v, _| v.draft().to_string()),
        "https://example.com/file.zip"
    );

    click(visual, "add/button");
    assert_eq!(
        host.added.lock().unwrap().as_slice(),
        ["https://example.com/file.zip".to_string()],
        "the URL reaches the host once, unchanged"
    );
    assert_eq!(entity.read_with(visual, |v, _| v.summary().1), "tasks");
}

#[test]
fn an_empty_link_is_not_sent() {
    let host = FakeHost::with_rows(vec![]);
    let mut cx = TestAppContext::single();
    let (visual, _entity) = open(&mut cx, host.clone());

    click(visual, "nav-add");
    click(visual, "add/button");

    assert!(
        host.added.lock().unwrap().is_empty(),
        "nothing to add, nothing sent"
    );
}

#[test]
fn the_extension_page_shows_the_address_and_connects_without_a_code() {
    let host = FakeHost::with_rows(vec![]);
    let mut cx = TestAppContext::single();
    let (visual, entity) = open(&mut cx, host.clone());

    click(visual, "nav-extension");
    let (_, page, address) = entity.read_with(visual, |v, _| v.summary());
    assert_eq!(page, "extension");
    assert_eq!(
        address, "ws://127.0.0.1:54321",
        "the fixed address is shown"
    );
    // There is nothing to type and no button to press: the extension connects by itself.
    assert!(bounds(visual, "pair/button").is_none(), "no pairing button");
}

#[test]
fn a_failed_row_keeps_its_place_and_its_reason() {
    let mut failed = row(4, "broken.bin", TaskStatus::Failed, 0, None);
    failed.error = Some("the server answered 403".into());
    let host = FakeHost::with_rows(vec![failed]);
    let mut cx = TestAppContext::single();
    let (visual, entity) = open(&mut cx, host);

    assert!(
        bounds(visual, "row/4").is_some(),
        "a failed task stays visible"
    );
    let rows = entity.read_with(visual, |v, _| v.summary().0);
    assert_eq!(rows[0].status, TaskStatus::Failed);
    assert_eq!(rows[0].error.as_deref(), Some("the server answered 403"));
}
