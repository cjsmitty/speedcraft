//! Owns at most one running engine and exposes start/stop/halt/resume plus a
//! status view. Safe to share across HTTP handlers.

use crate::config::Config;
use crate::engine::{Control, Recorder, RecorderStatus, Shared, StampedEvent, StatusSnapshot};
use crate::runtime::{
    build_engine, check_live_gates, losses_accepted, run_engine, Mode, LIVE_CONFIRM_PHRASE,
};
use crate::types::{now_ms, ExchangeId};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

struct Inner {
    state: EngineState,
    mode: Option<Mode>,
    since_ms: i64,
    last_error: Option<String>,
    shutdown_tx: Option<watch::Sender<bool>>,
    control_tx: Option<mpsc::Sender<Control>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

pub struct Supervisor {
    pub name: String,
    pub cfg_path: PathBuf,
    pub shared: Arc<Shared>,
    inner: Arc<Mutex<Inner>>,
    started: Instant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineInfo {
    pub state: EngineState,
    pub mode: Option<String>,
    pub since_ms: i64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigSummary {
    pub path: String,
    pub live_in_config: bool,
    pub live_env_set: bool,
    pub min_edge_bps: String,
    pub max_notional_quote: String,
    pub max_daily_loss_quote: String,
    pub pairs: Vec<String>,
    pub exchanges: Vec<String>,
    pub stop_file: String,
    pub config_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent: String,
    pub version: String,
    pub hostname: String,
    pub uptime_s: u64,
    pub now_ms: i64,
    pub engine: EngineInfo,
    pub config: ConfigSummary,
    pub stop_file_present: bool,
    pub snapshot: StatusSnapshot,
    pub recorder: RecorderStatus,
    /// Recordings available next to the config, newest first.
    pub recordings: Vec<RecordingInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingInfo {
    pub path: String,
    pub bytes: u64,
    pub modified_ms: i64,
}

impl Supervisor {
    pub fn new(name: impl Into<String>, cfg_path: PathBuf) -> Arc<Supervisor> {
        Arc::new(Supervisor {
            name: name.into(),
            cfg_path,
            shared: Shared::new(),
            inner: Arc::new(Mutex::new(Inner {
                state: EngineState::Stopped,
                mode: None,
                since_ms: now_ms(),
                last_error: None,
                shutdown_tx: None,
                control_tx: None,
                task: None,
            })),
            started: Instant::now(),
        })
    }

    pub fn load_config(&self) -> Result<Config> {
        Config::load(&self.cfg_path)
    }

    pub fn state(&self) -> EngineState {
        self.inner.lock().unwrap().state
    }

    /// Start the engine. `confirm` must equal the live phrase for live mode.
    pub fn start(self: &Arc<Self>, mode: Mode, confirm: Option<&str>) -> Result<()> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state != EngineState::Stopped {
                bail!("engine is {:?}; stop it first", inner.state);
            }
        }
        let cfg = self.load_config()?;
        if mode == Mode::Live {
            let phrase_ok = confirm
                .map(|c| c.trim() == LIVE_CONFIRM_PHRASE)
                .unwrap_or(false);
            if !phrase_ok {
                bail!("live mode requires typing the confirmation phrase exactly: {LIVE_CONFIRM_PHRASE}");
            }
            check_live_gates(&cfg, true)?;
        }
        let engine = build_engine(cfg, mode)?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (control_tx, control_rx) = mpsc::channel(32);
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state = EngineState::Starting;
            inner.mode = Some(mode);
            inner.since_ms = now_ms();
            inner.last_error = None;
            inner.shutdown_tx = Some(shutdown_tx);
            inner.control_tx = Some(control_tx);
        }
        // Reset the shared snapshot so a stale one from a previous run is not shown.
        *self.shared.status.write().unwrap() = StatusSnapshot {
            mode: mode.as_str().into(),
            started_ms: now_ms(),
            ..Default::default()
        };
        let shared = self.shared.clone();
        let inner = self.inner.clone();
        let inner_for_task = inner.clone();
        let task = tokio::spawn(async move {
            // The engine publishes its first snapshot after preflight; flip to
            // Running as soon as that happens.
            let watcher_inner = inner_for_task.clone();
            let watcher_shared = shared.clone();
            let watcher = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let published = watcher_shared.status.read().unwrap().updated_ms > 0;
                    if published {
                        let mut i = watcher_inner.lock().unwrap();
                        if i.state == EngineState::Starting {
                            i.state = EngineState::Running;
                        }
                        break;
                    }
                }
            });
            let result = run_engine(engine, shutdown_rx, Some(shared), Some(control_rx)).await;
            watcher.abort();
            let mut i = inner_for_task.lock().unwrap();
            if let Err(e) = result {
                tracing::error!(error = %e, "engine exited with error");
                i.last_error = Some(format!("{e:#}"));
            }
            i.state = EngineState::Stopped;
            i.shutdown_tx = None;
            i.control_tx = None;
            i.task = None;
            i.since_ms = now_ms();
        });
        inner.lock().unwrap().task = Some(task);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        match inner.state {
            EngineState::Stopped => bail!("engine is not running"),
            EngineState::Stopping => Ok(()),
            _ => {
                if let Some(tx) = &inner.shutdown_tx {
                    let _ = tx.send(true);
                }
                inner.state = EngineState::Stopping;
                Ok(())
            }
        }
    }

    pub async fn wait_stopped(&self, timeout: std::time::Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.state() == EngineState::Stopped {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    pub async fn control(&self, c: Control) -> Result<()> {
        let tx = {
            let inner = self.inner.lock().unwrap();
            inner.control_tx.clone()
        };
        match tx {
            Some(tx) => tx
                .send(c)
                .await
                .map_err(|_| anyhow::anyhow!("engine is not accepting commands")),
            None => bail!("engine is not running"),
        }
    }

    pub fn stop_file_path(&self) -> PathBuf {
        let name = self
            .load_config()
            .map(|c| c.stop_file)
            .unwrap_or_else(|_| "STOP".into());
        let p = PathBuf::from(name);
        if p.is_absolute() {
            p
        } else {
            self.cfg_path.parent().map(|d| d.join(&p)).unwrap_or(p)
        }
    }

    pub fn set_stop_file(&self, present: bool) -> Result<()> {
        let p = self.stop_file_path();
        if present {
            std::fs::write(&p, b"stopped from UI\n")?;
        } else if p.exists() {
            std::fs::remove_file(&p)?;
        }
        Ok(())
    }

    pub fn config_summary(&self) -> ConfigSummary {
        match self.load_config() {
            Ok(c) => ConfigSummary {
                path: self.cfg_path.display().to_string(),
                live_in_config: c.live,
                live_env_set: losses_accepted(&c),
                min_edge_bps: c.strategy.min_edge_bps.to_string(),
                max_notional_quote: c.strategy.max_notional_quote.to_string(),
                max_daily_loss_quote: c.risk.max_daily_loss_quote.to_string(),
                pairs: c.pairs.iter().map(|p| p.pair.clone()).collect(),
                exchanges: c
                    .enabled_exchanges()
                    .iter()
                    .map(|e| e.to_string())
                    .collect(),
                stop_file: c.stop_file,
                config_error: None,
            },
            Err(e) => ConfigSummary {
                path: self.cfg_path.display().to_string(),
                live_in_config: false,
                live_env_set: crate::runtime::live_env_set(),
                min_edge_bps: String::new(),
                max_notional_quote: String::new(),
                max_daily_loss_quote: String::new(),
                pairs: vec![],
                exchanges: vec![],
                stop_file: "STOP".into(),
                config_error: Some(format!("{e:#}")),
            },
        }
    }

    pub fn status(&self) -> AgentStatus {
        let inner = self.inner.lock().unwrap();
        AgentStatus {
            agent: self.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            hostname: hostname(),
            uptime_s: self.started.elapsed().as_secs(),
            now_ms: now_ms(),
            engine: EngineInfo {
                state: inner.state,
                mode: inner.mode.map(|m| m.as_str().to_string()),
                since_ms: inner.since_ms,
                last_error: inner.last_error.clone(),
            },
            config: self.config_summary(),
            stop_file_present: self.stop_file_path().exists(),
            snapshot: self.shared.status.read().unwrap().clone(),
            recorder: self.shared.recorder_status(),
            recordings: self.recordings(),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.cfg_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn recordings(&self) -> Vec<RecordingInfo> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.data_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("quotes-") && name.ends_with(".jsonl") {
                    if let Ok(md) = e.metadata() {
                        let modified_ms = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        out.push(RecordingInfo {
                            path: e.path().display().to_string(),
                            bytes: md.len(),
                            modified_ms,
                        });
                    }
                }
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.modified_ms));
        out
    }

    /// Start writing every quote the running engine sees to a new file.
    pub fn start_recording(&self) -> Result<RecorderStatus> {
        if self.state() == EngineState::Stopped {
            bail!("start the engine (paper is fine) before recording");
        }
        let mut guard = self.shared.recorder.lock().unwrap();
        if guard.is_some() {
            bail!("already recording");
        }
        let name = format!("quotes-{}.jsonl", chrono::Utc::now().format("%Y%m%d-%H%M"));
        let rec = Recorder::open(self.data_dir().join(name))?;
        *guard = Some(rec);
        drop(guard);
        Ok(self.shared.recorder_status())
    }

    pub fn stop_recording(&self) -> Result<RecorderStatus> {
        let mut guard = self.shared.recorder.lock().unwrap();
        let Some(mut rec) = guard.take() else {
            bail!("not recording")
        };
        rec.flush();
        Ok(RecorderStatus {
            active: false,
            path: Some(rec.path.display().to_string()),
            started_ms: rec.started_ms,
            count: rec.count,
        })
    }

    /// Rank the pairs in a recording (default: the newest) at the configured
    /// fee floors plus the standard tiers.
    pub fn scan(&self, file: Option<&str>) -> Result<ScanResult> {
        let path = match file {
            Some(f) => PathBuf::from(f),
            None => {
                if let Some(r) = self.shared.recorder.lock().unwrap().as_mut() {
                    r.flush();
                }
                PathBuf::from(
                    self.recordings()
                        .first()
                        .ok_or_else(|| anyhow::anyhow!("no recordings yet; record first"))?
                        .path
                        .clone(),
                )
            }
        };
        let quotes = crate::replay::read_quotes(&path)?;
        if quotes.is_empty() {
            bail!("recording {} is empty", path.display());
        }
        let cfg = self.load_config()?;
        let mut mine = crate::scan::FeeTier::uniform("yours", 10.0, 10.0, 2.0);
        for e in cfg.enabled_exchanges() {
            let f = cfg.fee_floor(e);
            mine.taker_bps
                .insert(e, f.taker_bps.to_string().parse().unwrap_or(10.0));
            mine.maker_bps
                .insert(e, f.maker_bps.to_string().parse().unwrap_or(10.0));
        }
        mine.buffer_bps = cfg
            .strategy
            .slippage_buffer_bps
            .to_string()
            .parse()
            .unwrap_or(2.0);
        let tiers = vec![
            mine,
            crate::scan::FeeTier::volume(),
            crate::scan::FeeTier::uniform("zero", 0.0, 0.0, 0.0),
        ];
        let stats = crate::scan::scan(&quotes, &tiers, 3000);
        let first = quotes.iter().map(|q| q.ts_ms).min().unwrap_or(0);
        let last = quotes.iter().map(|q| q.ts_ms).max().unwrap_or(0);
        Ok(ScanResult {
            path: path.display().to_string(),
            quotes: quotes.len(),
            span_ms: last - first,
            min_edge_bps: cfg.strategy.min_edge_bps.to_string().parse().unwrap_or(5.0),
            stats,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoinsView {
    pub catalog: Vec<crate::coins::CoinInfo>,
    pub selected: Vec<String>,
    pub exchanges: Vec<ExchangeId>,
}

impl Supervisor {
    pub fn coins(&self) -> Result<CoinsView> {
        let cfg = self.load_config()?;
        let selected = cfg
            .pairs
            .iter()
            .filter_map(|p| p.pair.split('/').next().map(|b| b.to_string()))
            .collect();
        Ok(CoinsView {
            catalog: crate::coins::catalog(),
            selected,
            exchanges: cfg.enabled_exchanges(),
        })
    }

    /// Replace the configured pairs with `bases` (e.g. ["BTC", "SOL"]).
    pub fn set_coins(&self, bases: &[String]) -> Result<CoinsView> {
        let cfg = self.load_config()?;
        let text = self.read_config_text()?;
        let new_text = crate::coins::apply_coins(&text, bases, &cfg.enabled_exchanges())?;
        self.write_config_text(&new_text)?;
        self.coins()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanResult {
    pub path: String,
    pub quotes: usize,
    pub span_ms: i64,
    pub min_edge_bps: f64,
    pub stats: Vec<crate::scan::PairStats>,
}

impl Supervisor {
    pub fn events(&self, since: u64, limit: usize) -> (Vec<StampedEvent>, u64) {
        let ring = self.shared.events.lock().unwrap();
        (ring.since(since, limit), ring.latest_seq())
    }

    pub fn read_config_text(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.cfg_path)?)
    }

    /// Validate and write a new config. Takes effect on next engine start.
    pub fn write_config_text(&self, text: &str) -> Result<()> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        let tmp = self.cfg_path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.cfg_path)?;
        Ok(())
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE_TOML;

    fn temp_cfg(live: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "speedcraft-sup-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        let text = if live {
            EXAMPLE_TOML.replace("live = false", "live = true")
        } else {
            EXAMPLE_TOML.to_string()
        };
        std::fs::write(&p, text).unwrap();
        p
    }

    #[tokio::test]
    async fn live_start_requires_phrase_and_gates() {
        let sup = Supervisor::new("t", temp_cfg(false));
        let e = sup.start(Mode::Live, None).unwrap_err().to_string();
        assert!(e.contains("confirmation phrase"));
        let e = sup
            .start(Mode::Live, Some(LIVE_CONFIRM_PHRASE))
            .unwrap_err()
            .to_string();
        assert!(e.contains("live = true in config (false)"), "{e}");
        assert_eq!(sup.state(), EngineState::Stopped);
        let sup = Supervisor::new("t", temp_cfg(true));
        // env var not set in tests → still refused.
        let e = sup
            .start(Mode::Live, Some(LIVE_CONFIRM_PHRASE))
            .unwrap_err()
            .to_string();
        assert!(e.contains(crate::runtime::LIVE_ENV), "{e}");
    }

    #[tokio::test]
    async fn stop_file_toggle_and_config_roundtrip() {
        let sup = Supervisor::new("t", temp_cfg(false));
        assert!(!sup.status().stop_file_present);
        sup.set_stop_file(true).unwrap();
        assert!(sup.status().stop_file_present);
        sup.set_stop_file(false).unwrap();
        assert!(!sup.status().stop_file_present);
        let text = sup.read_config_text().unwrap();
        assert!(sup
            .write_config_text(&text.replace("min_edge_bps = 5", "min_edge_bps = 7"))
            .is_ok());
        assert_eq!(sup.config_summary().min_edge_bps, "7");
        assert!(sup.write_config_text("this is not toml").is_err());
        assert!(sup
            .write_config_text(&text.replace("depth_fraction = 0.5", "depth_fraction = 9"))
            .is_err());
        assert_eq!(
            sup.config_summary().min_edge_bps,
            "7",
            "invalid writes must not replace the file"
        );
    }

    #[tokio::test]
    async fn recording_requires_running_engine_and_scan_needs_a_file() {
        let sup = Supervisor::new("t", temp_cfg(false));
        assert!(sup.start_recording().is_err());
        assert!(sup.stop_recording().is_err());
        assert!(sup
            .scan(None)
            .unwrap_err()
            .to_string()
            .contains("no recordings"));
        assert!(sup.status().recordings.is_empty());
        // A recording written by hand is found and scanned.
        let dir = sup.cfg_path.parent().unwrap().to_path_buf();
        let f = dir.join("quotes-20260101-0000.jsonl");
        std::fs::write(&f, concat!(
            r#"{"ts_ms":1,"exchange":"coinbase","pair":"BTC/USD","bid":"100","bid_qty":"1","ask":"100.1","ask_qty":"1"}"#, "\n",
            r#"{"ts_ms":2,"exchange":"kraken","pair":"BTC/USD","bid":"101","bid_qty":"1","ask":"101.2","ask_qty":"1"}"#, "\n"
        )).unwrap();
        assert_eq!(sup.status().recordings.len(), 1);
        let r = sup.scan(None).unwrap();
        assert_eq!(r.quotes, 2);
        assert_eq!(r.stats[0].pair, "BTC/USD");
        assert!(r.stats[0].arb_net_p95.contains_key("yours"));
    }

    #[tokio::test]
    async fn commands_when_stopped_are_rejected() {
        let sup = Supervisor::new("t", temp_cfg(false));
        assert!(sup.stop().is_err());
        assert!(sup.control(Control::Resume).await.is_err());
        let st = sup.status();
        assert_eq!(st.engine.state, EngineState::Stopped);
        assert_eq!(st.agent, "t");
        assert!(st.config.exchanges.len() >= 2);
    }
}
