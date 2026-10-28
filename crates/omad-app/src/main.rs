//! The OmaLLDownloader desktop entry point.
//!
//! Wiring only: it creates the store, the engine thread and the extension bridge, then
//! hands the view a [`Service`] that owns both. Nothing in this file renders.

use odk::prelude::*;
use omad_app::{BoardSink, EngineHost, Notifier, Row, TaskBoard, Tasks};
use omad_core::{AddTask, TaskId, TaskStatus, ssrf::UrlPolicy};
use omad_engine::engine::Plan;
use omad_engine::{Engine, EngineConfig, Outcome, Progress, StoreSink};
use omad_rpc::protocol::TaskView;
use omad_rpc::server::{Bridge, BridgeConfig, BridgeHost};
use omad_store::{NewTask, Store, TaskRecord};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;

fn main() {
    // Before ODK: point it at a copy of Omarchy's theme that carries the `mode` ODK needs.
    let adapter = omad_app::theme::install();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,omad_engine=info,omad_rpc=info".into()),
        )
        .init();

    let service = match Service::start() {
        Ok(service) => service,
        Err(error) => {
            eprintln!("OmaLLDownloader could not start: {error}");
            std::process::exit(1);
        }
    };

    // A theme switch in Omarchy rewrites the copy; ODK's own watcher then reloads. The
    // window asks for a repaint through the same notifier the engine uses.
    let service_for_theme = service.clone();
    let _theme_thread = adapter.watch(move || service_for_theme.wake());

    let host: Arc<dyn EngineHost> = Arc::new(service.clone());
    odk::ui::run(move |cx| {
        let host = host.clone();
        odk::ui::open_window(
            cx,
            WindowOptions {
                // The app id is what Hyprland, the bar and the launcher match a window
                // against; the desktop entry names the same value.
                app_id: Some("omalldownloader".into()),
                titlebar: Some(odk::ui::gpui::TitlebarOptions {
                    title: Some("OmaLLDownloader".into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                window_min_size: Some(odk::ui::gpui::Size {
                    width: odk::ui::gpui::px(720.0),
                    height: odk::ui::gpui::px(480.0),
                }),
                // A fixed size on request, for screenshots and for a tiling setup where a
                // floating window is wanted: `OMAD_WINDOW=1200x820`.
                window_bounds: window_bounds_from_env(),
                ..Default::default()
            },
            move |_, cx| cx.new(|_| Tasks::new(host)),
        )
        .expect("the window opens");
    });
}

/// One command the engine thread runs.
enum Command {
    Add {
        url: String,
        /// The name the browser would have used, when it had one.
        filename: Option<String>,
        origin: Option<String>,
        referer: Option<String>,
        allow_private: bool,
    },
    Pause(TaskId),
    Resume(TaskId),
    Remove(TaskId),
}

#[derive(Clone)]
struct Service {
    board: Arc<TaskBoard>,
    /// Kept alive for the life of the process: the bridge's handle, the runtime it runs
    /// on, and the store the tasks are written to. Dropping any of them ends the app.
    #[allow(dead_code)]
    store: Store,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    state: Arc<Mutex<ServiceState>>,
    #[allow(dead_code)]
    runtime: Arc<Runtime>,
    bridge_addr: Arc<Mutex<String>>,
    download_dir: String,
    /// One stop channel per running task. Sending `true` asks the engine to pause at its
    /// next read; the engine then commits what it wrote and reports `Paused`.
    stops: Arc<Mutex<HashMap<u64, tokio::sync::watch::Sender<bool>>>>,

    #[allow(dead_code)]
    handle: Arc<Mutex<Option<omad_rpc::BridgeHandle>>>,
}

#[derive(Default)]
struct ServiceState {
    notifier: Option<Notifier>,
    referer: Option<String>,
    origin: Option<String>,
}

impl Service {
    fn start() -> anyhow::Result<Self> {
        let dirs = Paths::discover()?;
        let store = Store::open(dirs.state.join("omalldownloader.sqlite3"))?;
        let root = omad_core::path::DownloadRoot::new(&dirs.downloads)?;

        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()?,
        );

        let board = Arc::new(TaskBoard::default());
        for record in store.list()? {
            board.update_from_record(&record);
        }

        let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let state = Arc::new(Mutex::new(ServiceState::default()));
        let bridge_addr = Arc::new(Mutex::new("not started".to_string()));
        let stops: Arc<Mutex<HashMap<u64, tokio::sync::watch::Sender<bool>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // The bridge listens on one fixed loopback port, so a fresh extension connects
        // with nothing to type; the store keeps the secret each origin was given.
        let secrets: HashMap<String, String> = store
            .paired_origins()?
            .into_iter()
            .filter(|(_, secret)| !secret.is_empty())
            .collect();
        let bridge_host = Arc::new(WindowBridge {
            board: board.clone(),
            commands: commands.clone(),
            store: store.clone(),
        });
        let bridge = runtime.block_on(async {
            Bridge::start(
                BridgeConfig {
                    bind: bridge_bind(),
                    paired_origins: secrets.keys().cloned().collect(),
                    trust_first_origin: true,
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                },
                bridge_host,
                secrets,
            )
            .await
        })?;
        *bridge_addr.lock().unwrap() = bridge.addr().to_string();
        // Hold the handle: dropping it shuts the bridge down.
        let handle: Arc<Mutex<Option<omad_rpc::BridgeHandle>>> = Arc::new(Mutex::new(Some(bridge)));

        let service = Self {
            board: board.clone(),
            store: store.clone(),
            commands: commands.clone(),
            state: state.clone(),
            runtime: runtime.clone(),
            bridge_addr,
            download_dir: dirs.downloads.to_string_lossy().to_string(),
            stops: stops.clone(),
            handle,
        };

        // The engine thread: one task at a time, in the order commands arrived.
        let sink = Arc::new(BoardSink {
            board: board.clone(),
            notifier: Mutex::new(None),
        });
        let engine_store = store.clone();
        let engine_root = root.clone();
        let engine_board = board.clone();
        let stops_for_engine = stops.clone();
        let wake_service = service.clone();
        runtime.spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    Command::Add {
                        url,
                        filename,
                        origin,
                        referer,
                        allow_private,
                    } => {
                        let target = EngineTarget {
                            store: engine_store.clone(),
                            board: engine_board.clone(),
                            root: engine_root.clone(),
                            sink: sink.clone() as Arc<dyn StoreSink>,
                            stops: stops_for_engine.clone(),
                        };
                        if let Err(error) =
                            add_and_run(target, url, filename, origin, referer, allow_private).await
                        {
                            tracing::warn!(%error, "a task could not be added");
                        }
                        wake_service.wake();
                    }
                    Command::Pause(id) => {
                        // Ask the engine to stop at its next read. It commits what it wrote
                        // and reports `Paused`, which is what lands in the record; a task
                        // that is not running here (added by the CLI, say) is marked
                        // paused directly.
                        let sender = stops_for_engine.lock().unwrap().get(&id.0).cloned();
                        match sender {
                            Some(sender) => {
                                let _ = sender.send(true);
                            }
                            None => {
                                let _ = engine_store.set_status(id, TaskStatus::Paused, None);
                                engine_board.set_status(id.0, TaskStatus::Paused, None);
                            }
                        }
                        wake_service.wake();
                    }
                    Command::Resume(id) => {
                        if let Ok(Some(record)) = engine_store.get(id) {
                            let target = EngineTarget {
                                store: engine_store.clone(),
                                board: engine_board.clone(),
                                root: engine_root.clone(),
                                sink: sink.clone() as Arc<dyn StoreSink>,
                                stops: stops_for_engine.clone(),
                            };
                            if let Err(error) = resume_existing(target, record).await {
                                tracing::warn!(%error, "a task could not be resumed");
                            }
                            wake_service.wake();
                        }
                    }
                    Command::Remove(id) => {
                        if let Some(sender) = stops_for_engine.lock().unwrap().get(&id.0) {
                            let _ = sender.send(true);
                        }
                        let _ = engine_store.delete(id);
                        engine_board.remove(id.0);
                        wake_service.wake();
                        // The task's files stay on disk on purpose (R2): removing a task
                        // is not permission to delete a file the user may still want.
                    }
                }
            }
        });

        // The store is shared with `omad` and with the bar widget: mirror what others wrote
        // so a task added from the command line appears here without a restart.
        {
            let store = store.clone();
            let board = board.clone();
            let wake = service.clone();
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    if let Ok(records) = store.list() {
                        board.adopt(&records);
                        wake.wake();
                    }
                }
            });
        }

        // Pick up where the last run stopped.
        for record in store.resumable()? {
            let _ = commands.send(Command::Resume(record.id));
        }

        Ok(service)
    }

    fn wake(&self) {
        if let Some(notifier) = self.state.lock().unwrap().notifier.clone() {
            notifier();
        }
    }
}

struct Paths {
    state: PathBuf,
    downloads: PathBuf,
}

impl Paths {
    fn discover() -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));
        // The task list is data, not state: `XDG_STATE_HOME` belongs to the theme
        // adapter, which points it at its own copy of Omarchy's theme.
        let state = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("omalldownloader");
        let downloads = std::env::var_os("XDG_DOWNLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Downloads"));
        if !downloads.exists() {
            std::fs::create_dir_all(&downloads)?;
        }
        Ok(Self { state, downloads })
    }
}

#[derive(Clone)]
struct EngineTarget {
    store: Store,
    board: Arc<TaskBoard>,
    root: omad_core::path::DownloadRoot,
    sink: Arc<dyn StoreSink>,
    stops: Arc<Mutex<HashMap<u64, tokio::sync::watch::Sender<bool>>>>,
}

async fn add_and_run(
    target: EngineTarget,
    url: String,
    filename: Option<String>,
    origin: Option<String>,
    referer: Option<String>,
    allow_private: bool,
) -> anyhow::Result<()> {
    // An ed2k link is not a URL this program can hold; a local client downloads it instead.
    if omad_engine::packs::is_ed2k(&url) {
        omad_engine::packs::delegate_ed2k(&url).map_err(|error| anyhow::anyhow!("{error}"))?;
        return Ok(());
    }
    let parsed = url::Url::parse(&url)?;
    let name = choose_name(&parsed, filename, allow_private).await;
    // The path is built from a validated root and a sanitised name; the caller never
    // supplies either (R2).
    let path = target.root.resolve(None, &name)?;
    let connections = 8u8;
    // A link that came from the browser may not reach loopback or the LAN, unless the user
    // asked for it. `OMAD_ALLOW_PRIVATE` is the escape hatch for a local test server.
    let allow_private = allow_private || std::env::var_os("OMAD_ALLOW_PRIVATE").is_some();

    let record = target.store.create(NewTask {
        url: url.clone(),
        filename: name,
        dest: "downloads".into(),
        file_path: path.to_string_lossy().to_string(),
        connections,
        allow_private,
        referer: referer.clone(),
    })?;
    target.board.update_from_record(&record);
    let _ = origin;

    run_task(
        target,
        RunRequest {
            id: record.id,
            url: parsed,
            path,
            resume: None,
            connections,
            allow_private,
            referer,
        },
    )
    .await
}

async fn resume_existing(target: EngineTarget, record: TaskRecord) -> anyhow::Result<()> {
    let parsed = url::Url::parse(&record.url)?;
    let plan = target
        .store
        .plan_of(record.id)?
        .and_then(|(plan, _)| plan)
        .and_then(|json| Plan::from_json(&json));
    let path = PathBuf::from(&record.file_path);
    let connections = record.connections;
    let allow_private = record.allow_private;
    run_task(
        target,
        RunRequest {
            id: record.id,
            url: parsed,
            path,
            resume: plan,
            connections,
            allow_private,
            referer: record.referer.clone(),
        },
    )
    .await
}

/// Wraps the sink so every durable write goes to the store and every visible change to
/// the board. The engine decides *when*; this decides *where*.
struct Commit {
    store: Store,
    board: Arc<TaskBoard>,
    sink: Arc<dyn StoreSink>,
}

impl StoreSink for Commit {
    fn on_plan(&self, id: TaskId, probe: &omad_engine::http::Probe, plan: &Plan, connections: u8) {
        let _ = self.store.set_plan(
            id,
            probe.total_bytes,
            probe.etag.as_deref(),
            probe.sha256.as_deref(),
            &plan.to_json(),
            connections,
        );
        let _ = self.store.set_status(id, TaskStatus::Running, None);
        self.board.mark_owned(id.0);
        self.board.set_status(id.0, TaskStatus::Running, None);
        self.sink.on_plan(id, probe, plan, connections);
    }
    fn on_progress(&self, id: TaskId, progress: &Progress) {
        self.sink.on_progress(id, progress);
    }
    fn on_commit(&self, id: TaskId, received: u64, total: Option<u64>, speed: u64) {
        let _ = self.store.set_progress(id, received, total, speed);
    }
    fn on_finished(&self, id: TaskId, outcome: Outcome, recorded: u64, total: Option<u64>) {
        let _ = self.store.set_progress(id, recorded, total, 0);
        match outcome {
            Outcome::Completed => {
                let _ = self.store.set_status(id, TaskStatus::Completed, None);
                self.board.set_status(id.0, TaskStatus::Completed, None);
            }
            Outcome::Paused => {
                let _ = self.store.set_status(id, TaskStatus::Paused, None);
                self.board.set_status(id.0, TaskStatus::Paused, None);
            }
        }
        self.board.release(id.0);
        self.sink.on_finished(id, outcome, recorded, total);
    }
    fn on_failed(&self, id: TaskId, error: &str, resumable: bool) {
        let _ = self.store.set_status(id, TaskStatus::Failed, Some(error));
        self.board
            .set_status(id.0, TaskStatus::Failed, Some(error.to_string()));
        self.sink.on_failed(id, error, resumable);
    }
}

/// Everything a run needs that is not the target itself.
struct RunRequest {
    id: TaskId,
    url: url::Url,
    path: PathBuf,
    resume: Option<Plan>,
    connections: u8,
    allow_private: bool,
    referer: Option<String>,
}

async fn run_task(target: EngineTarget, request: RunRequest) -> anyhow::Result<()> {
    let RunRequest {
        id,
        url,
        path,
        resume,
        connections,
        allow_private,
        referer,
    } = request;
    let engine = Engine::new(EngineConfig {
        max_connections: connections,
        chunk_size: 4 * 1024 * 1024,
        policy: UrlPolicy { allow_private },
        user_agent: std::env::var("OMAD_USER_AGENT").ok(),
        referer,
    })?;
    let commit = Arc::new(Commit {
        store: target.store.clone(),
        board: target.board.clone(),
        sink: target.sink.clone(),
    });
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    target.stops.lock().unwrap().insert(id.0, stop_tx);
    let stops = target.stops.clone();
    let outcome = engine
        .run_task(id, url, path, resume, connections, commit, stop_rx, None)
        .await;
    stops.lock().unwrap().remove(&id.0);
    outcome?;
    Ok(())
}

impl EngineHost for Service {
    fn rows(&self) -> Vec<Row> {
        self.board.rows()
    }

    fn add(&self, url: String) {
        let state = self.state.lock().unwrap();
        let _ = self.commands.send(Command::Add {
            url,
            filename: None,
            origin: state.origin.clone(),
            referer: state.referer.clone(),
            // A link typed into the window is the user's own request, so loopback and the
            // local network are allowed; a link from the browser is not.
            allow_private: state.origin.is_none(),
        });
    }

    fn pause(&self, id: u64) {
        let _ = self.commands.send(Command::Pause(TaskId(id)));
    }

    fn resume(&self, id: u64) {
        let _ = self.commands.send(Command::Resume(TaskId(id)));
    }

    fn remove(&self, id: u64) {
        let _ = self.commands.send(Command::Remove(TaskId(id)));
    }

    fn pause_all(&self) {
        // One signal for every task that is running in this process.
        for sender in self.stops.lock().unwrap().values() {
            let _ = sender.send(true);
        }
        for row in self.board.rows() {
            if matches!(row.status, TaskStatus::Queued) {
                self.pause(row.id);
            }
        }
    }

    fn resume_all(&self) {
        for row in self.board.rows() {
            if matches!(row.status, TaskStatus::Paused | TaskStatus::Failed) {
                self.resume(row.id);
            }
        }
    }

    fn reveal(&self, id: u64) {
        let Some(path) = self.board.path_of(id) else {
            return;
        };
        let dir = std::path::Path::new(&path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(&path));
        open_with_desktop(&dir);
    }

    fn open_file(&self, id: u64) {
        let Some(path) = self.board.path_of(id) else {
            return;
        };
        open_with_desktop(std::path::Path::new(&path));
    }

    fn download_dir(&self) -> String {
        self.download_dir.clone()
    }

    fn paired_browsers(&self) -> usize {
        self.store
            .paired_origins()
            .map(|origins| origins.len())
            .unwrap_or(0)
    }

    fn bridge_summary(&self) -> String {
        format!("ws://{}", self.bridge_addr.lock().unwrap())
    }

    fn set_notifier(&self, notifier: Notifier) {
        self.state.lock().unwrap().notifier = Some(notifier);
    }

    fn set_referer(&self, referer: Option<String>) {
        self.state.lock().unwrap().referer = referer;
    }

    fn referer(&self) -> Option<String> {
        self.state.lock().unwrap().referer.clone()
    }

    fn set_origin(&self, origin: Option<String>) {
        self.state.lock().unwrap().origin = origin;
    }

    fn origin(&self) -> Option<String> {
        self.state.lock().unwrap().origin.clone()
    }
}

/// `OMAD_WINDOW=WxH` gives the window a fixed size. Absent or unparsable, the window is
/// the compositor's to place.
fn window_bounds_from_env() -> Option<odk::ui::gpui::WindowBounds> {
    let text = std::env::var("OMAD_WINDOW").ok()?;
    let (width, height) = text.split_once('x')?;
    let size = odk::ui::gpui::Size {
        width: odk::ui::gpui::px(width.trim().parse().ok()?),
        height: odk::ui::gpui::px(height.trim().parse().ok()?),
    };
    Some(odk::ui::gpui::WindowBounds::Windowed(
        odk::ui::gpui::Bounds {
            origin: odk::ui::gpui::point(odk::ui::gpui::px(0.0), odk::ui::gpui::px(0.0)),
            size,
        },
    ))
}

/// The name a task will be saved under.
///
/// The browser's own name wins: it is what the user saw in the browser's download bar, and
/// it is often the only name a link has (`.../download?id=7` has none). Next the URL, then
/// whatever the server says in `Content-Disposition` — asked for with the same `Referer` a
/// browser would send, because a host that guards its files refuses the question otherwise.
async fn choose_name(url: &url::Url, given: Option<String>, allow_private: bool) -> String {
    if let Some(given) = given
        .map(|name| name.trim().to_string())
        .filter(|n| !n.is_empty())
    {
        let clean = omad_core::filename::sanitize(&given);
        if !clean.is_empty() {
            return clean;
        }
    }

    let from_url = omad_core::filename::from_url(url);
    let placeholder = from_url.is_empty()
        || from_url.eq_ignore_ascii_case("download")
        || from_url.eq_ignore_ascii_case("file");
    if !placeholder {
        return from_url;
    }

    let policy = UrlPolicy { allow_private };
    match omad_engine::probe(url, &policy).await {
        Ok(outcome) => match outcome.probe.suggested_filename {
            Some(name) => {
                let clean = omad_core::filename::sanitize(&name);
                if !clean.is_empty() {
                    return clean;
                }
                from_url
            }
            None => from_url,
        },
        Err(_) => from_url,
    }
}

/// Where the extension bridge listens.
///
/// One fixed loopback port, so a freshly installed extension finds the app without being
/// told anything. `OMAD_BRIDGE_PORT` overrides it; a port of 0 asks the system to choose,
/// which is what the fallback does when the fixed one is taken.
fn bridge_bind() -> std::net::SocketAddr {
    let port = std::env::var("OMAD_BRIDGE_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(omad_rpc::server::DEFAULT_PORT);
    std::net::SocketAddr::from(([127, 0, 0, 1], port))
}

/// Hands a path to the desktop's own opener: xdg-open, `open` on macOS, `explorer` on
/// Windows. A failure is logged, never shown as a crash.
fn open_with_desktop(path: &std::path::Path) {
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";

    if let Err(error) = std::process::Command::new(program).arg(path).spawn() {
        tracing::warn!(%error, path = %path.display(), "could not open the path");
    }
}

/// The bridge's view of the application.
struct WindowBridge {
    board: Arc<TaskBoard>,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    /// Kept so a first-contact secret can be written down.
    store: Store,
}

impl BridgeHost for WindowBridge {
    fn remember_origin(&self, origin: &str, secret: &str) {
        // Persisting here is what makes "connect once" survive a restart of either side.
        if let Err(error) = self.store.remember_origin(origin, secret) {
            tracing::warn!(%error, origin, "could not remember a paired origin");
        }
    }

    fn list(&self, _origin: &str) -> Vec<TaskView> {
        self.board
            .rows()
            .into_iter()
            .map(|row| TaskView {
                id: TaskId(row.id),
                url: row.url,
                filename: row.name,
                status: row.status,
                received_bytes: row.received,
                total_bytes: row.total,
                speed: row.speed,
                error: row.error,
            })
            .collect()
    }

    fn add(&self, origin: &str, task: AddTask) -> Result<TaskView, String> {
        // The bridge checked the origin against a paired profile before calling this;
        // it is recorded on the task and shown to the user.
        self.commands
            .send(Command::Add {
                url: task.url.clone(),
                filename: task.filename.clone(),
                origin: Some(origin.to_string()),
                referer: task.referer.clone(),
                allow_private: false,
            })
            .map_err(|e| e.to_string())?;
        Ok(TaskView {
            id: TaskId(0),
            url: task.url,
            filename: task.filename.unwrap_or_default(),
            status: TaskStatus::Queued,
            received_bytes: 0,
            total_bytes: None,
            speed: 0,
            error: None,
        })
    }

    fn pause(&self, _origin: &str, id: TaskId) -> Result<(), String> {
        self.commands
            .send(Command::Pause(id))
            .map_err(|e| e.to_string())
    }

    fn resume(&self, _origin: &str, id: TaskId) -> Result<(), String> {
        self.commands
            .send(Command::Resume(id))
            .map_err(|e| e.to_string())
    }

    fn remove(&self, _origin: &str, id: TaskId) -> Result<(), String> {
        self.commands
            .send(Command::Remove(id))
            .map_err(|e| e.to_string())
    }
}
