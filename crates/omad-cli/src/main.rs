//! `omad`: the OmaLLDownloader command line.
//!
//! It owns a task list of its own — the same store the application uses — so it works
//! with the window open or closed. Two commands matter for scripting: `probe` asks what a
//! URL is without downloading it, and `wait` blocks until a task finishes, which makes
//! `omad add ... && omad wait ...` a working pipeline.

use clap::{Parser, Subcommand};
use omad_core::ssrf::UrlPolicy;
use omad_core::{TaskId, TaskStatus, filename};
use omad_engine::engine::Plan;
use omad_engine::http::HttpProbe;
use omad_engine::{Engine, EngineConfig, Outcome, Progress, StoreSink};
use omad_store::{NewTask, Store, TaskRecord};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "omad",
    version,
    about = "OmaLLDownloader: a download manager from the command line"
)]
struct Cli {
    /// Where to save. Defaults to $XDG_DOWNLOAD_DIR, else ~/Downloads.
    #[arg(long, global = true)]
    dir: Option<PathBuf>,
    /// Reach loopback and private addresses. Refused by default: a download manager is an
    /// easy way to make a request on someone else's behalf.
    #[arg(long, global = true)]
    allow_private_network: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// What a URL is: size, name, whether ranges work. Downloads nothing.
    Probe {
        url: String,
        /// Raw response headers as well.
        #[arg(long)]
        headers: bool,
    },
    /// Add a task and download it now.
    Add {
        url: String,
        /// A bare file name to use instead of the server's.
        #[arg(long)]
        name: Option<String>,
        /// Connections to open, up to 16.
        #[arg(long, default_value_t = 8)]
        connections: u8,
    },
    /// List tasks.
    Tasks {
        /// Only tasks in this state.
        #[arg(long)]
        status: Option<String>,
        /// JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Show one task.
    Task {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Resume a paused or failed task.
    Resume { id: String },
    /// Mark a task paused.
    Pause { id: String },
    /// Remove a task and its record. The file is kept.
    Remove { id: String },
    /// Block until a task finishes, then report its outcome.
    Wait {
        id: String,
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("omad: {error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let paths = Paths::discover(cli.dir.as_ref())?;
    let store = Store::open(paths.state.join("omalldownloader.sqlite3"))?;
    let root = omad_core::path::DownloadRoot::new(&paths.downloads)?;
    let allow_private_network = cli.allow_private_network;
    let policy = if allow_private_network {
        UrlPolicy::permissive()
    } else {
        UrlPolicy::default()
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match cli.command {
        Command::Probe { url, headers } => {
            let url = parse_url(&url)?;
            let probe = HttpProbe::new()?;
            let outcome = runtime.block_on(async {
                use omad_engine::http::ProbeSource;
                probe.probe(&url, &policy).await
            })?;
            let p = &outcome.probe;
            println!("url:        {}", p.final_url);
            println!("status:     {}", outcome.status);
            println!(
                "size:       {}",
                p.total_bytes
                    .map(human_bytes)
                    .unwrap_or_else(|| "unknown".into())
            );
            println!("ranges:     {}", if p.ranges { "yes" } else { "no" });
            println!(
                "name:       {}",
                p.suggested_filename
                    .clone()
                    .unwrap_or_else(|| filename::from_url(&p.final_url))
            );
            println!(
                "etag:       {}",
                p.etag.clone().unwrap_or_else(|| "-".into())
            );
            println!(
                "type:       {}",
                p.content_type.clone().unwrap_or_else(|| "-".into())
            );
            if let Some(sha) = &p.sha256 {
                println!("sha-256:    {sha}");
            }
            if headers {
                println!();
                for (name, value) in &outcome.headers {
                    println!("{name}: {value}");
                }
            }
        }

        Command::Add {
            url,
            name,
            connections,
        } => {
            let parsed = parse_url(&url)?;
            // The server usually names the file better than the URL does; a name on the
            // command line beats both. Either way it goes through `sanitize`.
            let proposed = match name {
                Some(name) => filename::sanitize(&name),
                None => match runtime.block_on(omad_engine::probe(&parsed, &policy)) {
                    Ok(outcome) => outcome
                        .probe
                        .suggested_filename
                        .unwrap_or_else(|| filename::from_url(&outcome.probe.final_url)),
                    // A probe that fails is the download's business to report; naming the
                    // file after the URL is still the right guess.
                    Err(_) => filename::from_url(&parsed),
                },
            };
            // The path comes from a validated root and a sanitised name, never from the
            // command line as a path.
            let path = root.resolve(None, &deduplicate(&root, &proposed))?;
            let record = store.create(NewTask {
                url: url.clone(),
                filename: path.file_name().unwrap().to_string_lossy().to_string(),
                dest: "downloads".into(),
                file_path: path.to_string_lossy().to_string(),
                connections: connections.clamp(1, omad_engine::engine::MAX_CONNECTIONS),
                allow_private: allow_private_network,
            })?;
            println!("added {}", record.id);
            let outcome = download(&runtime, &store, record, parsed, path.clone(), policy)?;
            println!("{}", outcome_label(outcome));
            if outcome == Outcome::Completed {
                println!("saved to {}", path.display());
            } else {
                std::process::exit(2);
            }
        }

        Command::Tasks { status, json } => {
            let records = store.list()?;
            let records: Vec<TaskRecord> = match status.as_deref() {
                Some(wanted) => {
                    let wanted = TaskStatus::from_storage(wanted)
                        .ok_or_else(|| anyhow::anyhow!("unknown status {wanted:?}"))?;
                    records.into_iter().filter(|r| r.status == wanted).collect()
                }
                None => records,
            };
            if json {
                let rows: Vec<_> = records
                    .iter()
                    .map(|r| serde_json::to_value(r.snapshot()).unwrap())
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if records.is_empty() {
                println!("no tasks");
            } else {
                println!(
                    "{:<10} {:<10} {:>12} {:<28}",
                    "ID", "STATE", "PROGRESS", "NAME"
                );
                for r in &records {
                    println!(
                        "{:<10} {:<10} {:>12} {}",
                        r.id.to_string(),
                        r.status.as_str(),
                        progress_text(r),
                        r.filename
                    );
                }
            }
        }

        Command::Task { id, json } => {
            let id = parse_id(&id)?;
            let record = store
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("no task {id}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&record.snapshot())?);
            } else {
                println!("id:       {}", record.id);
                println!("url:      {}", record.url);
                println!("name:     {}", record.filename);
                println!("state:    {}", record.status.as_str());
                println!("progress: {}", progress_text(&record));
                println!("file:     {}", record.file_path);
                if let Some(error) = &record.error {
                    println!("error:    {error}");
                }
            }
        }

        Command::Resume { id } => {
            let id = parse_id(&id)?;
            let record = store
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("no task {id}"))?;
            let parsed = parse_url(&record.url)?;
            let path = PathBuf::from(&record.file_path);
            let outcome = download(&runtime, &store, record, parsed, path.clone(), policy)?;
            println!("{}", outcome_label(outcome));
        }

        Command::Pause { id } => {
            let id = parse_id(&id)?;
            store.set_status(id, TaskStatus::Paused, None)?;
            println!("paused {id}");
        }

        Command::Remove { id } => {
            let id = parse_id(&id)?;
            store.delete(id)?;
            println!("removed {id} (the file was kept)");
        }

        Command::Wait { id, timeout } => {
            let id = parse_id(&id)?;
            let start = std::time::Instant::now();
            loop {
                let record = store
                    .get(id)?
                    .ok_or_else(|| anyhow::anyhow!("no task {id}"))?;
                if record.status.is_terminal() || record.status == TaskStatus::Failed {
                    println!("{} {}", record.id, record.status.as_str());
                    if record.status == TaskStatus::Failed {
                        std::process::exit(2);
                    }
                    return Ok(());
                }
                if let Some(limit) = timeout
                    && start.elapsed().as_secs() >= limit
                {
                    anyhow::bail!("timed out waiting for {id}");
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    Ok(())
}

fn download(
    runtime: &tokio::runtime::Runtime,
    store: &Store,
    record: TaskRecord,
    url: url::Url,
    path: PathBuf,
    policy: UrlPolicy,
) -> anyhow::Result<Outcome> {
    // A task remembers whether the user allowed a private address; asking again on every
    // resume would be a worse experience than trusting the record.
    let policy = if record.allow_private {
        UrlPolicy::permissive()
    } else {
        policy
    };
    let engine = Engine::new(EngineConfig {
        max_connections: record.connections,
        chunk_size: 4 * 1024 * 1024,
        policy,
        user_agent: None,
    })?;

    // A record the engine can consult on a crash: same file, same plan.
    let sink: Arc<dyn StoreSink> = Arc::new(CliSink {
        store: store.clone(),
    });
    let id = record.id;
    let plan = store
        .plan_of(record.id)?
        .and_then(|(plan, _)| plan)
        .and_then(|json| Plan::from_json(&json))
        .filter(|_| record.status != TaskStatus::Completed);

    // `omad pause` runs in another process, so this one watches the record: a pause
    // written there by anyone stops this transfer at its next read.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let watcher_store = store.clone();
    let watcher = runtime.spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            match watcher_store.get(id) {
                Ok(Some(current)) if current.status == TaskStatus::Paused => {
                    let _ = stop_tx.send(true);
                    return;
                }
                Ok(None) => return,
                _ => {}
            }
        }
    });

    let outcome = runtime.block_on(async {
        engine
            .run_task(record.id, url, path, plan, record.connections, sink, stop_rx, None)
            .await
    });
    watcher.abort();
    Ok(outcome?)
}

/// Writes the record the way the engine's invariants require: on commit, and only with
/// the sum of acknowledged chunk progress.
struct CliSink {
    store: Store,
}

impl StoreSink for CliSink {
    fn on_plan(&self, id: TaskId, probe: &omad_engine::http::Probe, plan: &Plan, c: u8) {
        let _ = self.store.set_plan(
            id,
            probe.total_bytes,
            probe.etag.as_deref(),
            probe.sha256.as_deref(),
            &plan.to_json(),
            c,
        );
        let _ = self.store.set_status(id, TaskStatus::Running, None);
    }
    fn on_progress(&self, _id: TaskId, _progress: &Progress) {}
    fn on_commit(&self, id: TaskId, received: u64, total: Option<u64>, speed: u64) {
        let _ = self.store.set_progress(id, received, total, speed);
    }
    fn on_finished(&self, id: TaskId, outcome: Outcome, recorded: u64, total: Option<u64>) {
        let _ = self.store.set_progress(id, recorded, total, 0);
        let status = match outcome {
            Outcome::Completed => TaskStatus::Completed,
            Outcome::Paused => TaskStatus::Paused,
        };
        let _ = self.store.set_status(id, status, None);
    }
    fn on_failed(&self, id: TaskId, error: &str, _resumable: bool) {
        let _ = self.store.set_status(id, TaskStatus::Failed, Some(error));
    }
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed => "completed",
        Outcome::Paused => "paused",
    }
}

fn parse_url(text: &str) -> anyhow::Result<url::Url> {
    let url = url::Url::parse(text).map_err(|e| anyhow::anyhow!("{text} is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("only http and https are supported, not {}", url.scheme());
    }
    Ok(url)
}

fn parse_id(text: &str) -> anyhow::Result<TaskId> {
    TaskId::parse(text).ok_or_else(|| anyhow::anyhow!("{text} is not a task id"))
}

fn deduplicate(root: &omad_core::path::DownloadRoot, name: &str) -> String {
    filename::deduplicate(name, |candidate| {
        root.resolve(None, candidate)
            .map(|p| p.exists())
            .unwrap_or(true)
    })
}

fn progress_text(record: &TaskRecord) -> String {
    match record.total_bytes {
        Some(total) if total > 0 => format!(
            "{}/{}",
            human_bytes(record.received_bytes),
            human_bytes(total)
        ),
        _ => human_bytes(record.received_bytes),
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

struct Paths {
    state: PathBuf,
    downloads: PathBuf,
}

impl Paths {
    fn discover(explicit: Option<&PathBuf>) -> anyhow::Result<Self> {
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
        let downloads = match explicit {
            Some(dir) => dir.clone(),
            None => std::env::var_os("XDG_DOWNLOAD_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("Downloads")),
        };
        std::fs::create_dir_all(&downloads)?;
        Ok(Self { state, downloads })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_accepted() {
        assert!(parse_url("https://example.com/a").is_ok());
        assert!(parse_url("http://example.com/a").is_ok());
        assert!(parse_url("file:///etc/passwd").is_err());
        assert!(parse_url("javascript:alert(1)").is_err());
        assert!(parse_url("not a url").is_err());
    }

    #[test]
    fn ids_are_accepted_with_or_without_the_prefix() {
        assert_eq!(parse_id("tsk_12").unwrap(), TaskId(12));
        assert_eq!(parse_id("12").unwrap(), TaskId(12));
        assert!(parse_id("nope").is_err());
    }

    #[test]
    fn bytes_are_shown_in_the_unit_that_fits() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }
}
