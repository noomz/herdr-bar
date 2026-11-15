//! Rows from the other machines saved in Herdr (`herdr machine list`).
//!
//! Each machine answers over SSH through `herdr --machine <label>`, which
//! takes a few hundred milliseconds when it answers at all, so none of it
//! runs on the input loop: one thread lists the machines, then each fetch
//! and each pane read runs on its own thread with the configured deadline
//! and reports back over a channel the bar drains between keys. A machine
//! is never asked twice at once.

use crate::app::{monotonic, Pool};
use crate::client::HerdrClient;
use crate::config::{Config, MachineSources};
use crate::opener::herdr_bin;
use crate::pyjson::{self, Value};
use crate::pystr;
use std::collections::HashSet;
use std::sync::mpsc::{channel, Receiver, Sender};

/// Seconds between snapshots of the same machine; local rows refresh faster.
pub const REFRESH: f64 = 3.0;

/// Something a background thread brought back.
pub enum Arrival {
    /// A machine's rows, or why it could not be read.
    Rows(String, std::result::Result<Pool, String>),
    /// A remote pane's visible text, keyed `pane@label`; empty on failure.
    Preview(String, String),
}

enum Message {
    Labels(Vec<String>),
    Arrival(Arrival),
}

pub struct Machines {
    sources: MachineSources,
    config: Config,
    /// None until the machine list comes back.
    labels: Option<Vec<String>>,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    fetching: HashSet<String>,
    reading: HashSet<String>,
    next_fetch: f64,
}

impl Machines {
    /// Start listing the machines `config` asks for; None when it asks for none.
    pub fn start(config: &Config) -> Option<Machines> {
        let sources = config.machines.clone()?;
        let (sender, receiver) = channel();
        let list = sender.clone();
        let timeout_ms = sources.timeout_ms;
        let wanted = sources.clone();
        std::thread::spawn(move || {
            let labels = herdr(&["machine", "list", "--json"], timeout_ms)
                .ok()
                .and_then(|out| pyjson::loads(pystr::strip(&String::from_utf8_lossy(&out))).ok())
                .map(|machines| enabled_labels(&machines, &wanted))
                .unwrap_or_default();
            let _ = list.send(Message::Labels(labels));
        });
        Some(Machines {
            sources,
            config: config.clone(),
            labels: None,
            sender,
            receiver,
            fetching: HashSet::new(),
            reading: HashSet::new(),
            next_fetch: 0.0,
        })
    }

    /// When the bar should next come back for this: soon while anything is
    /// out, else at the next round of snapshots.
    pub fn wake_at(&self) -> f64 {
        if self.labels.is_none() || !self.fetching.is_empty() || !self.reading.is_empty() {
            return monotonic() + 0.05;
        }
        self.next_fetch
    }

    /// Start whatever is due and hand back whatever has arrived. Never waits.
    pub fn pump(&mut self) -> Vec<Arrival> {
        let mut arrivals = Vec::new();
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Labels(labels) => self.labels = Some(labels),
                Message::Arrival(arrival) => {
                    match &arrival {
                        Arrival::Rows(label, _) => self.fetching.remove(label),
                        Arrival::Preview(key, _) => self.reading.remove(key),
                    };
                    arrivals.push(arrival);
                }
            }
        }
        let now = monotonic();
        if now >= self.next_fetch {
            if let Some(labels) = &self.labels {
                self.next_fetch = now + REFRESH;
                for label in labels {
                    if self.fetching.insert(label.clone()) {
                        let (label, config, sender) = (label.clone(), self.config.clone(), self.sender.clone());
                        let timeout_ms = self.sources.timeout_ms;
                        std::thread::spawn(move || {
                            let rows = fetch(&label, &config, timeout_ms);
                            let _ = sender.send(Message::Arrival(Arrival::Rows(label, rows)));
                        });
                    }
                }
            }
        }
        arrivals
    }

    /// Read a remote pane in the background; arrives as `Arrival::Preview`.
    pub fn read_pane(&mut self, label: &str, pane_id: &str, lines: i64) {
        let key = format!("{}@{}", pane_id, label);
        if !self.reading.insert(key.clone()) {
            return;
        }
        let args = on_machine(label, &["pane", "read", pane_id, "--source", "visible", "--lines", &lines.to_string()]);
        let (sender, timeout_ms) = (self.sender.clone(), self.sources.timeout_ms);
        std::thread::spawn(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let text = herdr(&args, timeout_ms).map(|out| String::from_utf8_lossy(&out).into_owned()).unwrap_or_default();
            let _ = sender.send(Message::Arrival(Arrival::Preview(key, text)));
        });
    }
}

/// `args` run on the machine `label`.
pub fn on_machine(label: &str, args: &[&str]) -> Vec<String> {
    ["--machine", label].iter().chain(args).map(|arg| arg.to_string()).collect()
}

/// The labels of the enabled machines `sources` includes, from
/// `machine list --json`.
fn enabled_labels(machines: &Value, sources: &MachineSources) -> Vec<String> {
    let Value::List(machines) = machines else { return Vec::new() };
    machines
        .iter()
        .filter(|machine| machine.get("enabled").is_true())
        .filter_map(|machine| machine.get("label").as_str())
        .filter(|label| !label.is_empty() && sources.includes(label))
        .map(String::from)
        .collect()
}

fn fetch(label: &str, config: &Config, timeout_ms: u64) -> std::result::Result<Pool, String> {
    let args = on_machine(label, &["api", "snapshot"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = herdr(&args, timeout_ms)?;
    pool_of(label, &String::from_utf8_lossy(&out), config)
}

/// A machine's `api snapshot` answer as its rows.
fn pool_of(label: &str, answer: &str, config: &Config) -> std::result::Result<Pool, String> {
    let lines = pystr::splitlines(pystr::strip(answer));
    let response = pyjson::loads(lines.last().copied().unwrap_or("")).map_err(|_| "unreadable snapshot".to_string())?;
    let snapshot = HerdrClient::unwrap(response).and_then(HerdrClient::snapshot_of).map_err(|error| error.message)?;
    let boot_id = snapshot.get("boot_id").as_str().filter(|id| !id.is_empty());
    Ok(Pool::build(&snapshot, config).on_machine(label, boot_id))
}

/// Run the Herdr CLI with a deadline: its stdout, or why it failed.
fn herdr(args: &[&str], timeout_ms: u64) -> std::result::Result<Vec<u8>, String> {
    let mut argv = vec![herdr_bin()];
    argv.extend(args.iter().map(|arg| arg.to_string()));
    match crate::proc::run(&argv, timeout_ms as f64 / 1000.0) {
        Ok(output) if output.success => Ok(output.stdout),
        Ok(output) => {
            let err = String::from_utf8_lossy(&output.stderr);
            let err = pystr::strip(&err);
            Err(if err.is_empty() { "herdr failed".to_string() } else { err.to_string() })
        }
        Err(crate::proc::RunError::Timeout) => Err("timed out".into()),
        Err(crate::proc::RunError::Spawn(error)) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(include: &[&str]) -> MachineSources {
        MachineSources { include: include.iter().map(|s| s.to_string()).collect(), timeout_ms: 1500 }
    }

    #[test]
    fn only_enabled_included_machines_are_asked() {
        let machines = pyjson::loads(
            r#"[{"label": "drm", "enabled": true}, {"label": "omarchy", "enabled": true},
                {"label": "spare", "enabled": false}, {"label": "", "enabled": true}, {"enabled": true}]"#,
        )
        .unwrap();
        assert_eq!(enabled_labels(&machines, &sources(&["*"])), vec!["drm", "omarchy"]);
        assert_eq!(enabled_labels(&machines, &sources(&["omarchy", "spare"])), vec!["omarchy"]);
        assert!(enabled_labels(&Value::Null, &sources(&["*"])).is_empty());
    }

    #[test]
    fn commands_run_on_the_machine() {
        assert_eq!(on_machine("drm", &["tab", "focus", "w1:t4"]), vec!["--machine", "drm", "tab", "focus", "w1:t4"]);
    }

    #[test]
    fn a_snapshot_answer_becomes_that_machines_rows() {
        let snapshot = pyjson::dumps(&crate::testutil::snapshot(), false);
        let answer = format!(r#"{{"id": "cli:api:snapshot", "result": {{"snapshot": {}}}}}"#, snapshot);
        let pool = pool_of("drm", &answer, &Config::default()).unwrap();
        assert!(!pool.items.is_empty());
        assert!(pool.items.iter().all(|item| item.machine.as_deref() == Some("drm") && item.key.ends_with("@drm")));
        assert!(pool_of("drm", "garbage", &Config::default()).is_err());
        assert!(pool_of("drm", r#"{"result": {}}"#, &Config::default()).is_err());
    }
}
